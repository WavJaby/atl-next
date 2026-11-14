// SPDX-License-Identifier: MIT
//! Libraries stored inside an APK, named `<zip>!/<entry>`: what ART passes for an app installed with extractNativeLibs=false.
//! The guest bionic linker opens such a path itself (linker.cpp `open_library_in_zipfile`: mmaps the entry at its offset); the host side reads the entry only to classify it, and checks bionic's mapping conditions first so a refusal is named instead of the guest's generic "not found".

use std::fs::File;
use std::io::Read;

use zip::{CompressionMethod, ZipArchive};

/// bionic linker.cpp `kZipFileSeparator`; bionic splits at its first occurrence (`strstr`).
const SEPARATOR: &str = "!/";

/// `(zip, entry)` of a `<zip>!/<entry>` path; None for a plain file path.
pub fn split(path: &str) -> Option<(&str, &str)> {
    path.split_once(SEPARATOR)
}

fn archive(zip: &str) -> Result<ZipArchive<File>, String> {
    let f = File::open(zip).map_err(|e| format!("{zip}: {e}"))?;
    ZipArchive::new(f).map_err(|e| format!("{zip}: {e}"))
}

/// The library's bytes: the file, or the zip entry (inflated when compressed: classifying does not need it mappable).
pub fn read(path: &str) -> Result<Vec<u8>, String> {
    let Some((zip, entry)) = split(path) else {
        return std::fs::read(path).map_err(|e| format!("{path}: {e}"));
    };
    let mut a = archive(zip)?;
    let mut f = a
        .by_name(entry)
        .map_err(|e| format!("{zip}: entry {entry}: {e}"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| format!("{zip}: entry {entry}: {e}"))?;
    Ok(buf)
}

/// bionic's conditions for mapping a zip entry (`open_library_in_zipfile`): stored, data at a `page`-aligned offset. Ok for a plain path.
pub fn check_mappable(path: &str, page: u64) -> Result<(), String> {
    let Some((zip, entry)) = split(path) else {
        return Ok(());
    };
    let mut a = archive(zip)?;
    let f = a
        .by_name(entry)
        .map_err(|e| format!("{zip}: entry {entry}: {e}"))?;
    if f.compression() != CompressionMethod::Stored {
        return Err(format!(
            "{zip}: entry {entry} is compressed ({}); bionic maps only stored entries",
            f.compression()
        ));
    }
    let off = f
        .data_start()
        .ok_or_else(|| format!("{zip}: entry {entry}: data offset unknown"))?;
    if off % page != 0 {
        return Err(format!(
            "{zip}: entry {entry} data at offset {off}, not aligned to the {page}-byte page bionic maps"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// 4096: the x86_64 / arm64 base page; bionic requires the entry offset to be a multiple of it.
    const PAGE: u64 = 4096;
    const BODY: &[u8] = b"\x7fELF stand-in body";

    fn apk(name: &str, method: CompressionMethod, align: u16) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("atl-nb-apk-{}-{name}.apk", std::process::id()));
        let mut w = zip::ZipWriter::new(File::create(&p).expect("create"));
        // a leading odd-sized entry so an unaligned lib entry does not land on a page by chance
        w.start_file(
            "AndroidManifest.xml",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .expect("start");
        w.write_all(&[0; 37]).expect("write");
        let o = SimpleFileOptions::default()
            .compression_method(method)
            .with_alignment(align);
        w.start_file("lib/arm64-v8a/libx.so", o).expect("start");
        w.write_all(BODY).expect("write");
        w.finish().expect("finish");
        p
    }

    #[test]
    fn stored_aligned_entry_reads_and_maps() {
        let p = apk("ok", CompressionMethod::Stored, 4096);
        let path = format!("{}!/lib/arm64-v8a/libx.so", p.display());
        assert_eq!(read(&path).expect("read"), BODY);
        check_mappable(&path, PAGE).expect("mappable");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn compressed_entry_is_named() {
        let p = apk("deflate", CompressionMethod::Deflated, 0);
        let path = format!("{}!/lib/arm64-v8a/libx.so", p.display());
        assert_eq!(read(&path).expect("read inflates"), BODY);
        let e = check_mappable(&path, PAGE).expect_err("compressed");
        assert!(e.contains("is compressed"), "{e}");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn unaligned_entry_is_named() {
        let p = apk("unaligned", CompressionMethod::Stored, 0);
        let path = format!("{}!/lib/arm64-v8a/libx.so", p.display());
        let e = check_mappable(&path, PAGE).expect_err("unaligned");
        assert!(e.contains("not aligned"), "{e}");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn plain_path_passes_through() {
        assert_eq!(split("/a/lib/arm64/libx.so"), None);
        assert_eq!(
            split("/a/base.apk!/lib/arm64-v8a/libx.so"),
            Some(("/a/base.apk", "lib/arm64-v8a/libx.so"))
        );
        check_mappable("/nonexistent/libx.so", PAGE).expect("plain path is not checked here");
    }
}
