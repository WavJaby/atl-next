// SPDX-License-Identifier: MIT
//! `test-apks.toml`: the test APKs `xtask run-apk <row>` runs, pinned to official builds. A url row's file is downloaded into
//! `<workspace>/cache/test-apks/<sha256>.apk` on first use; a google-play row's files (the base and its splits) are fetched by apkeep
//! with `xtask fetch --test-apks --play`. Every use verifies the cached files (size, sha256, signing certificate): a damaged url-row
//! file is replaced, a damaged Play file is refused by name and kept. Column meanings are in the table's header.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::apk_sig;
use atl_layout::Layout;

pub const FILE: &str = "test-apks.toml";
/// Under `<workspace>/cache`: the verified files, named by their sha256.
const CACHE_DIR: &str = "test-apks";
/// Play token tier 1: the Secret Service item `secret-tool lookup service atl-next item play-aas` prints. Attribute names are `service` and `item`.
const SECRET_TOOL: &str = "secret-tool";
const KEYRING_SERVICE: &str = "atl-next";
const KEYRING_ITEM: &str = "play-aas";
const ATTR_SERVICE: &str = "service";
const ATTR_ITEM: &str = "item";

/// Row key = the name `xtask run-apk` takes.
pub type Table = BTreeMap<String, Row>;

/// A validated row. Its files are the pins verified at every use: one for a url row, the base and every split for a google-play row.
#[derive(Debug, Clone)]
pub struct Row {
    pub package: String,
    pub version_name: String,
    pub version_code: u32,
    pub signer_sha256: String,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub enum Source {
    /// fdroid or github-release: one file, downloaded from `url`.
    Url { url: String, pin: Pin },
    /// google-play: the base and its splits, fetched for `device` by apkeep.
    GooglePlay { device: String, files: Vec<Pin> },
}

/// One file of a row, pinned by size and sha256. `file` is the name it goes by: a url row's last URL segment, a Play row's apkeep file name.
#[derive(Debug, Clone)]
pub struct Pin {
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

impl Row {
    /// The row's files in table order: one for a url row; the base and its splits for a google-play row.
    pub fn pins(&self) -> &[Pin] {
        match &self.source {
            Source::Url { pin, .. } => std::slice::from_ref(pin),
            Source::GooglePlay { files, .. } => files,
        }
    }
}

/// The file's shape before validation. One struct for every source, so a field the row's source does not take is refused by name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    package: String,
    version_name: String,
    version_code: u32,
    signer_sha256: String,
    source: RawSource,
    url: Option<String>,
    sha256: Option<String>,
    size: Option<u64>,
    device: Option<String>,
    files: Option<Vec<RawFile>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawSource {
    Fdroid,
    GithubRelease,
    GooglePlay,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    name: String,
    sha256: String,
    size: u64,
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
    let raw: BTreeMap<String, RawRow> = toml::from_str(text)?;
    raw.into_iter()
        .map(|(name, raw)| -> Result<(String, Row)> {
            let row = check_row(&name, raw).with_context(|| format!("[{name}]"))?;
            Ok((name, row))
        })
        .collect()
}

/// Schema-level invariants. The content fields are checked against the file itself, by `verify_bytes` and `check_identity`.
fn check_row(name: &str, raw: RawRow) -> Result<Row> {
    ensure!(
        is_row_name(name),
        "row names are lowercase letters, digits and `-`"
    );
    ensure!(
        is_package(&raw.package),
        "package `{}` is not a Java package name",
        raw.package
    );
    ensure!(!raw.version_name.trim().is_empty(), "version_name is empty");
    ensure!(raw.version_code > 0, "version_code must be positive");
    ensure!(
        is_sha256(&raw.signer_sha256),
        "signer_sha256 must be 64 lowercase hex digits"
    );
    let source = match raw.source {
        RawSource::GooglePlay => google_play_source(&raw)?,
        kind => url_source(kind, &raw)?,
    };
    Ok(Row {
        package: raw.package,
        version_name: raw.version_name,
        version_code: raw.version_code,
        signer_sha256: raw.signer_sha256,
        source,
    })
}

/// fdroid and github-release: one file, its url and pin.
fn url_source(kind: RawSource, raw: &RawRow) -> Result<Source> {
    ensure!(
        raw.device.is_none() && raw.files.is_none(),
        "device and files apply to source google-play only"
    );
    let url = raw
        .url
        .clone()
        .context("url is required (source fdroid, github-release)")?;
    let sha256 = raw
        .sha256
        .clone()
        .context("sha256 is required (source fdroid, github-release)")?;
    let size = raw
        .size
        .context("size is required (source fdroid, github-release)")?;
    ensure!(is_sha256(&sha256), "sha256 must be 64 lowercase hex digits");
    ensure!(size > 0, "size must be positive");
    if kind == RawSource::Fdroid {
        let want = format!(
            "https://f-droid.org/repo/{}_{}.apk",
            raw.package, raw.version_code
        );
        ensure!(url == want, "url must be {want} (source fdroid)");
    } else {
        // <owner>/<repo>/releases/download/<tag>/<asset>.apk; the tag names the release, not the APK's versionName
        let path = url
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
    let file = url.rsplit('/').next().unwrap_or(&url).to_owned();
    Ok(Source::Url {
        url,
        pin: Pin { file, sha256, size },
    })
}

/// google-play: the base and its splits, each pinned under the name apkeep writes it.
fn google_play_source(raw: &RawRow) -> Result<Source> {
    ensure!(
        raw.url.is_none() && raw.sha256.is_none() && raw.size.is_none(),
        "url, sha256 and size apply to source fdroid and github-release only: a google-play row pins each file under files"
    );
    let device = raw
        .device
        .as_deref()
        .context("device is required (source google-play): apkeep's device profile, e.g. px_9a")?;
    ensure!(
        is_device(device),
        "device `{device}` is not an apkeep device name (lowercase letters, digits and `_`)"
    );
    let listed = raw
        .files
        .as_deref()
        .context("files is required (source google-play): the base and its splits")?;
    ensure!(!listed.is_empty(), "files is empty");
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for f in listed {
        ensure!(
            is_split(&f.name),
            "file name `{}` is not `base` or a split name (letters, digits, `_` and `.`)",
            f.name
        );
        ensure!(
            seen.insert(f.name.as_str()),
            "file `{}` is listed twice",
            f.name
        );
        ensure!(
            is_sha256(&f.sha256),
            "{}: sha256 must be 64 lowercase hex digits",
            f.name
        );
        ensure!(f.size > 0, "{}: size must be positive", f.name);
        files.push(Pin {
            file: apkeep_file_name(&raw.package, &f.name),
            sha256: f.sha256.clone(),
            size: f.size,
        });
    }
    ensure!(
        seen.contains("base"),
        "files must list `base`, the package's own APK"
    );
    Ok(Source::GooglePlay {
        device: device.to_owned(),
        files,
    })
}

fn is_row_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether a `run-apk` argument names a row. A row name holds no `.`, `/` or `\`, so every other argument is a file path: a bare name is never read as a file.
pub fn names_row(arg: &str) -> bool {
    !arg.contains(['.', '/', '\\'])
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

/// `base` (the package's own APK) or a split name: letters, digits, `_` and `.`; no leading, trailing or doubled `.`.
fn is_split(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

/// apkeep's device profile name: lowercase letters, digits and `_`.
fn is_device(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The file apkeep writes a split as: `<package>.apk` for `base`, else `<package>.<split>.apk` (gpapi-6.1.0 src/lib.rs:395,420).
pub fn apkeep_file_name(package: &str, split: &str) -> String {
    if split == "base" {
        format!("{package}.apk")
    } else {
        format!("{package}.{split}.apk")
    }
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

fn cached_path(dir: &Path, pin: &Pin) -> PathBuf {
    dir.join(format!("{}.apk", pin.sha256))
}

/// The row's verified file, which `run-apk` runs. A url row's file is downloaded if missing or damaged. A google-play row's files must already be cached and verified, and
/// `run-apk` runs one APK, so a row of more files is refused once its files all verify.
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
    let dir = cache_dir(root)?;
    let path = match &row.source {
        Source::Url { url, pin } => cached_or_download(&dir, name, row, url, pin)?,
        Source::GooglePlay { files, .. } => {
            let mut paths = Vec::new();
            for pin in files {
                paths.push(verified_play_file(&dir, name, row, pin)?);
            }
            ensure!(
                paths.len() == 1,
                "{name}: run-apk runs one APK, and this row has {} files (base and splits), all verified",
                paths.len()
            );
            paths.pop().context("a google-play row has no files")?
        }
    };
    Ok((row.clone(), path))
}

/// A google-play file's cached copy, verified. Missing or damaged: refused by name and kept, never deleted.
fn verified_play_file(dir: &Path, name: &str, row: &Row, pin: &Pin) -> Result<PathBuf> {
    let path = cached_path(dir, pin);
    ensure!(
        path.is_file(),
        "{name}: {}: not cached; `xtask fetch --test-apks --play --play-email <addr> --execute` fetches it",
        pin.file
    );
    verify_file(name, row, pin, &path).map_err(|e| {
        anyhow!("{e:#}; the copy is kept: `xtask fetch --test-apks --play --play-email <addr> --execute` replaces it")
    })?;
    Ok(path)
}

/// A url row's file: the cached one if it verifies, else a fresh download. A damaged cached copy is replaced.
fn cached_or_download(dir: &Path, name: &str, row: &Row, url: &str, pin: &Pin) -> Result<PathBuf> {
    let path = cached_path(dir, pin);
    if path.is_file() {
        match verify_file(name, row, pin, &path) {
            Ok(()) => return Ok(path),
            Err(e) => {
                println!("test-apks: {name}: cached file refused ({e:#}); downloading it again");
                std::fs::remove_file(&path).with_context(|| path.display().to_string())?;
            }
        }
    }
    download(dir, name, row, url, pin)
}

/// curl fetches the url into a per-process part file; the part is verified, then renamed into place, so the cache never holds an unverified file under a sha256 name.
fn download(dir: &Path, name: &str, row: &Row, url: &str, pin: &Pin) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    let path = cached_path(dir, pin);
    let part = dir.join(format!("{}.{}.part", pin.sha256, std::process::id()));
    println!("test-apks: {name}: downloading {url} ({} bytes)", pin.size);
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
        .arg(pin.size.to_string())
        .arg("--output")
        .arg(&part)
        .arg(url)
        .output()
        .context("running curl (deps.toml row curl)")?;
    let result = if out.status.success() {
        std::fs::read(&part)
            .with_context(|| part.display().to_string())
            .and_then(|bytes| verify_bytes(name, row, pin, &bytes))
    } else {
        Err(anyhow!(
            "{name}: download of {url} failed: {}",
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

fn verify_file(name: &str, row: &Row, pin: &Pin, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    verify_bytes(name, row, pin, &bytes)
}

/// Size, then sha256, then the signing certificate, each against the pin and the row. The first mismatch is the error, and names its file and field.
fn verify_bytes(name: &str, row: &Row, pin: &Pin, bytes: &[u8]) -> Result<()> {
    let label = format!("{name}: {}", pin.file);
    ensure!(
        bytes.len() as u64 == pin.size,
        "{label}: size: {FILE} says {} bytes, the file is {} bytes",
        pin.size,
        bytes.len()
    );
    let sha = sha256_hex(bytes);
    ensure!(
        sha == pin.sha256,
        "{label}: sha256: {FILE} says {}, the file is {sha}",
        pin.sha256
    );
    let cert = apk_sig::signing_certificate(bytes)
        .with_context(|| format!("{label}: signer_sha256: reading the signing certificate"))?;
    let signer = sha256_hex(cert);
    ensure!(
        signer == row.signer_sha256,
        "{label}: signer_sha256: {FILE} says {}, the signing certificate is {signer}",
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

/// The Play account: an address, no whitespace or quotes, and no leading `-` (apkeep would read it as a flag).
fn check_email(email: &str) -> Result<()> {
    ensure!(
        email.contains('@')
            && !email.starts_with('-')
            && !email.contains(char::is_whitespace)
            && !email.contains(['"', '\'', '`']),
        "--play-email `{email}` is not an address (needs `@`; no whitespace, quotes or leading `-`)"
    );
    Ok(())
}

/// Without `--play`, a google-play row is not fetched and not checked: listed as skipped.
fn skipped_without_play(row: &Row, play: Option<&str>) -> bool {
    matches!(row.source, Source::GooglePlay { .. }) && play.is_none()
}

/// `xtask fetch --test-apks`: every row's files verified in the cache. A dry run lists each row's state; `--execute` downloads what is missing or damaged.
/// `play` (`--play --play-email <addr>`) enables the google-play rows; without it they are listed as skipped. Every row is attempted, and any failure fails the command, naming its rows.
pub fn fetch(root: &Path, execute: bool, play: Option<&str>) -> Result<()> {
    if let Some(email) = play {
        check_email(email)?;
    }
    let table = load(root)?;
    let dir = cache_dir(root)?;
    let mut failed = Vec::new();
    for (name, row) in &table {
        if skipped_without_play(row, play) {
            println!(
                "test-apks: {name}: skipped: google-play row (`--play --play-email <addr>` fetches it)"
            );
            continue;
        }
        if let Err(e) = fetch_row(&dir, name, row, execute, play) {
            println!("test-apks: {name}: FAILED: {e:#}");
            failed.push(name.as_str());
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

/// One row: cached and verified, or (with `execute`) its missing or damaged files fetched.
fn fetch_row(dir: &Path, name: &str, row: &Row, execute: bool, play: Option<&str>) -> Result<()> {
    let cached = row.pins().iter().all(|pin| {
        let path = cached_path(dir, pin);
        path.is_file() && verify_file(name, row, pin, &path).is_ok()
    });
    if cached {
        println!("test-apks: {name}: cached and verified");
        return Ok(());
    }
    if !execute {
        match &row.source {
            Source::Url { url, pin } => println!(
                "test-apks: {name}: not cached (or damaged); --execute downloads {url} ({} bytes)",
                pin.size
            ),
            Source::GooglePlay { device, .. } => println!(
                "test-apks: {name}: not cached (or damaged); --execute fetches {} files from Google Play (device {device})",
                row.pins().len()
            ),
        }
        return Ok(());
    }
    match &row.source {
        Source::Url { url, pin } => {
            cached_or_download(dir, name, row, url, pin)?;
            println!("test-apks: {name}: downloaded and verified");
        }
        Source::GooglePlay { device, .. } => {
            let email = play.context("google-play rows need --play --play-email <addr>")?;
            fetch_play(dir, name, row, device, email)?;
            println!(
                "test-apks: {name}: fetched and verified ({} files)",
                row.pins().len()
            );
        }
    }
    Ok(())
}

/// apkeep fetches the row's base and splits into a work dir under the cache. Each file is verified under its apkeep name, then renamed to its
/// sha256 name; the work dir goes either way. The token is resolved here, so a dry run and a cached row never read it.
fn fetch_play(dir: &Path, name: &str, row: &Row, device: &str, email: &str) -> Result<()> {
    let token = play_token()?;
    let work = dir.join(format!("{name}.{}.play", std::process::id()));
    std::fs::create_dir_all(&work).with_context(|| work.display().to_string())?;
    let result = apkeep(&work, &row.package, device, email, &token).and_then(|()| {
        for pin in row.pins() {
            let got = work.join(&pin.file);
            verify_file(name, row, pin, &got)?;
            std::fs::rename(&got, cached_path(dir, pin))
                .with_context(|| format!("{name}: {}: moving it into the cache", pin.file))?;
        }
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// `apkeep -a <package> -d google-play -o device=<device>,split_apk=true -e <email> -t <token> <work>`, run in the work dir.
/// Its output is shown only on failure, with the token replaced.
fn apkeep(work: &Path, package: &str, device: &str, email: &str, token: &Token) -> Result<()> {
    let out = Command::new("apkeep")
        .current_dir(work)
        .args(["-a", package, "-d", "google-play", "-o"])
        .arg(format!("device={device},split_apk=true"))
        .args(["-e", email, "-t", token.as_str()])
        .arg(work)
        .output()
        .context("running apkeep (deps.toml row apkeep)")?;
    if out.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    bail!(
        "apkeep for {package} failed ({}): {}",
        out.status,
        text.replace(token.as_str(), "<redacted>").trim()
    )
}

/// The Google Play token. No Debug, Display or Clone: it is never printed, logged or put in an error.
struct Token(String);

impl Token {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the token came from: tier 1 the Secret Service, tier 2 stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    SecretService,
    Stdin,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Self::SecretService => "tier 1 (Secret Service, secret-tool)",
            Self::Stdin => "tier 2 (stdin)",
        }
    }
}

/// What the Secret Service probe found.
enum Probe {
    /// `secret-tool` is not on PATH.
    Absent,
    /// `secret-tool` ran and printed no secret: no item, a locked keyring, or a failure.
    Empty,
    /// The stored token.
    Found(Token),
}

/// Tier 1 when the probe finds the token. Otherwise tier 2: the token on stdin, read only then.
fn resolve_token(probe: Probe, stdin: impl FnOnce() -> Result<String>) -> Result<(Tier, Token)> {
    match probe {
        Probe::Found(token) => Ok((Tier::SecretService, token)),
        Probe::Absent | Probe::Empty => Ok((Tier::Stdin, stdin_token(&stdin()?)?)),
    }
}

/// The token on stdin: one line, no whitespace inside. Empty or blank input is refused.
fn stdin_token(text: &str) -> Result<Token> {
    let token = text.trim();
    ensure!(
        !token.is_empty(),
        "no token on stdin: the Play token is empty"
    );
    ensure!(
        !token.contains(char::is_whitespace),
        "the token on stdin holds whitespace: expected one line"
    );
    Ok(Token(token.to_owned()))
}

/// `secret-tool lookup`: its stdout is the token, read into memory and never printed. A missing binary is `Absent`; a failure or an empty answer is `Empty`.
fn secret_service_probe() -> Probe {
    let out = Command::new(SECRET_TOOL)
        .args([
            "lookup",
            ATTR_SERVICE,
            KEYRING_SERVICE,
            ATTR_ITEM,
            KEYRING_ITEM,
        ])
        .stderr(Stdio::null())
        .output();
    match out {
        Err(e) if e.kind() == ErrorKind::NotFound => Probe::Absent,
        Ok(out) if out.status.success() => {
            let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if token.is_empty() {
                Probe::Empty
            } else {
                Probe::Found(Token(token))
            }
        }
        _ => Probe::Empty,
    }
}

/// Stdin, read whole. Refused on a terminal: the token is piped in, never typed or echoed.
fn read_stdin() -> Result<String> {
    let stdin = std::io::stdin();
    ensure!(
        !stdin.is_terminal(),
        "stdin is a terminal: pipe the Play token in (scripts/env/play-fetch.ps1 does)"
    );
    let mut text = String::new();
    stdin
        .lock()
        .read_to_string(&mut text)
        .context("reading the Play token from stdin")?;
    Ok(text)
}

/// The Play token and its tier, logged. Resolved on the first Play download only.
fn play_token() -> Result<Token> {
    let (tier, token) = resolve_token(secret_service_probe(), read_stdin)?;
    println!("test-apks: play token from {}", tier.label());
    Ok(token)
}

/// `xtask fetch --test-apks --store-token`: the token on stdin goes into the Secret Service by `secret-tool store`, on that tool's stdin, never in argv.
/// The stored item is looked up again before the command reports success.
pub fn store_token() -> Result<()> {
    let token = stdin_token(&read_stdin()?)?;
    let label = format!("--label={KEYRING_SERVICE} {KEYRING_ITEM}");
    let mut child = Command::new(SECRET_TOOL)
        .args([
            "store",
            label.as_str(),
            ATTR_SERVICE,
            KEYRING_SERVICE,
            ATTR_ITEM,
            KEYRING_ITEM,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running secret-tool (libsecret-tools on Debian, libsecret on Arch); without it the token goes on stdin each run")?;
    child
        .stdin
        .take()
        .context("secret-tool: no stdin pipe")?
        .write_all(token.as_str().as_bytes())?;
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "secret-tool store failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    ensure!(
        matches!(secret_service_probe(), Probe::Found(_)),
        "stored, but `secret-tool lookup` finds no token"
    );
    println!(
        "test-apks: play token stored in the Secret Service ({KEYRING_SERVICE} / {KEYRING_ITEM}), read back"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST FIPS 180-2 example vector: SHA-256("abc").
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

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

    fn split(name: &str, sha256: &str, size: u64) -> String {
        format!("{{ name = \"{name}\", sha256 = \"{sha256}\", size = {size} }}")
    }

    /// A google-play row with the given device line and files array body; the base's digest is ABC's, a 3-byte file.
    fn play_row(device: &str, files: &str) -> String {
        format!(
            "[battle-cats-tw]\npackage = \"jp.co.ponos.battlecatstw\"\nversion_name = \"1.0.0\"\nversion_code = 1\n\
             signer_sha256 = \"{ZEROS}\"\nsource = \"google-play\"\n{device}\nfiles = [{files}]\n"
        )
    }

    #[test]
    fn shipped_table_parses() {
        let table = parse(include_str!("../../test-apks.toml")).expect("test-apks.toml");
        let names: Vec<&str> = table.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "altos-odyssey",
                "battle-cats-tw",
                "fossify-math",
                "fossify-notes",
                "material-catalog"
            ]
        );
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
            version_name = \"1.0\"\n\
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
    fn google_play_schema_refuses_each_bad_field() {
        let dev = "device = \"px_9a\"";
        let base = split("base", ABC, 3);
        assert!(
            parse(&play_row(
                dev,
                &format!("{base}, {}", split("config.en", ABC, 3))
            ))
            .is_ok()
        );
        let url = play_row(dev, &base).replace(
            "source = \"google-play\"\n",
            "source = \"google-play\"\nurl = \"https://f-droid.org/x.apk\"\n",
        );
        assert!(err(&url).contains("apply to source fdroid and github-release only"));
        assert!(err(&play_row("", &base)).contains("device is required"));
        assert!(err(&play_row("device = \"Px 9a\"", &base)).contains("not an apkeep device name"));
        assert!(err(&play_row(dev, "")).contains("files is empty"));
        assert!(
            err(&play_row(dev, &split("config/x", ABC, 3)))
                .contains("is not `base` or a split name")
        );
        assert!(
            err(&play_row(dev, &split("..", ABC, 3))).contains("is not `base` or a split name")
        );
        assert!(err(&play_row(dev, &split("config.en", ABC, 3))).contains("must list `base`"));
        let twice = format!(
            "{base}, {}, {}",
            split("config.en", ABC, 3),
            split("config.en", ABC, 3)
        );
        assert!(err(&play_row(dev, &twice)).contains("listed twice"));
        assert!(err(&play_row(dev, &split("base", "ABC", 3))).contains("sha256 must be"));
        assert!(err(&play_row(dev, &split("base", ABC, 0))).contains("size must be positive"));
        let fdroid_only =
            play_row(dev, &base).replace("source = \"google-play\"", "source = \"fdroid\"");
        assert!(err(&fdroid_only).contains("device and files apply to source google-play only"));
    }

    /// Shipped row names pass the argument rule, paths fail it: the split `run-apk` makes between a row and a file.
    #[test]
    fn arguments_are_rows_or_paths() {
        let table = parse(include_str!("../../test-apks.toml")).expect("test-apks.toml");
        for name in table.keys() {
            assert!(names_row(name), "{name}");
        }
        for path in [
            "./fossify-math",
            "fossify-math.apk",
            "out/app",
            "dir\\app",
            "/abs/app",
        ] {
            assert!(!names_row(path), "{path}");
        }
    }

    #[test]
    fn split_files_are_named_as_apkeep_names_them() {
        assert_eq!(
            apkeep_file_name("jp.co.ponos.battlecatstw", "base"),
            "jp.co.ponos.battlecatstw.apk"
        );
        assert_eq!(
            apkeep_file_name("jp.co.ponos.battlecatstw", "config.en"),
            "jp.co.ponos.battlecatstw.config.en.apk"
        );
    }

    #[test]
    fn google_play_rows_are_skipped_without_play() {
        let table = parse(include_str!("../../test-apks.toml")).expect("test-apks.toml");
        let battle_cats = table.get("battle-cats-tw").expect("row");
        let fossify = table.get("fossify-notes").expect("row");
        assert!(skipped_without_play(battle_cats, None));
        assert!(!skipped_without_play(
            battle_cats,
            Some("gate@example.invalid")
        ));
        assert!(!skipped_without_play(fossify, None));
    }

    #[test]
    fn play_email_must_be_one_plain_address() {
        assert!(check_email("gate@example.invalid").is_ok());
        for bad in [
            "no-at-sign",
            "-x@example.invalid",
            "a b@example.invalid",
            "a\"@example.invalid",
        ] {
            assert!(check_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sha256_matches_the_nist_vector() {
        assert_eq!(sha256_hex(b"abc"), ABC);
    }

    #[test]
    fn stored_token_takes_tier_1_without_reading_stdin() {
        let (tier, token) = resolve_token(Probe::Found(Token("ya29.stored".to_owned())), || {
            panic!("stdin read on tier 1")
        })
        .expect("tier 1");
        assert_eq!(tier, Tier::SecretService);
        assert_eq!(token.as_str(), "ya29.stored");
    }

    #[test]
    fn absent_secret_tool_takes_the_stdin_tier() {
        let (tier, token) =
            resolve_token(Probe::Absent, || Ok("ya29.piped\n".to_owned())).expect("tier 2");
        assert_eq!(tier, Tier::Stdin);
        assert_eq!(token.as_str(), "ya29.piped");
    }

    #[test]
    fn empty_lookup_takes_the_stdin_tier() {
        let (tier, token) =
            resolve_token(Probe::Empty, || Ok("ya29.piped\n".to_owned())).expect("tier 2");
        assert_eq!(tier, Tier::Stdin);
        assert_eq!(token.as_str(), "ya29.piped");
    }

    #[test]
    fn stdin_token_refuses_empty_and_multi_line_input() {
        assert!(stdin_token("").is_err());
        assert!(stdin_token(" \n").is_err());
        assert!(stdin_token("ya29.a b\n").is_err());
        assert_eq!(
            stdin_token("ya29.x\n").expect("one line").as_str(),
            "ya29.x"
        );
    }

    #[test]
    fn verify_names_the_failing_file_and_field() {
        let row = row();
        let pin = Pin {
            file: "jp.co.ponos.battlecatstw.config.en.apk".to_owned(),
            sha256: ZEROS.to_owned(),
            size: 3,
        };
        // "abc" passes size; its sha256 is not the pin's
        let e = format!(
            "{:#}",
            verify_bytes("battle-cats-tw", &row, &pin, b"abc").unwrap_err()
        );
        assert!(
            e.starts_with("battle-cats-tw: jp.co.ponos.battlecatstw.config.en.apk: sha256: "),
            "{e}"
        );
        let pin = Pin {
            sha256: ABC.to_owned(),
            ..pin
        };
        // sha256 passes, and "abc" is no APK: the signing certificate is what fails
        let e = format!(
            "{:#}",
            verify_bytes("battle-cats-tw", &row, &pin, b"abc").unwrap_err()
        );
        assert!(e.contains("signer_sha256"), "{e}");
        let pin = Pin { size: 4, ..pin };
        let e = format!(
            "{:#}",
            verify_bytes("battle-cats-tw", &row, &pin, b"abc").unwrap_err()
        );
        assert!(
            e.starts_with("battle-cats-tw: jp.co.ponos.battlecatstw.config.en.apk: size:"),
            "{e}"
        );
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
