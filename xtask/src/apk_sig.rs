// SPDX-License-Identifier: MIT
//! The signing certificate of an APK: the first certificate of the first signer of its APK Signature Scheme v2 block (v3 when there is no v2).
//! v1 (JAR) signatures are not read: an APK that has only those has no block and is refused.
//! Layout: the end-of-central-directory record gives the central directory's offset; the signing block ends there with a footer of its size (8 bytes) and the magic `APK Sig Block 42`.
//! The block holds ID-value pairs (v2 = 0x7109871a, v3 = 0xf05368c0). A v2/v3 value is a length-prefixed sequence of signers; a signer starts with its signed data, which holds the digests, then the certificates (DER).
//! Every length is checked: a truncated or hostile file is an error, never a panic.

use anyhow::{Context, Result, bail, ensure};

/// The end-of-central-directory record without its comment.
const EOCD_LEN: usize = 22;
/// The comment length is a 16-bit field.
const MAX_COMMENT: usize = u16::MAX as usize;
const MAGIC: &[u8; 16] = b"APK Sig Block 42";
/// The footer after the pairs: the block size (8 bytes) and the magic.
const FOOTER: usize = 8 + MAGIC.len();
const ID_V2: u32 = 0x7109_871a;
const ID_V3: u32 = 0xf053_68c0;

/// The DER bytes of the signing certificate: the one `apksigner --print-certs` digests as `Signer #1`.
pub fn signing_certificate(apk: &[u8]) -> Result<&[u8]> {
    let pairs = signing_pairs(apk)?;
    let value = match pair_value(pairs, ID_V2)? {
        Some(value) => value,
        None => pair_value(pairs, ID_V3)?
            .context("no APK Signature Scheme v2 or v3 block (a v1-only APK is not read)")?,
    };
    let (signers, _) = field(value)?;
    let (signer, _) = field(signers)?;
    let (signed, _) = field(signer)?;
    let (_digests, rest) = field(signed)?;
    let (certificates, _) = field(rest)?;
    let (cert, _) = field(certificates)?;
    ensure!(!cert.is_empty(), "the first signer's certificate is empty");
    Ok(cert)
}

/// The ID-value pairs of the signing block: the bytes between its leading size and its footer.
fn signing_pairs(apk: &[u8]) -> Result<&[u8]> {
    let eocd = end_of_central_directory(apk)?;
    let cd = u32::from_le_bytes(apk[eocd + 16..eocd + 20].try_into()?) as usize;
    ensure!(cd != 0xffff_ffff, "zip64 central directory: not read");
    ensure!(
        cd >= FOOTER && cd <= eocd,
        "central directory offset {cd} lies outside the zip records"
    );
    let footer = &apk[cd - FOOTER..cd];
    ensure!(
        &footer[8..] == MAGIC,
        "no APK signing block before the central directory (not signed, or v1 only)"
    );
    let size = u64::from_le_bytes(footer[..8].try_into()?);
    let Some(total) = size.checked_add(8).filter(|t| *t <= cd as u64) else {
        bail!("signing block size {size} does not fit before the central directory");
    };
    ensure!(
        size >= FOOTER as u64,
        "signing block size {size} is below its footer"
    );
    let start = cd - total as usize;
    let leading = u64::from_le_bytes(apk[start..start + 8].try_into()?);
    ensure!(
        leading == size,
        "signing block sizes differ: {leading} leading, {size} trailing"
    );
    Ok(&apk[start + 8..cd - FOOTER])
}

/// Offset of the end-of-central-directory record: the signature whose comment length runs to the end of the file.
fn end_of_central_directory(apk: &[u8]) -> Result<usize> {
    ensure!(
        apk.len() >= EOCD_LEN,
        "{} bytes: shorter than an end-of-central-directory record",
        apk.len()
    );
    let last = apk.len() - EOCD_LEN;
    let first = last.saturating_sub(MAX_COMMENT);
    for at in (first..=last).rev() {
        if apk[at..at + 4] == *b"PK\x05\x06" {
            let comment = usize::from(u16::from_le_bytes([apk[at + 20], apk[at + 21]]));
            if at + EOCD_LEN + comment == apk.len() {
                return Ok(at);
            }
        }
    }
    bail!("no end-of-central-directory record: not a zip file")
}

/// The value of the first pair with `id`. A pair is a u64 length covering the id and the value, then the u32 id, then the value.
fn pair_value(mut pairs: &[u8], id: u32) -> Result<Option<&[u8]>> {
    while !pairs.is_empty() {
        ensure!(
            pairs.len() >= 12,
            "truncated ID-value pair in the signing block"
        );
        let len = u64::from_le_bytes(pairs[..8].try_into()?);
        ensure!(
            len >= 4 && len <= (pairs.len() - 8) as u64,
            "ID-value pair of {len} bytes overruns the signing block"
        );
        let len = len as usize;
        if u32::from_le_bytes(pairs[8..12].try_into()?) == id {
            return Ok(Some(&pairs[12..8 + len]));
        }
        pairs = &pairs[8 + len..];
    }
    Ok(None)
}

/// The first u32-LE-length-prefixed field of `buf`, and the bytes after it.
fn field(buf: &[u8]) -> Result<(&[u8], &[u8])> {
    let head = buf.get(..4).context("truncated length field")?;
    let len = u32::from_le_bytes(head.try_into()?) as usize;
    let body = &buf[4..];
    ensure!(
        len <= body.len(),
        "length field of {len} bytes overruns its {} bytes",
        body.len()
    );
    Ok(body.split_at(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CERT: &[u8] = b"ATL test certificate";

    /// A u32-LE-length-prefixed field.
    fn field_bytes(body: &[u8]) -> Vec<u8> {
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(body);
        out
    }

    /// One ID-value pair.
    fn pair(id: u32, value: &[u8]) -> Vec<u8> {
        let mut out = ((value.len() + 4) as u64).to_le_bytes().to_vec();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(value);
        out
    }

    /// A v2 value, or a v3 one (min/max SDK words after the signed data, which the reader skips).
    fn signers_value(cert: &[u8], v3: bool) -> Vec<u8> {
        let digests = field_bytes(&field_bytes(b"digest record"));
        let certificates = field_bytes(&field_bytes(cert));
        let signed = [digests, certificates, field_bytes(b"")].concat();
        let mut signer = field_bytes(&signed);
        if v3 {
            signer.extend_from_slice(&[0; 8]);
        }
        signer.extend_from_slice(&field_bytes(b"signature"));
        signer.extend_from_slice(&field_bytes(b"public key"));
        field_bytes(&field_bytes(&signer))
    }

    /// The zip records after the local entries: a central directory entry and the end record.
    fn zip_tail(mut file: Vec<u8>) -> Vec<u8> {
        let cd = file.len();
        file.extend_from_slice(b"PK\x01\x02 central directory entry");
        let cd_size = (file.len() - cd) as u32;
        file.extend_from_slice(b"PK\x05\x06");
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&[1, 0, 1, 0]);
        file.extend_from_slice(&cd_size.to_le_bytes());
        file.extend_from_slice(&(cd as u32).to_le_bytes());
        file.extend_from_slice(&0u16.to_le_bytes());
        file
    }

    /// A signed-looking file: a local entry, the signing block holding `pairs`, the zip records.
    fn apk_with_block(pairs: &[u8]) -> Vec<u8> {
        let mut file = b"PK\x03\x04 local entry".to_vec();
        let size = (pairs.len() + FOOTER) as u64;
        file.extend_from_slice(&size.to_le_bytes());
        file.extend_from_slice(pairs);
        file.extend_from_slice(&size.to_le_bytes());
        file.extend_from_slice(MAGIC);
        zip_tail(file)
    }

    fn message(err: anyhow::Error) -> String {
        format!("{err:#}")
    }

    #[test]
    fn reads_the_v2_certificate() {
        let apk = apk_with_block(&pair(ID_V2, &signers_value(CERT, false)));
        assert_eq!(signing_certificate(&apk).unwrap(), CERT);
    }

    #[test]
    fn falls_back_to_v3_and_prefers_v2() {
        let v3 = apk_with_block(&pair(ID_V3, &signers_value(b"v3 cert", true)));
        assert_eq!(signing_certificate(&v3).unwrap(), b"v3 cert");
        let both = apk_with_block(
            &[
                pair(ID_V3, &signers_value(b"v3 cert", true)),
                pair(ID_V2, &signers_value(CERT, false)),
            ]
            .concat(),
        );
        assert_eq!(signing_certificate(&both).unwrap(), CERT);
    }

    #[test]
    fn refuses_unsigned_and_v1_only_files() {
        // the central directory directly after 64 bytes of local data: no footer before it
        let unsigned = zip_tail(vec![b'x'; 64]);
        let err = message(signing_certificate(&unsigned).unwrap_err());
        assert!(err.contains("no APK signing block"), "{err}");
        let v1 = apk_with_block(&pair(0x1234_5678, b"not a signature scheme"));
        let err = message(signing_certificate(&v1).unwrap_err());
        assert!(err.contains("no APK Signature Scheme v2 or v3"), "{err}");
        let err = message(signing_certificate(b"abc").unwrap_err());
        assert!(err.contains("shorter"), "{err}");
    }

    #[test]
    fn corrupt_lengths_are_errors() {
        let mut pairs = 1000u64.to_le_bytes().to_vec();
        pairs.extend_from_slice(&ID_V2.to_le_bytes());
        let overrun = apk_with_block(&pairs);
        assert!(message(signing_certificate(&overrun).unwrap_err()).contains("overruns"));
        let short_field = apk_with_block(&pair(ID_V2, &1000u32.to_le_bytes()));
        assert!(message(signing_certificate(&short_field).unwrap_err()).contains("overruns"));
    }
}
