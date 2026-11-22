// SPDX-License-Identifier: MIT
//! Kept gate logs. A check gate that ends red keeps its log in `<workspace>/out/<checkout id>/check-gates-red/<run>/<gate>.log`; an undecided one in `check-gates-undecided/`. The next check overwrites `check-gates/<gate>.log` in the build dir but never these. `<run>` is the check run's UTC start (`YYYYMMDD-HHMMSS-mmm`), so the names sort oldest first.
//! The logs sit beside the checkout's task logs, not in its build dir: a build dir is regenerable (a ram tier shows it empty outside a build; a land reset, refresh or tier change remakes it), and the evidence must outlive it. A path under `out/<id>/` is readable from any shell.
//! `prune` (out.rs `prune_dirs`: `out prune`, and land's cleanup) keeps the newest [`KEEP_RUNS`] runs of each bucket of each live checkout and removes the rest. Undecided has its own bucket: it is routine on a GPU-starved host, and one shared count would evict the red runs.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::build_pool;
use crate::out;
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
}
