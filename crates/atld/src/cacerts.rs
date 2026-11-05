// SPDX-License-Identifier: MIT
//! The system CA store Conscrypt reads, from the distro's trust bundle: one PEM file per certificate named `<subject_hash_old>.<n>`.
//! Conscrypt's TrustedCertificateStore reads `/apex/com.android.conscrypt/cacerts` when the target SDK >= 34 and that dir is non-empty, else `$ANDROID_ROOT/etc/security/cacerts`. Here the APEX dir does not
//! exist, so every app reads the second; the staged ART root (ANDROID_ROOT) links its `etc/security/cacerts` to `/system/etc/security/cacerts`, written here at every atld start (the runtime root is made fresh).
//! Lookup is by name: Conscrypt hashes a subject with X509_NAME_hash_old and probes `<hash>.0`, `.1`, ... until one is missing, so a hash collision takes the next free suffix.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use md5::{Digest, Md5};

/// The distro bundles, first existing wins: Debian/Ubuntu (and the compat link on Arch, Gentoo), Arch's p11-kit extract, Fedora/RHEL, openSUSE, Alpine.
pub const BUNDLES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// The first of [`BUNDLES`] that exists.
pub fn distro_bundle() -> Result<PathBuf> {
    BUNDLES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .with_context(|| format!("no CA bundle at any of {BUNDLES:?}"))
}

/// One DER certificate of a bundle with its store file name.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub der: Vec<u8>,
}

/// The bundle's certificates (PEM `CERTIFICATE` blocks; other blocks and text between them skipped), deduplicated by DER, named in bundle order: the first with a hash gets `.0`.
pub fn entries(bundle: &str) -> Result<Vec<Entry>> {
    let mut seen = BTreeSet::new();
    let mut next: BTreeMap<u32, u32> = BTreeMap::new();
    let mut out = Vec::new();
    for block in pem_blocks(bundle) {
        let (label, der) = pem_rfc7468::decode_vec(block.as_bytes())
            .map_err(|e| anyhow::anyhow!("PEM block: {e}"))?;
        if label != "CERTIFICATE" || !seen.insert(der.clone()) {
            continue;
        }
        let hash = subject_hash_old(&der)?;
        let n = next.entry(hash).or_default();
        out.push(Entry {
            name: format!("{hash:08x}.{n}"),
            der,
        });
        *n += 1;
    }
    Ok(out)
}

/// The `-----BEGIN ...-----` .. `-----END ...-----` spans of `text`.
fn pem_blocks(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let start = rest.find("-----BEGIN ")?;
        let end_tag = rest[start..].find("-----END ")? + start;
        let end = rest[end_tag + 9..].find("-----")? + end_tag + 9 + 5;
        let block = &rest[start..end];
        rest = &rest[end..];
        Some(block)
    })
}

/// OpenSSL X509_NAME_hash_old of the certificate's subject: MD5 of the subject Name's DER, first 4 bytes little-endian.
/// The DER is the certificate's own bytes (OpenSSL hashes the cached encoding it decoded), not a re-encoding.
pub fn subject_hash_old(cert: &[u8]) -> Result<u32> {
    let md = Md5::digest(subject(cert)?);
    Ok(u32::from_le_bytes([md[0], md[1], md[2], md[3]]))
}

/// The subject Name TLV of an X.509 certificate (RFC 5280 4.1): Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version OPTIONAL, serialNumber, signature, issuer, validity, subject, ...
fn subject(cert: &[u8]) -> Result<&[u8]> {
    const SEQUENCE: u8 = 0x30;
    const VERSION: u8 = 0xa0;
    let (tag, cert_body, _) = tlv(cert)?;
    ensure!(tag == SEQUENCE, "certificate: not a SEQUENCE");
    let (tag, mut tbs, _) = tlv(cert_body)?;
    ensure!(tag == SEQUENCE, "tbsCertificate: not a SEQUENCE");
    if tbs.first() == Some(&VERSION) {
        tbs = tlv(tbs)?.2;
    }
    // serialNumber, signature, issuer, validity
    for _ in 0..4 {
        tbs = tlv(tbs)?.2;
    }
    let (tag, _, rest) = tlv(tbs)?;
    ensure!(tag == SEQUENCE, "subject: not a SEQUENCE");
    Ok(&tbs[..tbs.len() - rest.len()])
}

/// A DER TLV at the start of `b`: (tag, content, rest). Low tag numbers only (every tag on the path to the subject is one).
fn tlv(b: &[u8]) -> Result<(u8, &[u8], &[u8])> {
    let [tag, first, rest @ ..] = b else {
        bail!("DER: truncated header")
    };
    ensure!(tag & 0x1f != 0x1f, "DER: high tag number");
    let (len, rest) = if first & 0x80 == 0 {
        (usize::from(*first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        ensure!(
            (1..=std::mem::size_of::<usize>()).contains(&n) && rest.len() >= n,
            "DER: bad length"
        );
        let len = rest[..n]
            .iter()
            .fold(0usize, |l, &d| (l << 8) | usize::from(d));
        (len, &rest[n..])
    };
    ensure!(rest.len() >= len, "DER: content past the end");
    Ok((*tag, &rest[..len], &rest[len..]))
}

/// Writes the store into `dir` (created): each entry as one PEM `CERTIFICATE` block, the form Conscrypt's reader takes (PEM_read_bio). Returns the entry count.
pub fn write_store(bundle: &Path, dir: &Path) -> Result<usize> {
    let text =
        std::fs::read_to_string(bundle).with_context(|| format!("reading {}", bundle.display()))?;
    let entries = entries(&text).with_context(|| bundle.display().to_string())?;
    std::fs::create_dir_all(dir)?;
    for e in &entries {
        let pem = pem_rfc7468::encode_string("CERTIFICATE", pem_rfc7468::LineEnding::LF, &e.der)
            .map_err(|err| anyhow::anyhow!("PEM encoding: {err}"))?;
        std::fs::write(dir.join(&e.name), pem)?;
    }
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// The host's bundle: the oracle runs on the same certificates.
    fn bundle() -> String {
        std::fs::read_to_string(distro_bundle().expect("a distro CA bundle on the test host"))
            .expect("readable bundle")
    }

    /// `openssl x509 -subject_hash_old -noout` of one DER certificate: the independent oracle (OpenSSL's own X509_NAME_hash_old).
    fn openssl_hash(der: &[u8], scratch: &Path) -> String {
        let f = scratch.join("c.der");
        std::fs::write(&f, der).expect("write");
        let out = Command::new("openssl")
            .args([
                "x509",
                "-inform",
                "DER",
                "-subject_hash_old",
                "-noout",
                "-in",
            ])
            .arg(&f)
            .output()
            .expect("openssl on PATH (deps.toml)");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atld-cacerts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// Every certificate of the host bundle hashes as OpenSSL hashes it.
    #[test]
    fn hashes_match_openssl() {
        let s = scratch("oracle");
        let entries = entries(&bundle()).expect("entries");
        assert!(entries.len() > 50, "only {} certificates", entries.len());
        for e in &entries {
            let ours = e.name.split_once('.').expect("<hash>.<n>").0;
            assert_eq!(ours, openssl_hash(&e.der, &s), "{}", e.name);
        }
    }

    /// Two certificates with one subject (a re-issued root) share a hash: `.0` then `.1`, in bundle order; an exact duplicate is dropped.
    #[test]
    fn collision_takes_next_suffix() {
        let s = scratch("collision");
        let self_signed = |n: &str| {
            let (key, crt) = (s.join(format!("{n}.key")), s.join(format!("{n}.pem")));
            let ok = Command::new("openssl")
                .args([
                    "req",
                    "-x509",
                    "-newkey",
                    "ec",
                    "-pkeyopt",
                    "ec_paramgen_curve:P-256",
                    "-nodes",
                    "-days",
                    "1",
                ])
                .args(["-subj", "/CN=atl collision"])
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&crt)
                .output()
                .expect("openssl");
            assert!(ok.status.success());
            std::fs::read_to_string(crt).expect("pem")
        };
        let (a, b) = (self_signed("a"), self_signed("b"));
        let es = entries(&format!("{a}{b}{a}")).expect("entries");
        assert_eq!(es.len(), 2);
        let h = openssl_hash(&es[0].der, &s);
        assert_eq!(es[0].name, format!("{h}.0"));
        assert_eq!(es[1].name, format!("{h}.1"));
        assert_ne!(es[0].der, es[1].der);
    }

    /// The written store parses back to the bundle's certificates, byte for byte, one PEM block per file.
    #[test]
    fn store_round_trips() {
        let s = scratch("store");
        let src = s.join("bundle.pem");
        std::fs::write(&src, bundle()).expect("write");
        let dir = s.join("cacerts");
        let n = write_store(&src, &dir).expect("store");
        let want = entries(&bundle()).expect("entries");
        assert_eq!(n, want.len());
        for e in &want {
            let text = std::fs::read_to_string(dir.join(&e.name)).expect("file");
            let (label, der) = pem_rfc7468::decode_vec(text.as_bytes()).expect("PEM");
            assert_eq!(label, "CERTIFICATE");
            assert_eq!(der, e.der);
        }
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), n);
    }

    #[test]
    fn truncated_der_fails() {
        assert!(subject_hash_old(&[0x30, 0x82, 0x01]).is_err());
        assert!(subject_hash_old(&[0x30, 0x03, 0x02, 0x01, 0x00]).is_err());
    }
}
