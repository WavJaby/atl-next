// SPDX-License-Identifier: MIT
//! Kept gate logs. A check gate that ends red keeps its log in `<workspace>/out/<checkout id>/check-gates-red/<run>/<gate>.log`; an undecided one in `check-gates-undecided/`. The next check overwrites `check-gates/<gate>.log` in the build dir but never these. `<run>` is the check run's UTC start (`YYYYMMDD-HHMMSS-mmm`), so the names sort oldest first. A rerun's log is `<gate>.rerun.log` (the first attempt's is never overwritten); the outcome ledger `gate-ledger.tsv` sits beside the buckets ([`LEDGER`]).
//! The logs sit beside the checkout's task logs, not in its build dir: a build dir is regenerable (a ram tier shows it empty outside a build; a land reset, refresh or tier change remakes it), and the evidence must outlive it. A path under `out/<id>/` is readable from any shell.
//! `prune` (out.rs `prune_dirs`: `out prune`, and land's cleanup) keeps the newest [`KEEP_RUNS`] runs of each bucket of each live checkout and removes the rest. Undecided has its own bucket: it is routine on a GPU-starved host, and one shared count would evict the red runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;

use crate::build_pool;
use crate::harness::Supply;
use crate::out;
use crate::resource_ledger;
use crate::verdict::Outcome;

/// A red gate's logs: `<checkout's kept dir>/check-gates-red/<run>/`.
pub const RED: &str = "check-gates-red";
/// An undecided gate's logs: `<checkout's kept dir>/check-gates-undecided/<run>/`.
pub const UNDECIDED: &str = "check-gates-undecided";
/// Every bucket.
pub const BUCKETS: [&str; 2] = [RED, UNDECIDED];

/// The directory holding checkout `root`'s kept logs: `<workspace>/out/<checkout id>/`, the owner's per-task log dir (`out_tasks.rs` classes it by checkout id).
pub fn task_dir(root: &Path) -> Result<PathBuf> {
    let ws = Layout::of(root)?.workspace().to_owned();
    Ok(ws.join("out").join(out::checkout_id(root)?))
}

/// Runs kept per bucket per checkout. Basis: a run keeps only its red (or undecided) gates' logs; a gate log is 25 B-21 KB (median 2.4 KB; measured 2026-11-22 on the 30 logs of `ws/land/check-gates`), so a red run is a few tens of KB at most. 50 runs stay near 3 MB per checkout, and ~30 MB with all 30 gates red in every run.
/// Coverage in days = 50 / red checks a day. origin/main gained 41-96 commits a day over 2026-11-08..22 (`git log`); two land reds went unreadable on 2026-11-22; the red rate is not measured yet.
pub const KEEP_RUNS: usize = 50;

/// This check run's name: its start in UTC, to the millisecond.
pub fn run_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    utc_stamp(ms)
}

/// `ms` since the epoch as `YYYYMMDD-HHMMSS-mmm` (UTC). Days to a civil date by H. Hinnant's `civil_from_days`: the constants are the algorithm's (400-year era of 146 097 days, March-based year).
fn utc_stamp(ms: u64) -> String {
    let (secs, millis) = (ms / 1_000, ms % 1_000);
    let (days, of_day) = (secs / 86_400, secs % 86_400);
    let z = i64::try_from(days).unwrap_or(i64::MAX) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}-{millis:03}",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// The bucket a gate's log is kept in, by its outcome; none for a pass, whose log the next check overwrites.
pub fn bucket_of(outcome: Outcome) -> Option<&'static str> {
    match outcome {
        Outcome::Pass => None,
        Outcome::Fail => Some(RED),
        Outcome::Undecided => Some(UNDECIDED),
    }
}

/// Copies `log` (a gate's log, just written) to `<task>/<bucket>/<run>/` (`task` = [`task_dir`]), made on first use; returns the kept path.
pub fn keep(task: &Path, bucket: &str, run: &str, log: &Path) -> Result<PathBuf> {
    let dir = task.join(bucket).join(run);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = log.file_name().context("a gate log with no file name")?;
    let kept = dir.join(name);
    fs::copy(log, &kept)
        .with_context(|| format!("copying {} to {}", log.display(), kept.display()))?;
    Ok(kept)
}

/// The checkout's outcome ledger: one line per gate row per check run, in `<task>/` (`task` = [`task_dir`]). The ceiling reads its trailing undecided lines.
pub const LEDGER: &str = "gate-ledger.tsv";
/// Consecutive undecided runs of one row that fail its checkout's step (a maintainer's choice, not a measurement). A row starved in every run gates nothing: it fails visibly, not silently.
pub const CEILING: usize = 3;

/// One row's ledger line. `first`: the attempt before a rerun (`-` when the row ran once). `stall`: the deciding run's host stall share, percent (`-` unknown). `grant`: the grant it held (`quiet`, or the class name).
/// `supply`: what starved the first attempt (`-` when it was not starved); the deciding run's verdict is in `outcome`.
pub struct Entry<'a> {
    pub run: &'a str,
    pub commit: &'a str,
    pub row: &'a str,
    pub outcome: Outcome,
    pub first: Option<Outcome>,
    pub stall: Option<f64>,
    pub grant: &'a str,
    pub supply: Option<Supply>,
}

/// The ledger's word for an outcome.
fn word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pass => "pass",
        Outcome::Fail => "fail",
        Outcome::Undecided => "undecided",
    }
}

/// Appends one line per entry to checkout `task`'s ledger, made on first use. Columns: run, commit, row, outcome, first, stall, grant, supply ([`COLUMNS`]).
pub fn ledger_append(task: &Path, entries: &[Entry<'_>]) -> Result<()> {
    fs::create_dir_all(task).with_context(|| format!("creating {}", task.display()))?;
    let path = task.join(LEDGER);
    let text: String = entries
        .iter()
        .map(|e| {
            let first = e.first.map_or("-", word);
            let stall = e.stall.map_or("-".to_owned(), |s| format!("{s:.1}"));
            let supply = e.supply.map_or("-", Supply::word);
            format!(
                "{}\t{}\t{}\t{}\t{first}\t{stall}\t{}\t{supply}\n",
                e.run,
                e.commit,
                e.row,
                word(e.outcome),
                e.grant
            )
        })
        .collect();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| path.display().to_string())?;
    file.write_all(text.as_bytes())
        .with_context(|| path.display().to_string())
}

/// A ledger line's columns, as [`ledger_append`] writes them.
const COLUMNS: usize = 8;

/// What [`trailing_undecided`] read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Trailing {
    /// Per row, the run names of its trailing undecided lines, oldest first.
    pub streaks: BTreeMap<String, Vec<String>>,
    /// Lines that are no ledger line (column count or outcome word): not read, so the caller reports them.
    pub malformed: usize,
}

/// The ledger's streaks. A pass or a fail ends the row's streak; other rows' lines do not. No ledger: empty.
pub fn trailing_undecided(task: &Path) -> Result<Trailing> {
    let path = task.join(LEDGER);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Trailing::default()),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    let decided = [Outcome::Pass, Outcome::Fail].map(word);
    let mut trailing = Trailing::default();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let undecided = cols.get(3) == Some(&word(Outcome::Undecided));
        if cols.len() != COLUMNS || !(undecided || decided.contains(&cols[3])) {
            trailing.malformed += 1;
            continue;
        }
        let streak = trailing.streaks.entry(cols[2].to_owned()).or_default();
        if undecided {
            streak.push(cols[0].to_owned());
        } else {
            streak.clear();
        }
    }
    Ok(trailing)
}

/// The check log of waits: one line per (attempt, row, wait, bound), `attempt row wait bound_ms`, tab-separated.
/// Holds only what neither store has: the resource ledger keeps took and stall; gate-ledger.tsv keeps the outcome and the commit, by (run, row).
/// `attempt` = the check run's name, `.rerun` suffixed for a rerun (gate_need.rs `attempt`). `gate-need` joins on (attempt, row, wait) for the bound and takes the commit from gate-ledger.tsv; a logged wait with no ledger record is counted there, not shown.
/// Written before the step's logs are cleaned up; the newest [`KEEP_RUNS`] check runs stay.
pub const WAITS: &str = "gate-waits.tsv";
/// The lock beside [`WAITS`].
pub const WAITS_LOCK: &str = "gate-waits.lock";

/// One wait's bound of one attempt, as [`waits_append`] writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitEntry {
    pub attempt: String,
    pub row: String,
    pub wait: String,
    pub bound_ms: u64,
}

/// Appends `entries` to the check log of checkout `task`, then keeps the newest `keep_runs` check runs; under the log's exclusive lock.
pub fn waits_append(task: &Path, entries: &[WaitEntry], keep_runs: usize) -> Result<()> {
    let path = task.join(WAITS);
    resource_ledger::rewrite(&path, &task.join(WAITS_LOCK), |text| {
        let mut lines: Vec<String> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if parse_wait_line(line).is_none() {
                bail!(
                    "{}:{}: not a check-log line: {line:?}",
                    path.display(),
                    i + 1
                );
            }
            lines.push(line.to_owned());
        }
        lines.extend(
            entries
                .iter()
                .map(|e| format!("{}\t{}\t{}\t{}", e.attempt, e.row, e.wait, e.bound_ms)),
        );
        Ok(newest_runs(&lines, keep_runs))
    })
}

/// The check log's bounds: (attempt, row, wait) -> bound_ms. No log yet: empty.
pub fn read_waits(task: &Path) -> Result<BTreeMap<(String, String, String), u64>> {
    let path = task.join(WAITS);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = resource_ledger::read_text(&path, &task.join(WAITS_LOCK))?;
    let mut bounds = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let (attempt, row, wait, bound) = parse_wait_line(line).with_context(|| {
            format!(
                "{}:{}: not a check-log line: {line:?}",
                path.display(),
                i + 1
            )
        })?;
        bounds.insert((attempt, row, wait), bound);
    }
    Ok(bounds)
}

/// A check-log line's (attempt, row, wait, bound_ms). Strict: four columns, no empty name, a number bound.
fn parse_wait_line(line: &str) -> Option<(String, String, String, u64)> {
    let cols: Vec<&str> = line.split('\t').collect();
    let &[attempt, row, wait, bound] = cols.as_slice() else {
        return None;
    };
    if [attempt, row, wait].iter().any(|c| c.is_empty()) {
        return None;
    }
    Some((
        attempt.to_owned(),
        row.to_owned(),
        wait.to_owned(),
        bound.parse().ok()?,
    ))
}

/// The check run an attempt belongs to: its name without the `.rerun` suffix (gate_need.rs `attempt`). Keys gate-ledger.tsv's run column.
pub fn check_run(attempt: &str) -> &str {
    attempt.strip_suffix(".rerun").unwrap_or(attempt)
}

/// The lines of the newest `keep_runs` check runs, in file order. A run's age is its last line.
fn newest_runs(lines: &[String], keep_runs: usize) -> String {
    let run_of = |line: &str| check_run(line.split('\t').next().unwrap_or_default()).to_owned();
    let mut order: Vec<String> = Vec::new();
    for line in lines {
        let run = run_of(line);
        order.retain(|r| *r != run);
        order.push(run);
    }
    let keep: BTreeSet<String> = order.iter().rev().take(keep_runs).cloned().collect();
    lines
        .iter()
        .filter(|line| keep.contains(&run_of(line)))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// One row's reruns over the selected check runs: `reruns` attempts that reran, by the rerun's outcome.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Retries {
    pub reruns: usize,
    pub pass: usize,
    pub fail: usize,
    pub undecided: usize,
}

/// Each row's retries in the newest `runs` check runs of the outcome ledger: a row whose first attempt ended undecided reran.
pub fn retries(task: &Path, runs: usize) -> Result<BTreeMap<String, Retries>> {
    let path = task.join(LEDGER);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    // lines that are no ledger line are not read here (trailing_undecided counts them)
    let lines: Vec<Vec<&str>> = text
        .lines()
        .map(|line| line.split('\t').collect::<Vec<&str>>())
        .filter(|cols| cols.len() == COLUMNS)
        .collect();
    let mut order: Vec<&str> = Vec::new();
    for cols in &lines {
        order.retain(|r| *r != cols[0]);
        order.push(cols[0]);
    }
    let keep: BTreeSet<&str> = order.iter().rev().take(runs).copied().collect();
    let mut totals: BTreeMap<String, Retries> = BTreeMap::new();
    for cols in lines.iter().filter(|cols| keep.contains(cols[0])) {
        let row = totals.entry(cols[2].to_owned()).or_default();
        // column 4 `first`: the attempt before a rerun, `-` when the row ran once; column 3 is the deciding outcome
        if cols[4] != "-" {
            row.reruns += 1;
            match cols[3] {
                "pass" => row.pass += 1,
                "fail" => row.fail += 1,
                "undecided" => row.undecided += 1,
                _ => {}
            }
        }
    }
    Ok(totals)
}

/// gate-ledger.tsv's commits of this checkout, per (run, row). `malformed`: lines that are no ledger line, skipped (a torn append; trailing_undecided counts them too).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Commits {
    pub by_run_row: BTreeMap<(String, String), String>,
    pub malformed: usize,
}

/// gate-ledger.tsv's commits of checkout `task`, per (run, row); no ledger yet: empty.
pub fn commits(task: &Path) -> Result<Commits> {
    let path = task.join(LEDGER);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Commits::default()),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    let decided = [Outcome::Pass, Outcome::Fail, Outcome::Undecided].map(word);
    let mut commits = Commits::default();
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let well_formed = cols.len() == COLUMNS
            && decided.contains(&cols[3])
            && cols[..3].iter().all(|c| !c.is_empty());
        if well_formed {
            commits
                .by_run_row
                .insert((cols[0].to_owned(), cols[2].to_owned()), cols[1].to_owned());
        } else {
            commits.malformed += 1;
        }
    }
    Ok(commits)
}

/// The run dirs of one bucket, newest first.
fn runs(bucket_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut runs = Vec::new();
    if !bucket_dir.is_dir() {
        return Ok(runs);
    }
    for entry in
        fs::read_dir(bucket_dir).with_context(|| format!("reading {}", bucket_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            runs.push(entry.path());
        }
    }
    runs.sort_unstable_by(|a, b| b.cmp(a));
    Ok(runs)
}

/// The runs past the newest `keep` (`runs` newest first): what `prune` removes.
fn beyond(runs: &[PathBuf], keep: usize) -> &[PathBuf] {
    &runs[keep.min(runs.len())..]
}

/// `out prune`'s pass over the live checkouts' kept logs: lists the runs past [`KEEP_RUNS`] in each bucket (dry run), or removes them under the checkout's build-dir hold (a build holding it is skipped). Returns (runs past the keep, removals that failed).
pub fn prune(ws: &Path, live: &BTreeSet<String>, execute: bool) -> Result<(usize, usize)> {
    let (mut past, mut failed) = (0, 0);
    for id in live {
        let task = ws.join("out").join(id);
        for bucket in BUCKETS {
            let stale = beyond(&runs(&task.join(bucket))?, KEEP_RUNS).to_vec();
            if stale.is_empty() {
                continue;
            }
            past += stale.len();
            if !execute {
                for run in &stale {
                    println!(
                        "kept {bucket} ws/{id}: {} (past the newest {KEEP_RUNS} runs; dry run)",
                        run.display()
                    );
                }
                continue;
            }
            let Some(_lock) = build_pool::try_lock_dir(ws, id)? else {
                println!("kept {bucket} ws/{id}: busy, skipped: a process holds it");
                continue;
            };
            for run in &stale {
                match fs::remove_dir_all(run) {
                    Ok(()) => println!("kept {bucket} ws/{id}: removed {}", run.display()),
                    Err(e) => {
                        failed += 1;
                        println!("kept {bucket} ws/{id}: FAILED {}: {e}", run.display());
                    }
                }
            }
        }
    }
    Ok((past, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-red-logs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// Expected values by calendar arithmetic: 1970-01-01 is day 0; 2026-01-01 is day 20454 (56 years, 14 leap days); 2028-02-29 is day 21243 (58 years, 14 leap days, 59 days into 2028).
    #[test]
    fn run_stamps_are_utc_calendar_time() {
        assert_eq!(utc_stamp(0), "19700101-000000-000");
        assert_eq!(utc_stamp(20_454 * 86_400 * 1_000), "20260101-000000-000");
        assert_eq!(utc_stamp(1_835_440_496_789), "20280229-123456-789");
    }

    #[test]
    fn beyond_is_the_oldest_past_the_newest_kept() {
        let runs: Vec<PathBuf> = ["d3", "d2", "d1", "d0"].iter().map(PathBuf::from).collect();
        assert_eq!(
            beyond(&runs, 2),
            &[PathBuf::from("d1"), PathBuf::from("d0")][..]
        );
        assert!(beyond(&runs, 9).is_empty());
    }

    /// Undecided runs past the keep are removed; red runs at the keep are not, however many undecided runs there are.
    #[test]
    fn prune_drops_the_oldest_of_each_bucket() {
        let ws = scratch("prune");
        let out = ws.join("out").join("land");
        let stamp = |i: usize| format!("20260101-{i:06}-000");
        for i in 0..=KEEP_RUNS {
            fs::create_dir_all(out.join(UNDECIDED).join(stamp(i))).expect("undecided run");
        }
        for i in 0..KEEP_RUNS {
            fs::create_dir_all(out.join(RED).join(stamp(i))).expect("red run");
        }
        let live = BTreeSet::from(["land".to_owned()]);
        assert_eq!(prune(&ws, &live, false).expect("dry run"), (1, 0));
        assert_eq!(
            runs(&out.join(UNDECIDED)).expect("list").len(),
            KEEP_RUNS + 1
        );
        assert_eq!(prune(&ws, &live, true).expect("prune"), (1, 0));
        assert!(
            !out.join(UNDECIDED).join(stamp(0)).exists(),
            "the oldest undecided run stayed"
        );
        assert!(
            out.join(UNDECIDED).join(stamp(KEEP_RUNS)).is_dir(),
            "the newest undecided run went"
        );
        assert_eq!(
            runs(&out.join(RED)).expect("list").len(),
            KEEP_RUNS,
            "a red run was evicted"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A dir a build holds is counted and skipped, not pruned.
    #[test]
    fn prune_skips_a_held_dir() {
        let ws = scratch("held");
        let out = ws.join("out").join("land");
        let stamp = |i: usize| format!("20260101-{i:06}-000");
        for i in 0..=KEEP_RUNS {
            fs::create_dir_all(out.join(RED).join(stamp(i))).expect("red run");
        }
        let live = BTreeSet::from(["land".to_owned()]);
        let _hold = build_pool::hold_dir(&ws, "land", || {}).expect("shared hold");
        assert_eq!(prune(&ws, &live, true).expect("prune"), (1, 0));
        assert_eq!(
            runs(&out.join(RED)).expect("list").len(),
            KEEP_RUNS + 1,
            "a held dir was pruned"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn keep_copies_the_log_and_bucket_follows_outcome() {
        let ws = scratch("keep");
        let logs = ws.join("check-gates");
        fs::create_dir_all(&logs).expect("logs");
        let log = logs.join("window-ime.log");
        fs::write(&log, "window: FAILED ime\n").expect("log");
        let kept = keep(&ws, RED, "20261122-100000-000", &log).expect("keep");
        assert_eq!(
            kept,
            ws.join(RED)
                .join("20261122-100000-000")
                .join("window-ime.log")
        );
        fs::write(&log, "window: pass ime\n").expect("overwrite");
        assert_eq!(
            fs::read_to_string(&kept).expect("kept"),
            "window: FAILED ime\n"
        );
        assert_eq!(bucket_of(Outcome::Pass), None);
        assert_eq!(bucket_of(Outcome::Fail), Some(RED));
        assert_eq!(bucket_of(Outcome::Undecided), Some(UNDECIDED));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The ledger line's columns, then the streaks: per row, the runs since its last pass or fail; another row's lines do not end it. Expected values are written out, not read back from the code.
    #[test]
    fn ledger_streaks_are_per_row_and_a_deciding_run_resets_them() {
        let ws = scratch("ledger");
        let entry = |run: &'static str, row: &'static str, outcome: Outcome| Entry {
            run,
            commit: "c0ffee",
            row,
            outcome,
            first: (outcome == Outcome::Undecided).then_some(Outcome::Undecided),
            stall: None,
            grant: "harness",
            supply: (outcome == Outcome::Undecided).then_some(Supply::Gpu),
        };
        ledger_append(&ws, &[entry("r1", "app", Outcome::Undecided)]).expect("r1");
        ledger_append(
            &ws,
            &[
                entry("r2", "app", Outcome::Undecided),
                entry("r2", "ime", Outcome::Pass),
            ],
        )
        .expect("r2");
        let text = fs::read_to_string(ws.join(LEDGER)).expect("ledger");
        assert_eq!(
            text.lines().next(),
            Some("r1\tc0ffee\tapp\tundecided\tundecided\t-\tharness\tgpu")
        );
        let streaks = trailing_undecided(&ws).expect("streaks").streaks;
        assert_eq!(streaks["app"], ["r1", "r2"]);
        assert!(streaks["ime"].is_empty());
        ledger_append(&ws, &[entry("r3", "app", Outcome::Pass)]).expect("r3");
        ledger_append(&ws, &[entry("r4", "app", Outcome::Undecided)]).expect("r4");
        let path = ws.join(LEDGER);
        let trailing = trailing_undecided(&ws).expect("streaks");
        assert_eq!(trailing.streaks["app"], ["r4"]);
        assert_eq!(trailing.malformed, 0);
        // a truncated line, a seven-column line and an unknown outcome are counted, and end or extend no streak
        let bad = "r5\tc0ffee\tapp\nr6\tc0ffee\tapp\tpass\t-\t-\tharness\nr7\tc0ffee\tapp\tred\t-\t-\tharness\t-\n";
        fs::write(&path, fs::read_to_string(&path).expect("ledger") + bad).expect("bad lines");
        let trailing = trailing_undecided(&ws).expect("streaks");
        assert_eq!(trailing.streaks["app"], ["r4"]);
        assert_eq!(trailing.malformed, 3);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    fn wait_entry(attempt: &str, wait: &str, bound_ms: u64) -> WaitEntry {
        WaitEntry {
            attempt: attempt.to_owned(),
            row: "app".to_owned(),
            wait: wait.to_owned(),
            bound_ms,
        }
    }

    /// The check log keeps each attempt's bound; a rerun's bound is apart from its first attempt's.
    #[test]
    fn the_waits_log_keeps_the_bound_per_attempt() {
        let ws = scratch("waits-bound");
        let entries = [
            wait_entry("r1", "marker", 60_000),
            wait_entry("r1.rerun", "marker", 60_000),
        ];
        waits_append(&ws, &entries, 50).expect("append");
        let bounds = read_waits(&ws).expect("read");
        let key = |attempt: &str| (attempt.to_owned(), "app".to_owned(), "marker".to_owned());
        assert_eq!(bounds.get(&key("r1")), Some(&60_000));
        assert_eq!(bounds.get(&key("r1.rerun")), Some(&60_000));
        let text = fs::read_to_string(ws.join(WAITS)).expect("log");
        assert_eq!(text.lines().next(), Some("r1\tapp\tmarker\t60000"));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The log keeps the newest `keep_runs` check runs, a rerun with its run.
    #[test]
    fn the_waits_log_keeps_the_newest_check_runs() {
        let ws = scratch("waits-keep");
        for run in [
            "20261126-100000-000",
            "20261126-110000-000",
            "20261126-120000-000",
        ] {
            let rerun = format!("{run}.rerun");
            let entries = [
                wait_entry(run, "marker", 9),
                wait_entry(&rerun, "marker", 9),
            ];
            waits_append(&ws, &entries, 2).expect("append");
        }
        let runs: BTreeSet<String> = read_waits(&ws)
            .expect("read")
            .into_keys()
            .map(|(attempt, _, _)| attempt.trim_end_matches(".rerun").to_owned())
            .collect();
        assert_eq!(
            runs,
            BTreeSet::from([
                "20261126-110000-000".to_owned(),
                "20261126-120000-000".to_owned()
            ])
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A row whose first attempt ended undecided reran; the totals read the newest `runs` check runs only.
    #[test]
    fn retries_count_the_reruns_of_the_newest_runs() {
        let ws = scratch("retries");
        let entry = |run: &'static str, outcome: Outcome, first: Option<Outcome>| Entry {
            run,
            commit: "c0ffee",
            row: "app",
            outcome,
            first,
            stall: None,
            grant: "quiet",
            supply: None,
        };
        ledger_append(&ws, &[entry("r1", Outcome::Pass, Some(Outcome::Undecided))]).expect("r1");
        ledger_append(&ws, &[entry("r2", Outcome::Fail, Some(Outcome::Undecided))]).expect("r2");
        ledger_append(&ws, &[entry("r3", Outcome::Pass, None)]).expect("r3");
        let want = |reruns, pass, fail, undecided| Retries {
            reruns,
            pass,
            fail,
            undecided,
        };
        assert_eq!(retries(&ws, 2).expect("retries")["app"], want(1, 0, 1, 0));
        assert_eq!(retries(&ws, 3).expect("retries")["app"], want(2, 1, 1, 0));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The check log holds what neither store has: `attempt row wait commit bound_ms`. No took (the resource ledger has it), no outcome (gate-ledger.tsv has it).
    #[test]
    fn the_waits_log_holds_bounds_only() {
        let ws = scratch("waits-bounds-only");
        waits_append(&ws, &[wait_entry("r1", "marker", 60_000)], 50).expect("append");
        let text = fs::read_to_string(ws.join(WAITS)).expect("log");
        assert_eq!(text, "r1\tapp\tmarker\t60000\n");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// gate-ledger.tsv's commits per (run, row): a rerun's row is its run's line. A line that is no ledger line is counted, not read.
    #[test]
    fn gate_ledger_commits_are_keyed_by_run_and_row() {
        let ws = scratch("ledger-commits");
        let entry = |run: &'static str,
                     commit: &'static str,
                     row: &'static str,
                     outcome: Outcome,
                     first: Option<Outcome>| Entry {
            run,
            commit,
            row,
            outcome,
            first,
            stall: None,
            grant: "quiet",
            supply: None,
        };
        ledger_append(
            &ws,
            &[
                entry("r1", "aaa111", "app", Outcome::Pass, None),
                entry("r1", "aaa111", "ime", Outcome::Fail, None),
                entry(
                    "r2",
                    "bbb222",
                    "app",
                    Outcome::Pass,
                    Some(Outcome::Undecided),
                ),
            ],
        )
        .expect("ledger");
        let path = ws.join(LEDGER);
        let text = fs::read_to_string(&path).expect("ledger");
        fs::write(&path, format!("{text}torn\tline")).expect("torn line");
        let commits = commits(&ws).expect("commits");
        assert_eq!(commits.by_run_row.len(), 3);
        assert_eq!(
            commits.by_run_row[&("r2".to_owned(), "app".to_owned())],
            "bbb222"
        );
        assert_eq!(commits.malformed, 1);
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
