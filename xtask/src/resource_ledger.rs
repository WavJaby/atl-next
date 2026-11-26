// SPDX-License-Identifier: MIT
//! The resource ledger: one machine-wide store of measured inputs that admission decides on (gate_need.rs reads the gate kind, test_run.rs the test kind).
//! One file, `<workspace>/out/pool/resource-ledger.tsv`, one tab-separated line per record:
//! - gate wait: `gate name wait run took_ms stall` (`stall` = host stall share, percent, `-` unknown);
//! - test attempt: `test name <empty> run mem_peak oom cpu_usec wall_ms` (`mem_peak` bytes, `oom` 0/1; an OOM's peak is a lower bound only);
//! - gate row attempt: `gate-run name <empty> run mem_peak oom cpu_usec wall_ms`, the same shape under its own kind (main's gate reader keeps it as another kind; the gate reader here never reads it).
//!
//! A line whose first column is another kind is skipped by the readers and kept as written by a rewrite, so a binary that knows fewer kinds neither fails on nor drops it. A malformed line of a known kind is an error naming its line.
//! Key = (kind, name, wait, run). A gate wait under one key keeps the largest took, so a replay changes nothing; a test attempt keeps its first record (an attempt is measured once). Line order is age (appends are serialized).
//! Retention: per (kind, name, wait), the newest `keep_runs` distinct runs and every record of them. Another kind's lines are not keyed and not cut.
//! Writes take the exclusive flock [`LOCK`] (beside the pool's locks, `build_pool::dir`), reload under it, merge, retain and replace the file by rename (temp in the same dir, fsync, rename, fsync dir).
//! Reads take the shared flock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::build_pool::{self, Demand};
use crate::flock::{Flock, Mode};
use crate::out;

/// The ledger's file name under [`out::POOL`].
pub const LEDGER: &str = "resource-ledger.tsv";
/// The lock file, beside the pool's slot and dir locks (`build_pool::dir`).
pub const LOCK: &str = "resource-ledger.lock";
/// The kind of a gate row's wait samples: `name` = the row, `wait` = the wait's name.
pub const GATE: &str = "gate";
/// The kind of a test binary's attempts: `name` = the binary's identity, `wait` empty, `run` = the attempt.
pub const TEST: &str = "test";
/// The kind of a gate row's attempts: `name` = the row, `wait` empty, `run` = the attempt. Eight columns, so the gate reader (`read`) keeps it as another kind; a 6-column gate line is a wait.
pub const GATE_RUN: &str = "gate-run";

/// (kind, name, wait, run): the key a record is stored under.
type Key = (String, String, String, String);

/// One gate wait of one attempt. `run` is an opaque attempt id (a check run's name, `.rerun` suffixed for a rerun).
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
    fn line(&self) -> String {
        let stall = self.stall.map_or("-".to_owned(), |s| format!("{s:.2}"));
        format!(
            "{}\t{}\t{}\t{}\t{}\t{stall}\n",
            self.kind, self.name, self.wait, self.run, self.took_ms
        )
    }

    fn parse(cols: &[&str]) -> Option<Self> {
        let &[kind, name, wait, run, took, stall] = cols else {
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

/// One attempt of a test binary or a gate row (`kind`), measured once per run id: `mem_peak` bytes (the leaf's peak; a lower bound when `oom`), `cpu_usec`, `wall_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// [`TEST`] or [`GATE_RUN`]
    pub kind: &'static str,
    pub name: String,
    pub run: String,
    pub mem_peak: u64,
    pub oom: bool,
    pub cpu_usec: u64,
    pub wall_ms: u64,
}

impl RunRecord {
    fn line(&self) -> String {
        format!(
            "{}\t{}\t\t{}\t{}\t{}\t{}\t{}\n",
            self.kind,
            self.name,
            self.run,
            self.mem_peak,
            u8::from(self.oom),
            self.cpu_usec,
            self.wall_ms
        )
    }

    fn parse(cols: &[&str]) -> Option<Self> {
        let &[kind, name, wait, run, mem, oom, cpu, wall] = cols else {
            return None;
        };
        let kind = match kind {
            TEST => TEST,
            GATE_RUN => GATE_RUN,
            _ => return None,
        };
        if !wait.is_empty() || name.is_empty() || run.is_empty() {
            return None;
        }
        Some(Self {
            kind,
            name: name.to_owned(),
            run: run.to_owned(),
            mem_peak: mem.parse().ok()?,
            oom: match oom {
                "0" => false,
                "1" => true,
                _ => return None,
            },
            cpu_usec: cpu.parse().ok()?,
            wall_ms: wall.parse().ok()?,
        })
    }
}

/// Bytes per GiB: the pool's memory unit.
pub const GIB: u64 = 1 << 30;
/// A unit's reservation: its largest passed peak plus this percent, rounded up to whole GiB. Initial value; to measure.
const HEADROOM_PERCENT: u64 = 25;

/// A unit's demand from its ledger attempts (test binaries and gate rows alike), oldest first: the largest passed peak plus [`HEADROOM_PERCENT`], whole GiB (at least 1, at most `budget`); cores the largest passed average; the last passed run's wall time. Cold (no passed run), or the last attempt an OOM (an unresolved lower bound): the whole budget, alone.
pub fn demand_of(runs: &[RunRecord], budget: u32, cpu: u32) -> (Demand, u64) {
    let whole = (
        Demand {
            mem_gib: budget,
            cores: f64::from(cpu),
        },
        u64::MAX,
    );
    let passed: Vec<&RunRecord> = runs.iter().filter(|r| !r.oom).collect();
    if passed.is_empty() || runs.last().is_some_and(|r| r.oom) {
        return whole;
    }
    let peak = passed.iter().map(|r| r.mem_peak).max().unwrap_or(0);
    let want = (peak + peak * HEADROOM_PERCENT / 100).div_ceil(GIB).max(1);
    let mem_gib = u32::try_from(want).unwrap_or(u32::MAX).min(budget);
    let cores = passed.iter().map(|r| cores_of(r)).fold(0.0, f64::max);
    let wall_ms = passed.last().map_or(0, |r| r.wall_ms);
    (Demand { mem_gib, cores }, wall_ms)
}

/// A run's average cores: its CPU time over its wall time.
fn cores_of(r: &RunRecord) -> f64 {
    if r.wall_ms == 0 {
        0.0
    } else {
        r.cpu_usec as f64 / (r.wall_ms as f64 * 1000.0)
    }
}

/// One ledger line: a gate wait, a test attempt, or another kind's line kept as written.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Wait(Record),
    Run(RunRecord),
    Other(String),
}

impl Row {
    /// The key a record is stored under; None for another kind's line (not keyed, never merged or cut).
    fn key(&self) -> Option<Key> {
        match self {
            Row::Wait(r) => Some((
                r.kind.clone(),
                r.name.clone(),
                r.wait.clone(),
                r.run.clone(),
            )),
            Row::Run(r) => Some((
                r.kind.to_owned(),
                r.name.clone(),
                String::new(),
                r.run.clone(),
            )),
            Row::Other(_) => None,
        }
    }

    fn line(&self) -> String {
        match self {
            Row::Wait(r) => r.line(),
            Row::Run(r) => r.line(),
            Row::Other(text) => format!("{text}\n"),
        }
    }

    /// None: a malformed line of a known kind (the caller names its line).
    fn parse(line: &str) -> Option<Self> {
        let cols: Vec<&str> = line.split('\t').collect();
        match cols.first().copied() {
            Some(GATE) => Record::parse(&cols).map(Row::Wait),
            Some(TEST | GATE_RUN) => RunRecord::parse(&cols).map(Row::Run),
            _ => Some(Row::Other(line.to_owned())),
        }
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

/// Parses `text` (the file at `path`, for the error); a malformed line of a known kind fails the whole read.
fn parse_text(path: &Path, text: &str) -> Result<Vec<Row>> {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            Row::parse(line).with_context(|| {
                format!("{}:{}: not a ledger line: {line:?}", path.display(), i + 1)
            })
        })
        .collect()
}

/// Every line of the ledger of workspace `ws`, oldest first. No file yet: empty.
fn load(ws: &Path) -> Result<Vec<Row>> {
    let path = ledger_path(ws);
    let text = read_text(&path, &lock_path(ws))?;
    parse_text(&path, &text)
}

/// The gate waits of the ledger of workspace `ws`, oldest first.
pub fn read(ws: &Path) -> Result<Vec<Record>> {
    Ok(load(ws)?
        .into_iter()
        .filter_map(|r| match r {
            Row::Wait(r) => Some(r),
            Row::Run(_) | Row::Other(_) => None,
        })
        .collect())
}

/// The attempts of kind `kind` ([`TEST`] or [`GATE_RUN`]) in the ledger of workspace `ws`, oldest first.
pub fn read_runs(ws: &Path, kind: &str) -> Result<Vec<RunRecord>> {
    Ok(load(ws)?
        .into_iter()
        .filter_map(|r| match r {
            Row::Run(r) if r.kind == kind => Some(r),
            Row::Run(_) | Row::Wait(_) | Row::Other(_) => None,
        })
        .collect())
}

/// Adds gate waits to the ledger of workspace `ws` under the exclusive lock, then keeps each window's newest `keep_runs` runs.
pub fn append(ws: &Path, records: &[Record], keep_runs: usize) -> Result<()> {
    append_rows(
        ws,
        records.iter().cloned().map(Row::Wait).collect(),
        keep_runs,
    )
}

/// [`append`] for test attempts.
pub fn append_runs(ws: &Path, runs: &[RunRecord], keep_runs: usize) -> Result<()> {
    append_rows(ws, runs.iter().cloned().map(Row::Run).collect(), keep_runs)
}

fn append_rows(ws: &Path, rows: Vec<Row>, keep_runs: usize) -> Result<()> {
    let path = ledger_path(ws);
    rewrite(&path, &lock_path(ws), |text| {
        let snapshot = parse_text(&path, text)?;
        Ok(render(&retain(merge(snapshot, &rows), keep_runs)))
    })
}

/// `batch` merged into `snapshot` by key: a new key is appended (age order). A known gate wait keeps its place and the larger took; a known test attempt keeps its first record.
fn merge(snapshot: Vec<Row>, batch: &[Row]) -> Vec<Row> {
    let mut out = snapshot;
    let mut at: BTreeMap<Key, usize> = out
        .iter()
        .enumerate()
        .filter_map(|(i, r)| Some((r.key()?, i)))
        .collect();
    for r in batch {
        let key = r
            .key()
            .expect("a batch row is a gate wait or a test attempt");
        match at.get(&key).copied() {
            Some(i) => {
                if let (Row::Wait(old), Row::Wait(new)) = (&mut out[i], r)
                    && new.took_ms > old.took_ms
                {
                    old.took_ms = new.took_ms;
                    old.stall = new.stall;
                }
            }
            None => {
                at.insert(key, out.len());
                out.push(r.clone());
            }
        }
    }
    out
}

/// `rows` (age order) with each (kind, name, wait) cut to its newest `keep_runs` distinct runs; every row of a kept run stays. Another kind's lines are kept.
fn retain(rows: Vec<Row>, keep_runs: usize) -> Vec<Row> {
    let mut runs: BTreeMap<(String, String, String), BTreeSet<String>> = BTreeMap::new();
    let mut kept: Vec<Row> = rows
        .into_iter()
        .rev()
        .filter(|r| {
            let Some((kind, name, wait, run)) = r.key() else {
                return true;
            };
            let seen = runs.entry((kind, name, wait)).or_default();
            if seen.contains(&run) {
                true
            } else if seen.len() < keep_runs {
                seen.insert(run);
                true
            } else {
                false
            }
        })
        .collect();
    kept.reverse();
    kept
}

/// The file text of `rows`, one line each; another kind's line is written as it was read.
fn render(rows: &[Row]) -> String {
    rows.iter().map(Row::line).collect()
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

    fn run(name: &str, run: &str, mem_peak: u64, wall_ms: u64) -> RunRecord {
        RunRecord {
            kind: TEST,
            name: name.to_owned(),
            run: run.to_owned(),
            mem_peak,
            oom: false,
            cpu_usec: 2_000_000,
            wall_ms,
        }
    }

    #[test]
    fn a_record_line_round_trips_and_an_unknown_stall_is_a_dash() {
        let mut r = rec("r1", "app", "marker", 100);
        let path = Path::new("x.tsv");
        assert_eq!(
            parse_text(path, &r.line()).expect("parse"),
            vec![Row::Wait(r.clone())]
        );
        r.stall = None;
        assert_eq!(r.line(), "gate\tapp\tmarker\tr1\t100\t-\n");
        assert_eq!(
            parse_text(path, &r.line()).expect("parse"),
            vec![Row::Wait(r)]
        );
    }

    /// The same key twice is one record with the largest took; a replay of the batch changes nothing.
    #[test]
    fn a_repeated_key_is_one_record_with_the_largest_took() {
        let snapshot = vec![Row::Wait(rec("r1", "app", "marker", 100))];
        let batch = [
            Row::Wait(rec("r1", "app", "marker", 300)),
            Row::Wait(rec("r1", "app", "marker", 200)),
        ];
        let once = merge(snapshot, &batch);
        assert_eq!(once, vec![Row::Wait(rec("r1", "app", "marker", 300))]);
        assert_eq!(
            merge(once.clone(), &batch),
            once,
            "a replay changes nothing"
        );
    }

    /// 21 runs of `marker` and one of `other`: the newest 20 runs of `marker` stay, the oldest goes; `other` has its own window.
    #[test]
    fn retention_cuts_each_wait_to_its_newest_runs() {
        let mut rows: Vec<Row> = (1..=21)
            .map(|i| Row::Wait(rec(&format!("r{i}"), "app", "marker", i)))
            .collect();
        rows.insert(0, Row::Wait(rec("r1", "app", "other", 5)));
        let kept = retain(rows, 20);
        let runs = |wait: &str| -> Vec<String> {
            kept.iter()
                .filter_map(|r| match r {
                    Row::Wait(r) if r.wait == wait => Some(r.run.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            runs("marker"),
            (2..=21).map(|i| format!("r{i}")).collect::<Vec<_>>()
        );
        assert_eq!(runs("other"), ["r1"]);
    }

    /// 21 attempts of one binary: the newest 20 stay; the gate wait keeps its own window.
    #[test]
    fn run_retention_cuts_each_name_to_its_newest_runs() {
        let mut rows: Vec<Row> = (1..=21)
            .map(|i| Row::Run(run("app", &format!("r{i}"), i, 1_000)))
            .collect();
        rows.insert(0, Row::Wait(rec("r1", "app", "marker", 5)));
        let kept = retain(rows, 20);
        let runs: Vec<String> = kept
            .iter()
            .filter_map(|r| match r {
                Row::Run(r) => Some(r.run.clone()),
                Row::Wait(_) | Row::Other(_) => None,
            })
            .collect();
        assert_eq!(runs, (2..=21).map(|i| format!("r{i}")).collect::<Vec<_>>());
        assert_eq!(kept.len(), 21, "the gate wait stays with the runs");
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

    /// A line of another kind is skipped by both readers and kept as written by a rewrite of either kind; a malformed line of a known kind still fails the read, naming its line.
    #[test]
    fn another_kinds_lines_are_skipped_and_kept() {
        let ws = scratch("mixed");
        fs::create_dir_all(ledger_path(&ws).parent().expect("pool dir")).expect("pool dir");
        let other = "future\tbin\t\tr1\t7\t0\t9\t3";
        fs::write(
            ledger_path(&ws),
            format!("gate\tapp\tmarker\tr1\t100\t0.10\n{other}\n"),
        )
        .expect("ledger");
        assert_eq!(
            read(&ws).expect("read"),
            vec![rec("r1", "app", "marker", 100)]
        );
        assert!(read_runs(&ws, TEST).expect("runs").is_empty());
        append(&ws, &[rec("r2", "app", "marker", 200)], 20).expect("append");
        append_runs(&ws, &[run("bin", "r1", 7, 50)], 20).expect("append runs");
        let text = fs::read_to_string(ledger_path(&ws)).expect("ledger text");
        assert_eq!(
            text,
            format!(
                "gate\tapp\tmarker\tr1\t100\t0.10\n{other}\ngate\tapp\tmarker\tr2\t200\t0.10\n\
                 test\tbin\t\tr1\t7\t0\t2000000\t50\n"
            )
        );
        fs::write(
            ledger_path(&ws),
            format!("{other}\ngate\tapp\tmarker\tr1\toops\t0.10\n"),
        )
        .expect("ledger");
        let msg = format!(
            "{:#}",
            read(&ws).expect_err("line 2 is a malformed gate line")
        );
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

    /// A test attempt is eight columns with an empty wait; `oom` is 0 or 1.
    #[test]
    fn a_run_line_round_trips_with_an_empty_wait() {
        let mut r = run("xtask lib t", "r1", 4_294_967_296, 2_000);
        r.oom = true;
        assert_eq!(
            r.line(),
            "test\txtask lib t\t\tr1\t4294967296\t1\t2000000\t2000\n"
        );
        let path = Path::new("x.tsv");
        assert_eq!(
            parse_text(path, &r.line()).expect("parse"),
            vec![Row::Run(r.clone())]
        );
        assert!(
            parse_text(path, "test\tapp\t\tr1\t1\t2\t3\t4\n").is_err(),
            "oom 2"
        );
        assert!(
            parse_text(path, "test\tapp\t\tr1\t1\t0\t3\n").is_err(),
            "short"
        );
    }

    /// One attempt is measured once: a replay of its run changes nothing, the first record stands.
    #[test]
    fn a_repeated_run_keeps_its_first_record() {
        let first = run("app", "r1", 100, 1_000);
        let replay = run("app", "r1", 999, 9_000);
        let once = merge(vec![Row::Run(first.clone())], &[Row::Run(replay.clone())]);
        assert_eq!(once, vec![Row::Run(first)]);
        assert_eq!(merge(once.clone(), &[Row::Run(replay)]), once);
    }

    /// A gate attempt is kind `gate-run`: eight columns like a test attempt, its own key (the same name and run under `test` is another record). Each reader sees its own kind; the gate waits (`read`) see neither.
    #[test]
    fn a_gate_run_line_is_its_own_kind() {
        let ws = scratch("gate-run");
        let gate = RunRecord {
            kind: GATE_RUN,
            ..run("window-ime", "r1", 1_073_741_824, 3_000)
        };
        append_runs(&ws, &[gate.clone(), run("window-ime", "r1", 9, 9)], 20).expect("append");
        assert_eq!(
            read_runs(&ws, GATE_RUN).expect("gate runs"),
            std::slice::from_ref(&gate)
        );
        assert_eq!(read_runs(&ws, TEST).expect("test runs").len(), 1);
        assert!(read(&ws).expect("gate waits").is_empty());
        assert_eq!(
            gate.line(),
            "gate-run\twindow-ime\t\tr1\t1073741824\t0\t2000000\t3000\n"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate waits and test attempts share the file: each read sees its own kind, and no append drops the other.
    #[test]
    fn waits_and_runs_share_one_file() {
        let ws = scratch("shared");
        append(&ws, &[rec("r1", "app", "marker", 100)], 20).expect("append");
        append_runs(&ws, &[run("bin", "r1", 7, 50)], 20).expect("append runs");
        assert_eq!(
            read(&ws).expect("waits"),
            vec![rec("r1", "app", "marker", 100)]
        );
        assert_eq!(
            read_runs(&ws, TEST).expect("runs"),
            vec![run("bin", "r1", 7, 50)]
        );
        append(&ws, &[rec("r2", "app", "marker", 200)], 20).expect("append");
        assert_eq!(
            read_runs(&ws, TEST).expect("runs").len(),
            1,
            "a gate append keeps the runs"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn a_malformed_run_line_names_its_line() {
        let ws = scratch("malformed-run");
        fs::create_dir_all(ledger_path(&ws).parent().expect("pool dir")).expect("pool dir");
        fs::write(
            ledger_path(&ws),
            "gate\tapp\tmarker\tr1\t100\t0.10\ntest\tbin\t\tr1\tbig\t0\t1\n",
        )
        .expect("ledger");
        let msg = format!(
            "{:#}",
            read_runs(&ws, TEST).expect_err("line 2 is malformed")
        );
        assert!(msg.contains("resource-ledger.tsv:2:"), "{msg}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
