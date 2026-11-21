// SPDX-License-Identifier: MIT
//! Which installed package an install session would reinstall unchanged. The data root keeps each package's code (`apps/<package>/apk`, host_layout.rs) across runs; it is the install record, and
//! the only one: a session whose APKs equal one package's installed APKs byte for byte is that package, already installed. AOSP never re-extracts an installed package's native libraries at boot
//! (PackageManagerService parses /data/app in place; NativeLibraryHelper copies at install only, and even then skips a library whose size, mtime and CRC already match: isFileDifferent), so atld
//! installs such a session in place instead of restaging it into an empty dir, where every library would be copied and fsynced again.
//! Identity is content, not names (session files keep their source names; the installed ones are `base.apk` / `split_<name>.apk`) and not metadata (a copied APK has a new mtime and inode).

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atl_android_paths::valid_package_name;

use crate::host_layout::{Item, package_dir};

/// Compare buffer: two of these live at once, one per file.
const CHUNK: usize = 1 << 20;

/// The installed package under `data_root` whose code dir holds exactly `files` (the session's APKs), each once, by content; None when no package does. Packages are tried in name order.
pub fn unchanged(data_root: &Path, files: &[PathBuf]) -> Result<Option<String>> {
    let Ok(entries) = std::fs::read_dir(data_root.join("apps")) else {
        return Ok(None);
    };
    let mut packages: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| valid_package_name(n))
        .collect();
    packages.sort();
    for package in packages {
        let code = package_dir(data_root, &package, Item::Apk);
        let Some(installed) = apks(&code)? else {
            continue;
        };
        if installed.len() == files.len() && pairs(files, installed)? {
            return Ok(Some(package));
        }
    }
    Ok(None)
}

/// The regular `.apk` files directly in `code`, or None when it is no dir.
fn apks(code: &Path) -> Result<Option<Vec<PathBuf>>> {
    let Ok(entries) = std::fs::read_dir(code) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e?;
        if e.file_type()?.is_file() && e.file_name().to_string_lossy().ends_with(".apk") {
            out.push(e.path());
        }
    }
    Ok(Some(out))
}

/// Whether every file of `files` equals a distinct file of `installed` (same count, checked by the caller).
fn pairs(files: &[PathBuf], mut installed: Vec<PathBuf>) -> Result<bool> {
    for f in files {
        let mut hit = None;
        for (i, candidate) in installed.iter().enumerate() {
            if same_content(f, candidate)? {
                hit = Some(i);
                break;
            }
        }
        match hit {
            Some(i) => {
                installed.swap_remove(i);
            }
            None => return Ok(false),
        }
    }
    Ok(true)
}

/// Byte equality; one inode (a hard link: atld stages by linking) is equal without reading.
fn same_content(a: &Path, b: &Path) -> Result<bool> {
    let ma = std::fs::metadata(a).with_context(|| format!("reading {}", a.display()))?;
    let mb = std::fs::metadata(b).with_context(|| format!("reading {}", b.display()))?;
    if (ma.dev(), ma.ino()) == (mb.dev(), mb.ino()) {
        return Ok(true);
    }
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        let m = read_full(&mut fb, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` unless the file ends first; the count read.
fn read_full(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let n = f.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atl-installed-code-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn put(path: &Path, bytes: &[u8]) -> PathBuf {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, bytes).expect("write");
        path.to_path_buf()
    }

    /// Copies (new inodes, other names) of a package's base + split are that package; the session order need not be the installed one.
    #[test]
    fn copies_under_other_names_are_the_installed_package() {
        let d = scratch("copies");
        put(&d.join("apps/p.q/apk/base.apk"), b"base");
        put(&d.join("apps/p.q/apk/split_config.x86_64.apk"), b"split");
        put(&d.join("apps/p.q/apk/lib/x86_64/liba.so"), b"lib");
        let files = [put(&d.join("src/2319.apk"), b"split"), put(&d.join("src/0d04.apk"), b"base")];
        assert_eq!(unchanged(&d, &files).expect("unchanged").as_deref(), Some("p.q"));
    }

    /// A hard link of the installed base (atld's staging) is equal without reading it.
    #[test]
    fn a_hard_link_is_the_installed_package() {
        let d = scratch("link");
        let base = put(&d.join("apps/p.q/apk/base.apk"), b"base");
        std::fs::create_dir_all(d.join("src")).expect("dir");
        std::fs::hard_link(&base, d.join("src/a.apk")).expect("link");
        assert_eq!(unchanged(&d, &[d.join("src/a.apk")]).expect("unchanged").as_deref(), Some("p.q"));
    }

    /// Break: one byte changed (same size), a split missing, or a split more -> a new install.
    #[test]
    fn a_changed_or_differently_split_session_is_new() {
        let d = scratch("changed");
        put(&d.join("apps/p.q/apk/base.apk"), b"base");
        put(&d.join("apps/p.q/apk/split_a.apk"), b"split");
        let base = put(&d.join("src/base.apk"), b"base");
        let changed = put(&d.join("src/changed.apk"), b"spliT");
        let split = put(&d.join("src/split.apk"), b"split");
        let extra = put(&d.join("src/extra.apk"), b"extra");
        assert_eq!(unchanged(&d, &[base.clone(), changed]).expect("changed"), None);
        assert_eq!(unchanged(&d, std::slice::from_ref(&base)).expect("fewer"), None);
        assert_eq!(unchanged(&d, &[base, split, extra]).expect("more"), None);
    }

    /// Two session files equal to one installed file do not both pair with it.
    #[test]
    fn each_installed_file_pairs_once() {
        let d = scratch("once");
        put(&d.join("apps/p.q/apk/base.apk"), b"same");
        put(&d.join("apps/p.q/apk/split_a.apk"), b"other");
        let files = [put(&d.join("src/a.apk"), b"same"), put(&d.join("src/b.apk"), b"same")];
        assert_eq!(unchanged(&d, &files).expect("once"), None);
    }

    /// No apps dir (a fresh data root), and a package with data but no code -> None.
    #[test]
    fn a_fresh_root_has_nothing_installed() {
        let d = scratch("fresh");
        let f = put(&d.join("src/a.apk"), b"x");
        assert_eq!(unchanged(&d, std::slice::from_ref(&f)).expect("fresh"), None);
        put(&d.join("apps/p.q/data/files/a.apk"), b"x");
        assert_eq!(unchanged(&d, &[f]).expect("no code"), None);
    }
}
