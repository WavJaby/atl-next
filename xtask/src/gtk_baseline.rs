// SPDX-License-Identifier: MIT
//! The GTK baseline's symbol source: the deb of `gtk-baseline.toml`, downloaded and verified into the cache and extracted. The names its library exports are the GTK 4.14 set that gtk_imports.rs checks against.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use object::read::elf::ElfFile64;
use object::{Architecture, Endianness, Object, ObjectSymbol};
use serde::Deserialize;

use crate::deps::Deps;
use crate::test_apks;
use atl_layout::Layout;

pub const FILE: &str = "gtk-baseline.toml";
/// The one row: the baseline.
const ROW: &str = "gtk-4-14";
const CACHE_DIR: &str = "gtk-baseline";

/// One row of gtk-baseline.toml; each field is documented in the file's header.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub package: String,
    pub version: String,
    pub arch: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub library: String,
}

/// The baseline row, schema-checked. No download.
pub fn load(root: &Path) -> Result<Row> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text)
}

fn parse(text: &str) -> Result<Row> {
    let mut rows: BTreeMap<String, Row> =
        toml::from_str(text).with_context(|| format!("in {FILE}"))?;
    let found = rows.keys().cloned().collect::<Vec<_>>().join(", ");
    ensure!(
        rows.len() == 1,
        "{FILE}: one row `{ROW}` expected, found: {found}"
    );
    let row = rows
        .remove(ROW)
        .with_context(|| format!("{FILE}: no row `{ROW}` (found: {found})"))?;
    check_fields(&row)?;
    Ok(row)
}

/// The row's own fields: https, the file name the url must end in, the digest's shape, and a library under usr/ named libgtk-4.so.N.
fn check_fields(row: &Row) -> Result<()> {
    let file = format!("{}_{}_{}.deb", row.package, row.version, row.arch);
    ensure!(
        row.url.starts_with("https://"),
        "{FILE} {ROW}: url is not https"
    );
    ensure!(
        row.url.rsplit('/').next() == Some(file.as_str()),
        "{FILE} {ROW}: url must end in {file}"
    );
    ensure!(
        row.sha256.len() == 64
            && row
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{FILE} {ROW}: sha256 is not 64 lowercase hex digits"
    );
    ensure!(row.size > 0, "{FILE} {ROW}: size is 0");
    let library = Path::new(&row.library);
    ensure!(
        library.starts_with("usr")
            && library
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("libgtk-4.so.")),
        "{FILE} {ROW}: library must be usr/.../libgtk-4.so.N"
    );
    Ok(())
}

/// The row's series against `[gtk4] min` in deps.toml: the table names the baseline the gate enforces, so the two must agree.
pub fn check(root: &Path, deps: &Deps) -> Result<()> {
    let row = load(root)?;
    let min = deps
        .get("gtk4")
        .and_then(|dep| dep.min.as_deref())
        .context("deps.toml: [gtk4] has no min")?;
    ensure!(
        series(&row.version) == series(min),
        "{FILE} {ROW}: version {} is not in the series of deps.toml [gtk4] min {min}",
        row.version
    );
    Ok(())
}

/// `4.14.5+ds-0ubuntu0.10` and `4.14.0` -> `4.14`.
fn series(version: &str) -> &str {
    let upstream = version.split(['+', '-']).next().unwrap_or(version);
    match upstream.match_indices('.').nth(1) {
        Some((end, _)) => &upstream[..end],
        None => upstream,
    }
}

/// The names the baseline library exports (defined, global, dynamic): the GTK 4.14 set. The deb is extracted into a fresh directory of the cache; nothing in it is run.
pub fn symbols(root: &Path) -> Result<BTreeSet<String>> {
    let row = load(root)?;
    let deb = deb(root)?;
    let dir = cache_dir(root)?.join(format!("x-{}", row.sha256));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| dir.display().to_string())?;
    }
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let status = Command::new("dpkg-deb")
        .arg("-x")
        .arg(&deb)
        .arg(&dir)
        .status()
        .context("running dpkg-deb -x")?;
    ensure!(status.success(), "dpkg-deb -x {}: {status}", deb.display());
    let lib = dir.join(&row.library);
    let data = std::fs::read(&lib).with_context(|| lib.display().to_string())?;
    let elf = ElfFile64::<Endianness>::parse(&*data)
        .with_context(|| format!("{}: not a 64-bit ELF", lib.display()))?;
    let machine = machine(&row.arch)?;
    ensure!(
        elf.architecture() == machine,
        "{}: ELF machine {:?}, row arch {}",
        lib.display(),
        elf.architecture(),
        row.arch
    );
    let mut names = BTreeSet::new();
    for sym in elf
        .dynamic_symbols()
        .filter(|s| !s.is_undefined() && s.is_global())
    {
        names.insert(sym.name()?.to_owned());
    }
    ensure!(
        !names.is_empty(),
        "{}: no exported dynamic symbols",
        lib.display()
    );
    Ok(names)
}

/// The ELF machine of a Debian arch name, for the arches the tables name.
fn machine(arch: &str) -> Result<Architecture> {
    Ok(match arch {
        "amd64" => Architecture::X86_64,
        "arm64" => Architecture::Aarch64,
        "riscv64" => Architecture::Riscv64,
        other => bail!("{FILE}: arch {other} has no ELF machine mapping"),
    })
}

/// The verified deb: the cached copy if it verifies, else a fresh download (curl, https, size cap) that is verified before it takes its sha256 name.
fn deb(root: &Path) -> Result<PathBuf> {
    let row = load(root)?;
    let dir = cache_dir(root)?;
    let path = dir.join(format!("{}.deb", row.sha256));
    if path.is_file() {
        match verify(&row, &path) {
            Ok(()) => return Ok(path),
            Err(e) => {
                println!("gtk-baseline: cached deb does not verify ({e:#}); downloading it again");
                std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
            }
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let part = dir.join(format!("{}.{}.part", row.sha256, std::process::id()));
    println!("gtk-baseline: downloading {} ({} bytes)", row.url, row.size);
    let result =
        test_apks::curl_pinned(&row.url, row.size, &part).and_then(|()| verify(&row, &part));
    match result {
        Ok(()) => {
            std::fs::rename(&part, &path).with_context(|| path.display().to_string())?;
            Ok(path)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Size and sha256 against the row, then the deb's control data against package, version and arch.
fn verify(row: &Row, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    ensure!(
        bytes.len() as u64 == row.size,
        "{}: {} bytes, row says {}",
        path.display(),
        bytes.len(),
        row.size
    );
    let sha256 = test_apks::sha256_hex(&bytes);
    ensure!(
        sha256 == row.sha256,
        "{}: sha256 {sha256}, row says {}",
        path.display(),
        row.sha256
    );
    let out = Command::new("dpkg-deb")
        .arg("-f")
        .arg(path)
        .args(["Package", "Version", "Architecture"])
        .output()
        .context("running dpkg-deb -f")?;
    ensure!(
        out.status.success(),
        "dpkg-deb -f {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let control = String::from_utf8_lossy(&out.stdout);
    for (field, want) in [
        ("Package", &row.package),
        ("Version", &row.version),
        ("Architecture", &row.arch),
    ] {
        let got = control
            .lines()
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(": "));
        ensure!(
            got == Some(want.as_str()),
            "{}: control {field} is {got:?}, row says {want}",
            path.display()
        );
    }
    Ok(())
}

fn cache_dir(root: &Path) -> Result<PathBuf> {
    Ok(Layout::of(root)?.workspace().join("cache").join(CACHE_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[gtk-4-14]
package = "libgtk-4-1"
version = "4.14.5+ds-0ubuntu0.10"
arch = "riscv64"
url = "https://launchpad.net/ubuntu/+archive/primary/+files/libgtk-4-1_4.14.5+ds-0ubuntu0.10_riscv64.deb"
sha256 = "13ba8cb35de6478e677e09903f4ff5d125857dfa3f241a8ce0a9c0576401e98d"
size = 3531026
library = "usr/lib/riscv64-linux-gnu/libgtk-4.so.1"
"#;

    #[test]
    fn the_pinned_row_parses() {
        let row = parse(VALID).expect("well formed");
        assert_eq!(row.version, "4.14.5+ds-0ubuntu0.10");
        assert_eq!(row.size, 3_531_026);
    }

    #[test]
    fn the_url_must_name_the_deb() {
        // a url of another version's file, with the row's version unchanged
        let other = VALID.replace("libgtk-4-1_4.14.5", "libgtk-4-1_4.18.6");
        assert!(parse(&other).is_err());
    }

    #[test]
    fn the_series_is_major_minor() {
        assert_eq!(series("4.14.5+ds-0ubuntu0.10"), "4.14");
        assert_eq!(series("4.14.0"), "4.14");
        assert_ne!(series("4.18.6"), "4.14");
    }
}
