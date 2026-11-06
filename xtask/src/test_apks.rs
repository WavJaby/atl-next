// SPDX-License-Identifier: MIT
//! `test-apks.toml`: the test APKs `xtask run-apk <row>` runs, pinned to official builds. A row's file lives in
//! `<workspace>/cache/test-apks/<sha256>.apk`: downloaded on first use, verified at every use (size, sha256, signing certificate).
//! A damaged or swapped file is refused and replaced, never run. Column meanings are in the table's header.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::apk_sig;
use atl_layout::Layout;

pub const FILE: &str = "test-apks.toml";
/// Under `<workspace>/cache`: the verified files, named by their sha256.
const CACHE_DIR: &str = "test-apks";

/// Row key = the name `xtask run-apk` takes.
pub type Table = BTreeMap<String, Row>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub package: String,
    pub version_name: String,
    pub version_code: u32,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub signer_sha256: String,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Fdroid,
    GithubRelease,
}

pub fn load(root: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

/// `cargo xtask check`, step tables: the table parses and its invariants hold. Returns its row count.
pub fn check(root: &Path) -> Result<usize> {
    Ok(load(root)?.len())
}

fn parse(text: &str) -> Result<Table> {
    let table: Table = toml::from_str(text)?;
    for (name, row) in &table {
        check_row(name, row).with_context(|| format!("[{name}]"))?;
    }
    Ok(table)
}

/// Schema-level invariants. The content fields are checked against the file itself, by `verify_bytes` and `check_identity`.
fn check_row(name: &str, row: &Row) -> Result<()> {
    ensure!(
        is_row_name(name),
        "row names are lowercase letters, digits and `-`"
    );
    ensure!(
        is_package(&row.package),
        "package `{}` is not a Java package name",
        row.package
    );
    ensure!(!row.version_name.trim().is_empty(), "version_name is empty");
    ensure!(row.version_code > 0, "version_code must be positive");
    ensure!(row.size > 0, "size must be positive");
    ensure!(
        is_sha256(&row.sha256),
        "sha256 must be 64 lowercase hex digits"
    );
    ensure!(
        is_sha256(&row.signer_sha256),
        "signer_sha256 must be 64 lowercase hex digits"
    );
    match row.source {
        Source::Fdroid => {
            let want = format!(
                "https://f-droid.org/repo/{}_{}.apk",
                row.package, row.version_code
            );
            ensure!(row.url == want, "url must be {want} (source fdroid)");
        }
        Source::GithubRelease => {
            // <owner>/<repo>/releases/download/<tag>/<asset>.apk; the tag names the release, not the APK's versionName
            let path = row
                .url
                .strip_prefix("https://github.com/")
                .context("url must start with https://github.com/ (source github-release)")?;
            let parts: Vec<&str> = path.split('/').collect();
            ensure!(
                parts.len() == 6
                    && !parts[0].is_empty()
                    && !parts[1].is_empty()
                    && parts[2] == "releases"
                    && parts[3] == "download"
                    && !parts[4].is_empty()
                    && parts[5].ends_with(".apk"),
                "url must be https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>.apk"
            );
        }
    }
    Ok(())
}

fn is_row_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// At least two dotted segments, each starting with a letter.
fn is_package(s: &str) -> bool {
    let segments: Vec<&str> = s.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|seg| {
            seg.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn cache_dir(root: &Path) -> Result<PathBuf> {
    Ok(Layout::of(root)?.workspace().join("cache").join(CACHE_DIR))
}

fn cached_path(dir: &Path, row: &Row) -> PathBuf {
    dir.join(format!("{}.apk", row.sha256))
}

/// The row's verified file: the cached one if it verifies, else a fresh download.
pub fn ensure(root: &Path, name: &str) -> Result<(Row, PathBuf)> {
    let table = load(root)?;
    let row = table.get(name).with_context(|| {
        format!(
            "no row `{name}` in {FILE} (rows: {})",
            table
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let path = cached_or_download(&cache_dir(root)?, name, row)?;
    Ok((row.clone(), path))
}

fn cached_or_download(dir: &Path, name: &str, row: &Row) -> Result<PathBuf> {
    let path = cached_path(dir, row);
    if path.is_file() {
        match verify_file(name, row, &path) {
            Ok(()) => return Ok(path),
            Err(e) => {
                println!("test-apks: {name}: cached file refused ({e:#}); downloading it again");
                std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
            }
        }
    }
    download(dir, name, row)
}

/// curl fetches the row's URL into a per-process part file; the part is verified, then renamed into place, so the cache never holds an unverified file under a sha256 name.
fn download(dir: &Path, name: &str, row: &Row) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    let path = cached_path(dir, row);
    let part = dir.join(format!("{}.{}.part", row.sha256, std::process::id()));
    println!(
        "test-apks: {name}: downloading {} ({} bytes)",
        row.url, row.size
    );
    let out = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "5",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-filesize",
        ])
        .arg(row.size.to_string())
        .arg("--output")
        .arg(&part)
        .arg(&row.url)
        .output()
        .context("running curl (deps.toml row curl)")?;
    let result = if out.status.success() {
        std::fs::read(&part)
            .with_context(|| part.display().to_string())
            .and_then(|bytes| verify_bytes(name, row, &bytes))
    } else {
        Err(anyhow!(
            "{name}: download of {} failed: {}",
            row.url,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    };
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

fn verify_file(name: &str, row: &Row, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    verify_bytes(name, row, &bytes)
}

/// Size, then sha256, then the signing certificate, each against the row. The first mismatch is the error, and names its field.
fn verify_bytes(name: &str, row: &Row, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() as u64 == row.size,
        "{name}: size: {FILE} says {} bytes, the file is {} bytes",
        row.size,
        bytes.len()
    );
    let sha = sha256_hex(bytes);
    ensure!(
        sha == row.sha256,
        "{name}: sha256: {FILE} says {}, the file is {sha}",
        row.sha256
    );
    let cert = apk_sig::signing_certificate(bytes)
        .with_context(|| format!("{name}: signer_sha256: reading the signing certificate"))?;
    let signer = sha256_hex(cert);
    ensure!(
        signer == row.signer_sha256,
        "{name}: signer_sha256: {FILE} says {}, the signing certificate is {signer}",
        row.signer_sha256
    );
    Ok(())
}

/// The manifest run-apk read must declare the row's identity: a file that passed the hashes but names another package or version is refused.
pub fn check_identity(
    name: &str,
    row: &Row,
    package: &str,
    version_code: i64,
    version_name: Option<&str>,
) -> Result<()> {
    ensure!(
        package == row.package,
        "{name}: package: {FILE} says {}, the manifest says {package}",
        row.package
    );
    ensure!(
        version_code == i64::from(row.version_code),
        "{name}: version_code: {FILE} says {}, the manifest says {version_code}",
        row.version_code
    );
    ensure!(
        version_name == Some(row.version_name.as_str()),
        "{name}: version_name: {FILE} says {}, the manifest says {}",
        row.version_name,
        version_name.unwrap_or("(none)")
    );
    Ok(())
}

/// `xtask fetch --test-apks`: every row's file verified in the cache. A dry run lists each row's state; `--execute` downloads what is missing or damaged.
/// Every row is attempted, and any failure fails the command, naming its rows.
pub fn fetch(root: &Path, execute: bool) -> Result<()> {
    let table = load(root)?;
    let dir = cache_dir(root)?;
    let mut failed = Vec::new();
    for (name, row) in &table {
        let path = cached_path(&dir, row);
        if path.is_file() && verify_file(name, row, &path).is_ok() {
            println!("test-apks: {name}: cached and verified");
        } else if !execute {
            println!(
                "test-apks: {name}: not cached (or damaged); --execute downloads {} ({} bytes)",
                row.url, row.size
            );
        } else {
            match cached_or_download(&dir, name, row) {
                Ok(_) => println!("test-apks: {name}: downloaded and verified"),
                Err(e) => {
                    println!("test-apks: {name}: FAILED: {e:#}");
                    failed.push(name.as_str());
                }
            }
        }
    }
    ensure!(
        failed.is_empty(),
        "{} test APK row(s) failed: {}",
        failed.len(),
        failed.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST FIPS 180-2 example vector: SHA-256("abc").
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    const VALID: &str = "[fossify-notes]\n\
        package = \"org.fossify.notes\"\n\
        version_name = \"1.7.0\"\n\
        version_code = 13\n\
        url = \"https://f-droid.org/repo/org.fossify.notes_13.apk\"\n\
        sha256 = \"5a56e0e39cc488e1f3b947d3801006d3b7450ec73c67f03195c64c5fd3b6bced\"\n\
        size = 9528948\n\
        signer_sha256 = \"affdb124d3f4720c2f98dbca9eacba0514fba4306e20a2786c861c3c0d6ff292\"\n\
        source = \"fdroid\"\n";

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be refused"))
    }

    fn row() -> Row {
        parse(VALID)
            .expect("valid row")
            .remove("fossify-notes")
            .expect("row")
    }

    #[test]
    fn shipped_table_parses() {
        let table = parse(include_str!("../../test-apks.toml")).expect("test-apks.toml");
        let names: Vec<&str> = table.keys().map(String::as_str).collect();
        assert_eq!(names, ["fossify-math", "fossify-notes", "material-catalog"]);
    }

    #[test]
    fn refuses_each_bad_field() {
        assert!(
            err(&VALID.replace("source = \"fdroid\"", "source = \"github\""))
                .contains("unknown variant")
        );
        assert!(err(&VALID.replace("size = ", "sizes = ")).contains("unknown field"));
        assert!(err(&VALID.replace("5a56e0", "5A56E0")).contains("sha256 must be"));
        assert!(
            err(&VALID.replace("size = 9528948", "size = 0")).contains("size must be positive")
        );
        assert!(
            err(&VALID.replace("version_code = 13", "version_code = 14")).contains("url must be")
        );
        assert!(
            err(&VALID.replace("org.fossify.notes\"", "Org.Fossify\""))
                .contains("not a Java package name")
        );
        let github = "[material-catalog]\n\
            package = \"io.material.catalog\"\n\
            version_name = \"1.14.0\"\n\
            version_code = 1\n\
            url = \"https://github.com/o/r/releases/download/1.14.1/catalog-debug.apk\"\n\
            sha256 = \"fe8658062885413953798af76e2df85b4d65f9c756f233268dc5e607d7435e09\"\n\
            size = 35799987\n\
            signer_sha256 = \"597d7771ec20993d1c45310913d2d4e4e8de1638b4ba58c5fbfbc071812236f4\"\n\
            source = \"github-release\"\n";
        // the release tag names the release: it need not equal the APK's versionName (1.14.0 tag, versionName 1.0)
        assert!(parse(github).is_ok());
        assert!(
            err(&github
                .replace("/1.14.1/", "/1.14.0/")
                .replace("catalog-debug.apk", "catalog.zip"))
            .contains("url must be")
        );
        assert!(parse(&github.replace("/1.14.1/", "/1.14.0/")).is_ok());
    }

    #[test]
    fn sha256_matches_the_nist_vector() {
        assert_eq!(sha256_hex(b"abc"), ABC);
    }

    #[test]
    fn verify_names_the_failing_field() {
        let mut row = row();
        row.size = 3;
        row.sha256 = ABC.to_owned();
        // "abc" passes size and sha256; it is no APK, so the signing certificate is what fails
        let e = format!(
            "{:#}",
            verify_bytes("fossify-notes", &row, b"abc").unwrap_err()
        );
        assert!(e.contains("signer_sha256"), "{e}");
        row.size = 4;
        let e = format!(
            "{:#}",
            verify_bytes("fossify-notes", &row, b"abc").unwrap_err()
        );
        assert!(e.contains("size:"), "{e}");
        row.size = 3;
        row.sha256 = "0".repeat(64);
        let e = format!(
            "{:#}",
            verify_bytes("fossify-notes", &row, b"abc").unwrap_err()
        );
        assert!(e.contains("sha256:"), "{e}");
    }

    #[test]
    fn identity_names_the_failing_field() {
        let row = row();
        assert!(
            check_identity(
                "fossify-notes",
                &row,
                "org.fossify.notes",
                13,
                Some("1.7.0")
            )
            .is_ok()
        );
        let e = format!(
            "{:#}",
            check_identity("fossify-notes", &row, "org.fossify.math", 13, Some("1.7.0"))
                .unwrap_err()
        );
        assert!(e.contains("package:"), "{e}");
        let e = format!(
            "{:#}",
            check_identity(
                "fossify-notes",
                &row,
                "org.fossify.notes",
                12,
                Some("1.7.0")
            )
            .unwrap_err()
        );
        assert!(e.contains("version_code:"), "{e}");
        let e = format!(
            "{:#}",
            check_identity("fossify-notes", &row, "org.fossify.notes", 13, None).unwrap_err()
        );
        assert!(e.contains("version_name:") && e.contains("(none)"), "{e}");
    }
}
