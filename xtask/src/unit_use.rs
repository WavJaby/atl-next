// SPDX-License-Identifier: MIT
//! The one way xtask runs cargo, and the unit use it records per target tree: the record `slot prune` reads instead of rebuild times.
//!
//! [`run`] is the only spawn point. A `Units::Recorded` run (build, test, clippy) gets `--message-format=json-render-diagnostics` (human diagnostics stay on stderr, JSON messages come on stdout), returns those messages, and records each unit that a `compiler-artifact` or `build-script-executed` message names, fresh ones included. Cargo reports every unit of a build's graph on every build, reused or not, so the last build that reports a unit is its last use. Schema: cargo book, External tools, JSON messages (https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages). `compiler-artifact` carries `package_id`, `filenames`, `executable`, `fresh`; `build-script-executed` carries `package_id`, `out_dir`.
//!
//! `Units::Plain` runs add no flag and record nothing: `cargo metadata`, `fmt`, `deny` and `install --list` take no `--message-format`, and the `cargo xtask` alias would hand it to xtask. The alias's own units (xtask and its dependencies) are compiled before any xtask code runs, so no run records them; `slot prune` judges them by mtime.
//!
//! A unit is named by the 16-hex-digit hash in its paths: `deps/<crate>-<hash>.*`, `examples/<name>-<hash>`, `build/<pkg>-<hash>/`, `.fingerprint/<pkg>-<hash>/`. An uplifted output (`<tree>/<name>`) is a hard link of its hashed file, so its inode names the hash. An output with no hash (a cdylib's `deps/lib<x>.so`) names only its package: every fingerprint dir of that package in the tree is recorded, superseded ones included. That keeps more, never less.
//!
//! The record `<tree>/.atl-unit-use.json` holds `units` (hash -> last use, unix seconds). A unit without an entry has no recorded use: `slot prune` judges it by its fingerprint dir's mtime. Updates take `flock` on `<tree>/.atl-unit-use.lock` and replace the file by rename. An unreadable record is an error for prune; the next recorded build replaces it with an empty record.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, IsTerminal, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON messages on stdout; diagnostics stay rendered on stderr.
const MESSAGE_FORMAT: &str = "--message-format=json-render-diagnostics";
/// Cargo writes a unit hash as `{:016x}`: 16 lowercase hex digits ending the unit's name.
const HASH_HEX_LEN: usize = 16;
/// Chunk size of the stderr copy that shows a live run.
const TEE_CHUNK: usize = 8192;
pub const FINGERPRINT: &str = ".fingerprint";
pub const BUILD: &str = "build";
pub const DEPS: &str = "deps";
pub const EXAMPLES: &str = "examples";
const RECORD: &str = ".atl-unit-use.json";
/// Written beside the record, then renamed over it: a reader sees the old or the new file, never half of one.
const RECORD_TMP: &str = ".atl-unit-use.json.tmp";
const RECORD_LOCK: &str = ".atl-unit-use.lock";

/// `<tree>/.atl-unit-use.json`. An older record may still carry `since`: serde ignores it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Record {
    /// Unit hash -> unix seconds of its last use. A unit with no entry is judged by its fingerprint dir mtime.
    pub units: BTreeMap<String, u64>,
}

/// Unix seconds now.
pub fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}

/// What a cargo run is for: a build whose units are recorded, or a subcommand that takes no `--message-format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Units {
    /// build, test, clippy, run: the run adds the flag, returns the messages, records their units.
    Recorded,
    /// metadata, fmt, deny, `install --list`, the `cargo xtask` alias: no flag, no record.
    Plain,
}

/// What a run shows while it runs. Its output is kept for the caller in every mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    /// stdout and stderr as they arrive, as an inherited child would show them.
    Both,
    /// stdout as it arrives; stderr kept for the caller, who shows it on failure.
    Stdout,
    /// nothing: the caller prints what it keeps.
    Nothing,
}

/// One finished cargo run.
#[derive(Debug)]
pub struct Run {
    pub status: ExitStatus,
    /// stdout without the JSON messages; every line keeps its newline.
    pub stdout: String,
    pub stderr: String,
    /// The JSON messages (objects with a `reason`), in order. Empty for `Units::Plain`.
    pub messages: Vec<Value>,
}

/// Runs `cmd` and returns its [`Run`]. A recorded run records its units whatever the status: a failed build still reports the units it finished. `cmd` may be a wrapper (`timeout -s KILL <secs> cargo ...`): the flag goes before its first `--`.
pub fn run(cmd: &mut Command, units: Units, echo: Echo) -> Result<Run> {
    let mut child_cmd = with_units(cmd, units)?;
    child_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if echo == Echo::Both {
        // stderr is a pipe here: cargo would drop colour and progress without these
        let when = if std::io::stderr().is_terminal() {
            "always"
        } else {
            "never"
        };
        child_cmd
            .env("CARGO_TERM_COLOR", when)
            .env("CARGO_TERM_PROGRESS_WHEN", when);
    }
    let mut child = child_cmd
        .spawn()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    // stderr is drained beside stdout: a full stderr pipe would block cargo
    let stderr_pipe = child.stderr.take().context("cargo stderr is not piped")?;
    let show_stderr = echo == Echo::Both;
    let stderr = thread::spawn(move || drain(stderr_pipe, show_stderr));
    let stdout_pipe = child.stdout.take().context("cargo stdout is not piped")?;
    let mut reader = BufReader::new(stdout_pipe);
    let mut text = String::new();
    let mut messages = Vec::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader
            .read_until(b'\n', &mut raw)
            .context("reading cargo stdout")?
            == 0
        {
            break;
        }
        let line = String::from_utf8_lossy(&raw).into_owned();
        let message = match units {
            Units::Recorded => serde_json::from_str::<Value>(line.trim_end())
                .ok()
                .filter(|m| m.get("reason").is_some()),
            Units::Plain => None,
        };
        match message {
            Some(message) => messages.push(message),
            None => {
                if echo != Echo::Nothing {
                    print!("{line}");
                }
                text.push_str(&line);
            }
        }
    }
    let status = child.wait().context("waiting for cargo")?;
    let stderr = stderr.join().unwrap_or_default();
    if units == Units::Recorded {
        record(&messages, now()?).context("recording cargo unit use")?;
    }
    Ok(Run {
        status,
        stdout: text,
        stderr,
        messages,
    })
}

/// [`run`] for a build whose output shows as it runs: returns the status.
pub fn status(cmd: &mut Command) -> Result<ExitStatus> {
    Ok(run(cmd, Units::Recorded, Echo::Both)?.status)
}

/// [`run`] for a build whose stderr shows only when it fails.
pub fn exec(cmd: &mut Command, what: &str) -> Result<()> {
    let run = run(cmd, Units::Recorded, Echo::Stdout)?;
    ensure!(
        run.status.success(),
        "{what} failed ({}):\n{}",
        run.status,
        run.stderr
    );
    Ok(())
}

/// Reads `from` to its end. With `show`, each chunk also goes to our stderr as it arrives; a failed write stops the showing, never the capture.
fn drain(mut from: impl Read, show: bool) -> String {
    let mut captured = Vec::new();
    let mut chunk = [0u8; TEE_CHUNK];
    let mut showing = show;
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                captured.extend_from_slice(&chunk[..n]);
                if showing {
                    let mut err = std::io::stderr().lock();
                    let written = err.write_all(&chunk[..n]);
                    showing = written.and_then(|()| err.flush()).is_ok();
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&captured).into_owned()
}

/// `cmd` as a new command, with the message format added before its first `--` (clippy's `-- -D warnings` must not receive it). `Recorded` refuses a `--message-format` the caller set: the run owns it.
fn with_units(cmd: &Command, units: Units) -> Result<Command> {
    let mut args: Vec<&OsStr> = cmd.get_args().collect();
    if units == Units::Recorded {
        ensure!(
            !args
                .iter()
                .any(|a| a.to_string_lossy().starts_with("--message-format")),
            "{:?} sets --message-format itself; the recorded run owns it",
            cmd.get_program()
        );
        let at = args
            .iter()
            .position(|a| *a == OsStr::new("--"))
            .unwrap_or(args.len());
        args.insert(at, OsStr::new(MESSAGE_FORMAT));
    }
    let mut out = Command::new(cmd.get_program());
    out.args(args);
    if let Some(dir) = cmd.get_current_dir() {
        out.current_dir(dir);
    }
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => out.env(key, value),
            None => out.env_remove(key),
        };
    }
    Ok(out)
}

/// Records the units `messages` name as used at `now`, in each tree they name.
pub fn record(messages: &[Value], now: u64) -> Result<()> {
    for (tree, uses) in uses_of(messages)? {
        let mut hashes = uses.hashes;
        for package in &uses.packages {
            hashes.extend(fingerprints_of(&tree, package)?);
        }
        update(&tree, |record| {
            for hash in hashes {
                record.units.insert(hash, now);
            }
        })?;
    }
    Ok(())
}

/// Drops the entries of units that `slot prune` removed from `tree`. A tree with no record is left alone.
pub fn forget(tree: &Path, hashes: &[String]) -> Result<()> {
    if load(tree)?.is_none() {
        return Ok(());
    }
    update(tree, |record| {
        for hash in hashes {
            record.units.remove(hash);
        }
    })
}

/// The record of `tree`; None when it has none. An unreadable record is an error.
pub fn load(tree: &Path) -> Result<Option<Record>> {
    let path = tree.join(RECORD);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("unreadable use record {}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Applies `change` to the record of `tree` under its lock. A missing record starts empty; an unreadable one is replaced the same way, with a note.
fn update(tree: &Path, change: impl FnOnce(&mut Record)) -> Result<()> {
    let lock_path = tree.join(RECORD_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    flock(&lock, FlockOperation::LockExclusive)
        .with_context(|| format!("flock {}", lock_path.display()))?;
    let mut record = match load(tree) {
        Ok(Some(record)) => record,
        Ok(None) => Record::default(),
        Err(e) => {
            eprintln!(
                "{e:#}: replaced by an empty record; its units are judged by mtime until used again"
            );
            Record::default()
        }
    };
    change(&mut record);
    let text = serde_json::to_string(&record).context("encoding the use record")?;
    let tmp = tree.join(RECORD_TMP);
    let path = tree.join(RECORD);
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
}

/// What one tree's messages used: unit hashes, and packages whose outputs named no hash.
#[derive(Debug, Default)]
struct Uses {
    hashes: BTreeSet<String>,
    packages: BTreeSet<String>,
}

fn uses_of(messages: &[Value]) -> Result<BTreeMap<PathBuf, Uses>> {
    let mut trees: BTreeMap<PathBuf, Uses> = BTreeMap::new();
    for message in messages {
        let mut paths: Vec<&str> = Vec::new();
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                paths.extend(
                    message["filenames"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                );
                paths.extend(message["executable"].as_str());
            }
            Some("build-script-executed") => paths.extend(message["out_dir"].as_str()),
            _ => continue,
        }
        let package = message["package_id"].as_str().and_then(package_name);
        for path in paths {
            let Some((tree, hash)) = locate(Path::new(path)) else {
                continue;
            };
            let hash = match hash {
                Some(hash) => Some(hash),
                None => linked_hash(Path::new(path), &tree)?,
            };
            let uses = trees.entry(tree).or_default();
            match (hash, &package) {
                (Some(hash), _) => {
                    uses.hashes.insert(hash);
                }
                (None, Some(package)) => {
                    uses.packages.insert(package.clone());
                }
                (None, None) => {}
            }
        }
    }
    Ok(trees)
}

/// The tree an output path belongs to, and the hash its own name carries. None hash: an uplifted output (`<tree>/<name>`, or `<tree>/examples/<name>`), whose hash its inode gives.
fn locate(path: &Path) -> Option<(PathBuf, Option<String>)> {
    let parent = path.parent()?;
    let dir = file_name(parent)?;
    if dir == DEPS || dir == EXAMPLES {
        return Some((parent.parent()?.to_owned(), output_hash(&file_name(path)?)));
    }
    if let Some(hash) = unit_hash(&dir) {
        // build/<pkg>-<hash>/<file>: the build-script output of a unit
        let owner = parent.parent()?;
        if file_name(owner)? == BUILD {
            return Some((owner.parent()?.to_owned(), Some(hash.to_owned())));
        }
    }
    Some((parent.to_owned(), None))
}

/// The hash of the hashed file in `tree`'s `deps/` or `examples/` that `path` is a hard link of (cargo's uplift).
fn linked_hash(path: &Path, tree: &Path) -> Result<Option<String>> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let key = (meta.dev(), meta.ino());
    for dir in [DEPS, EXAMPLES] {
        for entry in dir_entries(&tree.join(dir))? {
            let entry_meta = fs::symlink_metadata(&entry)
                .with_context(|| format!("reading {}", entry.display()))?;
            if !entry_meta.is_file() || (entry_meta.dev(), entry_meta.ino()) != key {
                continue;
            }
            if let Some(hash) = file_name(&entry).and_then(|name| output_hash(&name)) {
                return Ok(Some(hash));
            }
        }
    }
    Ok(None)
}

/// The package name in a `package_id`: `<source>#<name>@<version>`, or `<source>#<version>` where the name is the source path's last segment.
fn package_name(package_id: &str) -> Option<String> {
    let (source, rest) = package_id.split_once('#')?;
    match rest.split_once('@') {
        Some((name, _)) => Some(name.to_owned()),
        None => source.rsplit('/').next().map(str::to_owned),
    }
}

/// The hashes of `package`'s fingerprint dirs in `tree`, superseded ones included.
fn fingerprints_of(tree: &Path, package: &str) -> Result<Vec<String>> {
    let mut hashes = Vec::new();
    for dir in dir_entries(&tree.join(FINGERPRINT))? {
        let Some(name) = file_name(&dir) else {
            continue;
        };
        let Some((pkg, hash)) = split_unit(&name) else {
            continue;
        };
        if pkg == package {
            hashes.push(hash.to_owned());
        }
    }
    Ok(hashes)
}

/// The hash of a unit name `<pkg>-<hash>`; None for any other name.
pub fn unit_hash(name: &str) -> Option<&str> {
    split_unit(name).map(|(_, hash)| hash)
}

/// `<pkg>` and `<hash>` of a unit name `<pkg>-<hash>`.
fn split_unit(name: &str) -> Option<(&str, &str)> {
    let (pkg, hash) = name.rsplit_once('-')?;
    let well_formed = !pkg.is_empty()
        && hash.len() == HASH_HEX_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    well_formed.then_some((pkg, hash))
}

/// The hash of an output file name `<crate>-<hash>[.<ext>]`; None for `libfoo.so` and the like.
pub fn output_hash(name: &str) -> Option<String> {
    unit_hash(name.split('.').next()?).map(str::to_owned)
}

/// The entries of `dir`, unsorted. Missing: none.
fn dir_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        paths.push(
            entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path(),
        );
    }
    Ok(paths)
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fixture clock: every record time and message time below is relative to it.
    const NOW: u64 = 1_800_000_000;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("xtask-unit-use-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Writes `text` at `path`, parents included.
    fn file(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, text).expect("file");
    }

    /// The outputs of one debug tree: lib `pdep` (hashed deps files), bin `pbin` and example `ex` (uplifted hard links), cdylib `pcdy` (no hash), and two fingerprint dirs of `pcdy`.
    fn fixture(root: &Path) -> PathBuf {
        let tree = root.join("debug");
        file(&tree.join("deps/libpdep-82c9bf5494a1a958.rlib"), "rlib");
        file(&tree.join("deps/libpdep-82c9bf5494a1a958.rmeta"), "rmeta");
        file(&tree.join("deps/pbin-4d8b50e752b24547"), "bin");
        fs::hard_link(tree.join("deps/pbin-4d8b50e752b24547"), tree.join("pbin"))
            .expect("uplift bin");
        file(&tree.join("examples/ex-6478fe1bccb24b75"), "example");
        fs::hard_link(
            tree.join("examples/ex-6478fe1bccb24b75"),
            tree.join("examples/ex"),
        )
        .expect("uplift example");
        file(&tree.join("deps/libpcdy.so"), "cdylib");
        fs::hard_link(tree.join("deps/libpcdy.so"), tree.join("libpcdy.so"))
            .expect("uplift cdylib");
        fs::create_dir_all(tree.join(FINGERPRINT).join("pcdy-98196d3cfe1a415c")).expect("fp");
        fs::create_dir_all(tree.join(FINGERPRINT).join("pcdy-1111111111111111")).expect("fp");
        tree
    }

    /// The messages of one build of `fixture`, shaped as cargo 1.99.1 prints them (paths under `tree`).
    fn messages(tree: &Path, lib_fresh: bool) -> Vec<Value> {
        let at = |rel: &str| tree.join(rel).display().to_string();
        vec![
            json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/pdep#0.1.0",
                   "filenames": [at("deps/libpdep-82c9bf5494a1a958.rlib"), at("deps/libpdep-82c9bf5494a1a958.rmeta")],
                   "executable": null, "fresh": lib_fresh}),
            json!({"reason": "build-script-executed", "package_id": "path+file:///ws/pdep#0.1.0",
                   "out_dir": at("build/pdep-6870961078c9e1fc/out")}),
            json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/pbin#0.1.0",
                   "filenames": [at("pbin")], "executable": at("pbin"), "fresh": false}),
            json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/pbin#0.1.0",
                   "filenames": [at("examples/ex")], "executable": at("examples/ex"), "fresh": true}),
            json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/pcdy#0.1.0",
                   "filenames": [at("libpcdy.so")], "executable": null, "fresh": true}),
            json!({"reason": "build-finished", "success": true}),
        ]
    }

    /// The unit hashes `fixture` + `messages` record, derived by hand from the names above.
    const FIXTURE_UNITS: [&str; 6] = [
        "1111111111111111",
        "4d8b50e752b24547",
        "6478fe1bccb24b75",
        "6870961078c9e1fc",
        "82c9bf5494a1a958",
        "98196d3cfe1a415c",
    ];

    #[test]
    fn each_output_names_its_unit_and_an_unhashed_one_its_package() {
        let root = scratch("map");
        let tree = fixture(&root);
        record(&messages(&tree, false), NOW).expect("record");
        let record = load(&tree).expect("load").expect("record exists");
        let got: Vec<&str> = record.units.keys().map(String::as_str).collect();
        assert_eq!(
            got, FIXTURE_UNITS,
            "lib deps files, build-script run dir, uplifted bin and example by inode, cdylib by package (both generations)"
        );
        assert!(record.units.values().all(|&at| at == NOW));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    /// A reused unit is a use: cargo reports it with `fresh: true` and no rebuild. Break target: skipping fresh messages fails here.
    #[test]
    fn a_reused_unit_is_recorded() {
        let root = scratch("fresh");
        let tree = fixture(&root);
        let reused = vec![
            json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/pdep#0.1.0",
            "filenames": [tree.join("deps/libpdep-82c9bf5494a1a958.rlib").display().to_string()],
            "executable": null, "fresh": true}),
        ];
        record(&reused, NOW).expect("record");
        let record = load(&tree).expect("load").expect("record exists");
        assert_eq!(record.units.get("82c9bf5494a1a958"), Some(&NOW));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn the_last_use_moves_forward() {
        let root = scratch("merge");
        let tree = fixture(&root);
        record(&messages(&tree, false), NOW).expect("first build");
        record(&messages(&tree, true), NOW + 100).expect("second build");
        let record = load(&tree).expect("load").expect("record exists");
        assert_eq!(record.units.get("82c9bf5494a1a958"), Some(&(NOW + 100)));
        assert_eq!(record.units.get("6478fe1bccb24b75"), Some(&(NOW + 100)));
        assert!(!tree.join(RECORD_TMP).exists(), "temp file left behind");
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn unreadable_record_is_refused_by_load_and_replaced_by_the_next_build() {
        let root = scratch("garbled");
        let tree = fixture(&root);
        file(&tree.join(RECORD), "{not json");
        assert!(load(&tree).is_err(), "garbage read as a record");
        record(&messages(&tree, true), NOW).expect("the build still records");
        let record = load(&tree).expect("load").expect("record exists");
        let got: Vec<&str> = record.units.keys().map(String::as_str).collect();
        assert_eq!(
            got, FIXTURE_UNITS,
            "replacement holds only this build's units"
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    /// A record written before `since` was dropped still reads: serde ignores the field.
    #[test]
    fn a_record_with_since_still_loads() {
        let root = scratch("since");
        let tree = fixture(&root);
        file(
            &tree.join(RECORD),
            r#"{"since":1700000000,"units":{"82c9bf5494a1a958":1800000000}}"#,
        );
        let record = load(&tree).expect("load").expect("record exists");
        assert_eq!(record.units.get("82c9bf5494a1a958"), Some(&NOW));
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn package_name_from_both_package_id_forms() {
        assert_eq!(
            package_name("path+file:///ws/pcdy#0.1.0").as_deref(),
            Some("pcdy")
        );
        assert_eq!(
            package_name("path+file:///ws/dir#pkg-name@0.1.0").as_deref(),
            Some("pkg-name")
        );
        assert_eq!(
            package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.200")
                .as_deref(),
            Some("serde")
        );
    }

    #[test]
    fn output_and_unit_names_carry_sixteen_lowercase_hex_digits() {
        assert_eq!(
            output_hash("libfoo-0123456789abcdef.rlib").as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            output_hash("foo-0123456789abcdef").as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            output_hash("foo-0123456789abcdef.d").as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(output_hash("libcdy.so"), None);
        assert_eq!(unit_hash("x-0123456789ABCDEF"), None);
        assert_eq!(unit_hash("0123456789abcdef"), None);
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn message_format_goes_before_the_first_double_dash() {
        let mut cmd = Command::new("cargo");
        cmd.args(["clippy", "--lib", "--", "-D", "warnings"]);
        let out = with_units(&cmd, Units::Recorded).expect("recorded");
        assert_eq!(
            args_of(&out),
            ["clippy", "--lib", MESSAGE_FORMAT, "--", "-D", "warnings"]
        );
    }

    #[test]
    fn a_wrapped_cargo_gets_the_flag_before_its_double_dash() {
        let mut cmd = Command::new("timeout");
        cmd.args(["-s", "KILL", "600", "cargo", "test", "--", "--nocapture"]);
        let out = with_units(&cmd, Units::Recorded).expect("recorded");
        assert_eq!(out.get_program(), "timeout");
        assert_eq!(
            args_of(&out),
            [
                "-s",
                "KILL",
                "600",
                "cargo",
                "test",
                MESSAGE_FORMAT,
                "--",
                "--nocapture"
            ]
        );
    }

    #[test]
    fn plain_run_adds_no_message_format() {
        let mut cmd = Command::new("cargo");
        cmd.args(["metadata", "--format-version", "1"]);
        let out = with_units(&cmd, Units::Plain).expect("plain");
        assert_eq!(args_of(&out), ["metadata", "--format-version", "1"]);
    }

    #[test]
    fn recorded_run_refuses_a_message_format_it_would_duplicate() {
        let mut cmd = Command::new("cargo");
        cmd.args(["build", "--message-format=short"]);
        let err = with_units(&cmd, Units::Recorded).expect_err("duplicate flag");
        assert!(format!("{err:#}").contains("owns it"), "{err:#}");
    }

    /// Prints the messages of `fixture` as cargo would, one per line, then `plain` as an ordinary line. The script's own arguments land after the sh `--`, so the appended flag is `$0`-style noise it ignores.
    fn shell_run(root: &Path, tree: &Path, units: Units) -> Run {
        let jsonl = root.join("messages.jsonl");
        let text: String = messages(tree, false)
            .iter()
            .map(|m| format!("{m}\n"))
            .collect();
        fs::write(&jsonl, text).expect("messages");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo plain line; cat \"$1\"", "sh"])
            .arg(&jsonl);
        run(&mut cmd, units, Echo::Nothing).expect("run")
    }

    #[test]
    fn runner_returns_messages_records_their_units_and_keeps_plain_lines_apart() {
        let root = scratch("runner");
        let tree = fixture(&root);
        let run = shell_run(&root, &tree, Units::Recorded);
        assert!(run.status.success());
        assert_eq!(run.messages.len(), 6, "every JSON message is returned");
        assert_eq!(run.stdout, "plain line\n", "messages are not stdout text");
        let record = load(&tree).expect("load").expect("record exists");
        let got: Vec<&str> = record.units.keys().map(String::as_str).collect();
        assert_eq!(got, FIXTURE_UNITS);
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn plain_run_returns_text_and_records_nothing() {
        let root = scratch("plain");
        let tree = fixture(&root);
        let run = shell_run(&root, &tree, Units::Plain);
        assert!(run.messages.is_empty());
        assert!(run.stdout.starts_with("plain line\n"));
        assert!(run.stdout.contains("\"reason\":\"compiler-artifact\""));
        assert!(
            load(&tree).expect("load").is_none(),
            "a plain run wrote a record"
        );
        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn exit_status_comes_back_and_the_stderr_is_kept() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo oops >&2; exit 3"]);
        let run = run(&mut cmd, Units::Plain, Echo::Nothing).expect("run");
        assert_eq!(run.status.code(), Some(3));
        assert_eq!(run.stderr, "oops\n");
    }
}
