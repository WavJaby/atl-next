// SPDX-License-Identifier: MIT
//! The resource ledger: one machine-wide store of measured inputs that admission decides on (gate_need.rs reads the gate kind).
//! One file, `<workspace>/out/pool/resource-ledger.tsv`, one line per record: `kind name wait run took_ms stall` (tab-separated; `stall` = host stall share, percent, `-` unknown).
//! Key = (kind, name, wait, run). The same key again is one record with the largest took, so a replay changes nothing. Line order is age (appends are serialized).
//! Retention: per (kind, name, wait), the newest `keep_runs` distinct runs and every record of them.
//! Writes take the exclusive flock [`LOCK`] (beside the pool's locks, `build_pool::dir`), reload under it, merge, retain and replace the file by rename (temp in the same dir, fsync, rename, fsync dir).
//! Reads take the shared flock. A malformed line is an error naming its line: the file is never half-written, so one is corruption.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::build_pool;
use crate::flock::{Flock, Mode};
use crate::out;

/// The ledger's file name under [`out::POOL`].
pub const LEDGER: &str = "resource-ledger.tsv";
/// The lock file, beside the pool's slot and dir locks (`build_pool::dir`).
pub const LOCK: &str = "resource-ledger.lock";
/// The kind of a gate row's wait samples: `name` = the row, `wait` = the wait's name.
pub const GATE: &str = "gate";

/// One ledger line. `run` is an opaque attempt id (a check run's name, `.rerun` suffixed for a rerun).
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub kind: String,
    pub name: String,
    pub wait: String,
    pub run: String,
    pub took_ms: u64,
    /// the attempt's host stall share, percent; None when unknown
    pub stall: Option<f64>,
}

impl Record {
    fn key(&self) -> (String, String, String, String) {
        (
            self.kind.clone(),
            self.name.clone(),
            self.wait.clone(),
            self.run.clone(),
        )
    }

    fn line(&self) -> String {
        let stall = self.stall.map_or("-".to_owned(), |s| format!("{s:.2}"));
        format!(
            "{}\t{}\t{}\t{}\t{}\t{stall}\n",
            self.kind, self.name, self.wait, self.run, self.took_ms
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let cols: Vec<&str> = line.split('\t').collect();
        let &[kind, name, wait, run, took, stall] = cols.as_slice() else {
            return None;
        };
        if kind != GATE || name.is_empty() || wait.is_empty() || run.is_empty() {
            return None;
        }
        let stall = match stall {
            "-" => None,
            s => Some(s.parse::<f64>().ok().filter(|v| v.is_finite())?),
        };
        Some(Self {
            kind: kind.to_owned(),
            name: name.to_owned(),
            wait: wait.to_owned(),
            run: run.to_owned(),
            took_ms: took.parse().ok()?,
            stall,
        })
    }
}

/// The ledger's path under workspace `ws`.
pub fn ledger_path(ws: &Path) -> PathBuf {
    ws.join(out::POOL).join(LEDGER)
}

/// The lock file's path under workspace `ws`.
pub fn lock_path(ws: &Path) -> PathBuf {
    build_pool::dir(ws).join(LOCK)
}

/// Parses `text` (the file at `path`, for the error); any bad line fails the whole read.
fn parse_text(path: &Path, text: &str) -> Result<Vec<Record>> {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            Record::parse(line).with_context(|| {
                format!("{}:{}: not a ledger line: {line:?}", path.display(), i + 1)
            })
        })
        .collect()
}

/// The ledger's records, oldest first. No file yet: empty.
pub fn read(ws: &Path) -> Result<Vec<Record>> {
    let path = ledger_path(ws);
    let text = read_text(&path, &lock_path(ws))?;
    parse_text(&path, &text)
}

/// Adds `records` to the ledger of workspace `ws` under the exclusive lock, then keeps each key's newest `keep_runs` runs.
pub fn append(ws: &Path, records: &[Record], keep_runs: usize) -> Result<()> {
    let path = ledger_path(ws);
    rewrite(&path, &lock_path(ws), |text| {
        let snapshot = parse_text(&path, text)?;
        Ok(render(&retain(merge(snapshot, records), keep_runs)))
    })
}

/// Adds `records` that are older than every record in the ledger (a migration): they go first, in their own order. A key both sides have keeps one record with the larger took. Then each key keeps its newest `keep_runs` runs.
pub fn append_older(ws: &Path, records: &[Record], keep_runs: usize) -> Result<()> {
    let path = ledger_path(ws);
    rewrite(&path, &lock_path(ws), |text| {
        let existing = parse_text(&path, text)?;
        Ok(render(&older_first(records, existing, keep_runs)))
    })
}

/// What [`append_older`] writes over `existing` (age order): `older` deduped and first, then `existing` merged in, then retained. Pure, so a dry run reports the same result.
pub fn older_first(older: &[Record], existing: Vec<Record>, keep_runs: usize) -> Vec<Record> {
    retain(merge(merge(Vec::new(), older), &existing), keep_runs)
}

/// `batch` merged into `snapshot` by key: a new key is appended (age order), a known key keeps its place and the larger took.
pub fn merge(snapshot: Vec<Record>, batch: &[Record]) -> Vec<Record> {
    let mut out = snapshot;
    let mut at: BTreeMap<(String, String, String, String), usize> =
        out.iter().enumerate().map(|(i, r)| (r.key(), i)).collect();
    for r in batch {
        match at.get(&r.key()).copied() {
            Some(i) => {
                if r.took_ms > out[i].took_ms {
                    out[i].took_ms = r.took_ms;
                    out[i].stall = r.stall;
                }
            }
            None => {
                at.insert(r.key(), out.len());
                out.push(r.clone());
            }
        }
    }
    out
}

/// `records` (age order) with each (kind, name, wait) cut to its newest `keep_runs` distinct runs; every record of a kept run stays.
pub fn retain(records: Vec<Record>, keep_runs: usize) -> Vec<Record> {
    let mut runs: BTreeMap<(String, String, String), BTreeSet<String>> = BTreeMap::new();
    let mut kept: Vec<Record> = records
        .into_iter()
        .rev()
        .filter(|r| {
            let seen = runs
                .entry((r.kind.clone(), r.name.clone(), r.wait.clone()))
                .or_default();
            if seen.contains(&r.run) {
                true
            } else if seen.len() < keep_runs {
                seen.insert(r.run.clone());
                true
            } else {
                false
            }
        })
        .collect();
    kept.reverse();
    kept
}

/// The file text of `records`, one line each.
pub fn render(records: &[Record]) -> String {
    records.iter().map(Record::line).collect()
}

/// Takes the lock at `lock_path`: exclusive to rewrite, shared to read. Creates the lock file on first use.
pub fn lock(lock_path: &Path, mode: Mode) -> Result<Flock> {
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| lock_path.display().to_string())?;
    Flock::take(file, mode).with_context(|| format!("flock {}", lock_path.display()))
}

/// `path`'s text under the shared lock; a missing file reads as empty.
pub fn read_text(path: &Path, lock_path: &Path) -> Result<String> {
    let _held = lock(lock_path, Mode::Shared)?;
    read_file(path)
}

/// `path`'s text; a missing file reads as empty. The caller holds the lock.
fn read_file(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Replaces `path` with `edit(its current text)` under the exclusive lock at `lock_path`.
pub fn rewrite(
    path: &Path,
    lock_path: &Path,
    edit: impl FnOnce(&str) -> Result<String>,
) -> Result<()> {
    let _held = lock(lock_path, Mode::Exclusive)?;
    let text = edit(&read_file(path)?)?;
    write_replace(path, &text)
}

/// Writes `text` to a temp file beside `path`, syncs it, renames it over `path`, syncs the dir. A reader with the old file open reads it whole; a killed writer leaves the old file and a temp the next write truncates.
fn write_replace(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("a store file with no dir")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tsv.tmp");
    let mut file = File::create(&tmp).with_context(|| tmp.display().to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("resource-ledger-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn rec(run: &str, name: &str, wait: &str, took_ms: u64) -> Record {
        Record {
            kind: GATE.to_owned(),
            name: name.to_owned(),
            wait: wait.to_owned(),
            run: run.to_owned(),
            took_ms,
            stall: Some(0.10),
        }
    }

    #[test]
    fn a_record_line_round_trips_and_an_unknown_stall_is_a_dash() {
        let mut r = rec("r1", "app", "marker", 100);
        let path = Path::new("x.tsv");
        assert_eq!(parse_text(path, &r.line()).expect("parse"), vec![r.clone()]);
        r.stall = None;
        assert_eq!(r.line(), "gate\tapp\tmarker\tr1\t100\t-\n");
        assert_eq!(parse_text(path, &r.line()).expect("parse"), vec![r]);
    }

    /// The same key twice is one record with the largest took; a replay of the batch changes nothing.
    #[test]
    fn a_repeated_key_is_one_record_with_the_largest_took() {
        let snapshot = vec![rec("r1", "app", "marker", 100)];
        let batch = [
            rec("r1", "app", "marker", 300),
            rec("r1", "app", "marker", 200),
        ];
        let once = merge(snapshot, &batch);
        assert_eq!(once, vec![rec("r1", "app", "marker", 300)]);
        assert_eq!(
            merge(once.clone(), &batch),
            once,
            "a replay changes nothing"
        );
    }

    /// 21 runs of `marker` and one of `other`: the newest 20 runs of `marker` stay, the oldest goes; `other` has its own window.
    #[test]
    fn retention_cuts_each_wait_to_its_newest_runs() {
        let mut records: Vec<Record> = (1..=21)
            .map(|i| rec(&format!("r{i}"), "app", "marker", i))
            .collect();
        records.insert(0, rec("r1", "app", "other", 5));
        let kept = retain(records, 20);
        let runs = |wait: &str| -> Vec<String> {
            kept.iter()
                .filter(|r| r.wait == wait)
                .map(|r| r.run.clone())
                .collect()
        };
        assert_eq!(
            runs("marker"),
            (2..=21).map(|i| format!("r{i}")).collect::<Vec<_>>()
        );
        assert_eq!(runs("other"), ["r1"]);
    }

    /// Eight writers, distinct keys, started together: each key is in the file once after all of them.
    #[test]
    fn concurrent_writers_keep_both_distinct_keys() {
        let ws = scratch("concurrent");
        const WRITERS: usize = 8;
        const BATCHES: usize = 10;
        let start = Barrier::new(WRITERS);
        std::thread::scope(|scope| {
            for w in 0..WRITERS {
                let (ws, start) = (&ws, &start);
                scope.spawn(move || {
                    start.wait();
                    for b in 0..BATCHES {
                        let batch = [rec(&format!("w{w}-{b}"), &format!("row{w}"), "marker", 1)];
                        append(ws, &batch, 1_000).expect("append");
                    }
                });
            }
        });
        let got = read(&ws).expect("read");
        assert_eq!(got.len(), WRITERS * BATCHES, "every distinct key kept");
        let replay = [rec("w0-0", "row0", "marker", 1)];
        append(&ws, &replay, 1_000).expect("replay");
        assert_eq!(
            read(&ws).expect("read").len(),
            WRITERS * BATCHES,
            "a replayed key stays one"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn a_malformed_line_is_an_error_that_names_its_line() {
        let ws = scratch("malformed");
        fs::create_dir_all(ledger_path(&ws).parent().expect("pool dir")).expect("pool dir");
        fs::write(
            ledger_path(&ws),
            "gate\tapp\tmarker\tr1\t100\t0.10\ngate\tapp\tmarker\tr2\toops\t0.10\n",
        )
        .expect("ledger");
        let err = read(&ws).expect_err("line 2 is malformed");
        let msg = format!("{err:#}");
        assert!(msg.contains("resource-ledger.tsv:2:"), "{msg}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A rewrite is a new file renamed into place: a handle on the old one still reads it whole, and no temp stays.
    #[test]
    fn a_rewrite_replaces_the_file_by_rename() {
        let ws = scratch("rename");
        append(&ws, &[rec("r1", "app", "marker", 100)], 20).expect("append");
        let path = ledger_path(&ws);
        let mut old = fs::File::open(&path).expect("open");
        append(&ws, &[rec("r2", "app", "marker", 200)], 20).expect("append");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut old, &mut text).expect("read old");
        assert_eq!(text, "gate\tapp\tmarker\tr1\t100\t0.10\n");
        assert_eq!(read(&ws).expect("read").len(), 2);
        assert!(!path.with_extension("tsv.tmp").exists());
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
