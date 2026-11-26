// SPDX-License-Identifier: MIT
//! The gate rows' need inputs: each wait's newest quiet passes, the input of its `need` (gate-starved-red-design 2.2).
//! Kept in the resource ledger (resource_ledger.rs, kind `gate`): one record per (row, wait, attempt) = the attempt's largest took, of a quiet passing attempt. Only a deciding pass enters, so a row adds at most one run per check step.
//! `attempt` = the check run's name, `.rerun` suffixed for a rerun ([`attempt`]).
//! Quiet = stall <= [`QUIET_STALL_PCT`]. A wait's `need` in a row = the largest took of its newest [`WINDOW`] distinct quiet runs there, once [`MIN_PASSES`] hold it; below that unset.
//! The bound stays out of the ledger: the check log (red_logs.rs `gate-waits.tsv`) keeps it per (attempt, row, wait). The commit and the outcome stay in gate-ledger.tsv, which `cargo xtask gate-need` joins by (run, row) for display. Took stays here.
//! `--migrate` moves the old `out/pool/gate-need.tsv` into the ledger once ([`migrate`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::out;
use crate::red_logs;
use crate::resource_ledger::{self, GATE, Record};

/// Quiet row: host stall share at most this (percent). Basis (gate-need.md, 2026-11-24): quiet runs peaked at 0.4 %; parallel lanes put 5-6 % on sibling-loaded rows, which this drops. The retired STARVED_STALL_SHARE was 10 %.
pub const QUIET_STALL_PCT: f64 = 5.0;
/// Quiet passes before a wait's `need` is derived. Basis: design 2.2, `need` = max over n >= 20 quiet passes (independent runs: one run repeats a wait, app-progress 72 samples in 8 passes).
pub const MIN_PASSES: usize = 20;
/// Newest quiet passes of a (row, wait) its `need` is the maximum of. Basis: the rule asks for at least [`MIN_PASSES`]. Every pass more keeps an outlier longer and raises the need, which excuses more reds; a need too low only leaves a red standing, the rule's default. So the smallest window the rule allows.
pub const WINDOW: usize = MIN_PASSES;
/// The old ledger's file name under [`out::POOL`]: read by [`migrate`] only, removed with it.
pub const OLD_LEDGER: &str = "gate-need.tsv";
/// Where [`migrate`] archives the old file: the checkout's task dir, outside the pool (the scheduler path).
pub const MIGRATED: &str = "gate-need.migrated.tsv";

/// A `wait:` line of a log (wait.rs `record`): `wait: <name> took=<ms> bound=<ms> need=<ms|unset>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitLine {
    pub name: String,
    pub took_ms: u64,
    pub bound_ms: u64,
}

/// A log's samples: its `wait:` lines with took < bound. A took >= bound is a timeout (a settle or a late wait), not a sample.
pub fn wait_samples(log: &str) -> Vec<WaitLine> {
    log.lines()
        .filter_map(parse_wait_line)
        .filter(|w| w.took_ms < w.bound_ms)
        .collect()
}

fn parse_wait_line(line: &str) -> Option<WaitLine> {
    let mut cols = line.strip_prefix("wait: ")?.split_whitespace();
    let name = cols.next()?;
    let took_ms = cols.next()?.strip_prefix("took=")?.parse().ok()?;
    let bound_ms = cols.next()?.strip_prefix("bound=")?.parse().ok()?;
    cols.next()?.strip_prefix("need=")?;
    Some(WaitLine {
        name: name.to_owned(),
        took_ms,
        bound_ms,
    })
}

/// A pass's attempt id: the check run's name, `.rerun` suffixed for the rerun that decided its row.
pub fn attempt(run: &str, rerun: bool) -> String {
    if rerun {
        format!("{run}.rerun")
    } else {
        run.to_owned()
    }
}

/// Quiet: the attempt's host stall share is known and at most [`QUIET_STALL_PCT`].
pub fn quiet(stall: Option<f64>) -> bool {
    stall.is_some_and(|s| s <= QUIET_STALL_PCT)
}

/// The ledger records of one deciding passing attempt of gate row `row`: per wait its largest took. Empty unless the attempt is quiet.
pub fn records(attempt: &str, row: &str, stall: Option<f64>, waits: &[WaitLine]) -> Vec<Record> {
    if !quiet(stall) {
        return Vec::new();
    }
    let mut largest: BTreeMap<&str, u64> = BTreeMap::new();
    for w in waits {
        let took = largest.entry(w.name.as_str()).or_insert(0);
        *took = (*took).max(w.took_ms);
    }
    largest
        .into_iter()
        .map(|(wait, took_ms)| Record {
            kind: GATE.to_owned(),
            name: row.to_owned(),
            wait: wait.to_owned(),
            run: attempt.to_owned(),
            took_ms,
            stall,
        })
        .collect()
}

/// The records the window reads, in ledger order: quiet gate records, and per (row, wait) the newest [`WINDOW`] distinct runs with every record of them.
pub fn window(records: &[Record]) -> Vec<&Record> {
    let mut runs: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    let mut kept: Vec<&Record> = records
        .iter()
        .rev()
        .filter(|r| r.kind == GATE && quiet(r.stall))
        .filter(|r| {
            let seen = runs.entry((r.name.as_str(), r.wait.as_str())).or_default();
            if seen.contains(r.run.as_str()) {
                true
            } else if seen.len() < WINDOW {
                seen.insert(r.run.as_str());
                true
            } else {
                false
            }
        })
        .collect();
    kept.reverse();
    kept
}

/// A check log's bounds: (attempt, row, wait) -> bound_ms (red_logs.rs `read_waits`).
pub type Bounds = BTreeMap<(String, String, String), u64>;

/// The check-side inputs of the display: the check log's bounds, and gate-ledger.tsv's commits by (run, row).
#[derive(Debug, Default)]
pub struct Joins {
    pub bounds: Bounds,
    pub commits: red_logs::Commits,
}

/// One (row, wait)'s line over its window.
#[derive(Debug, PartialEq)]
pub struct Stat {
    pub row: String,
    pub wait: String,
    /// records in the window: one per pass
    pub samples: usize,
    /// distinct quiet runs in the window: the count [`MIN_PASSES`] tests
    pub passes: usize,
    /// largest took in the window
    pub max_ms: u64,
    /// `max_ms` once [`MIN_PASSES`] passes hold it
    pub need: Option<u64>,
    /// largest took / bound over the window's records whose bound is logged
    pub ratio: Option<f64>,
    /// smallest and largest logged bound
    pub bounds: Option<(u64, u64)>,
    /// the commit of the largest took's run, from gate-ledger.tsv by (run, row); None when that run has no line there
    pub commit: Option<String>,
}

/// Every (row, wait) of the window, largest took / bound first (unlogged bounds last).
pub fn stats(records: &[Record], joins: &Joins) -> Vec<Stat> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Record>> = BTreeMap::new();
    for r in window(records) {
        groups
            .entry((r.name.as_str(), r.wait.as_str()))
            .or_default()
            .push(r);
    }
    let mut stats: Vec<Stat> = groups
        .into_iter()
        .map(|((row, wait), group)| {
            let passes: BTreeSet<&str> = group.iter().map(|r| r.run.as_str()).collect();
            let max_ms = group.iter().map(|r| r.took_ms).fold(0, u64::max);
            let known: Vec<(f64, u64)> = group
                .iter()
                .filter_map(|r| {
                    let bound = *joins.bounds.get(&bound_key(r))?;
                    Some((r.took_ms as f64 / bound as f64, bound))
                })
                .collect();
            let ratio = known.iter().map(|(ratio, _)| *ratio).reduce(f64::max);
            let range = known.iter().fold(None, |acc: Option<(u64, u64)>, (_, b)| {
                Some(acc.map_or((*b, *b), |(lo, hi)| (lo.min(*b), hi.max(*b))))
            });
            let commit = group
                .iter()
                .max_by_key(|r| r.took_ms)
                .and_then(|r| joins.commits.by_run_row.get(&commit_key(r)).cloned());
            Stat {
                row: row.to_owned(),
                wait: wait.to_owned(),
                samples: group.len(),
                passes: passes.len(),
                max_ms,
                need: (passes.len() >= MIN_PASSES).then_some(max_ms),
                ratio,
                bounds: range,
                commit,
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.ratio
            .unwrap_or(-1.0)
            .total_cmp(&a.ratio.unwrap_or(-1.0))
            .then_with(|| (&a.row, &a.wait).cmp(&(&b.row, &b.wait)))
    });
    stats
}

/// What the join could not show, counted for the display (`gate-need` prints both).
#[derive(Debug, PartialEq, Eq)]
pub struct JoinGaps {
    /// window records (quiet deciding passes) with no logged bound: migrated, or past the check log's runs
    pub unbounded: usize,
    /// logged waits with no ledger record: not quiet, not the deciding pass, or past the window; not shown
    pub unledgered: usize,
    /// window records whose (run, row) has no gate-ledger.tsv line: commit shown as -
    pub uncommitted: usize,
}

/// The check-log key of a ledger record: its attempt, row and wait (the join key of [`stats`]).
fn bound_key(r: &Record) -> (String, String, String) {
    (r.run.clone(), r.name.clone(), r.wait.clone())
}

/// The gate-ledger.tsv key of a ledger record: its check run (a rerun maps to its run) and row.
fn commit_key(r: &Record) -> (String, String) {
    (red_logs::check_run(&r.run).to_owned(), r.name.clone())
}

/// What the join could not show: window records with no logged bound, and logged waits that no ledger record backs.
pub fn join_gaps(records: &[Record], joins: &Joins) -> JoinGaps {
    let unbounded = window(records)
        .into_iter()
        .filter(|r| !joins.bounds.contains_key(&bound_key(r)))
        .count();
    let uncommitted = window(records)
        .into_iter()
        .filter(|r| !joins.commits.by_run_row.contains_key(&commit_key(r)))
        .count();
    let ledgered: BTreeSet<(&str, &str, &str)> = records
        .iter()
        .filter(|r| r.kind == GATE)
        .map(|r| (r.run.as_str(), r.name.as_str(), r.wait.as_str()))
        .collect();
    let unledgered = joins
        .bounds
        .keys()
        .filter(|(run, row, wait)| !ledgered.contains(&(run.as_str(), row.as_str(), wait.as_str())))
        .count();
    JoinGaps {
        unbounded,
        unledgered,
        uncommitted,
    }
}

/// Each wait's derived `need` (ms) in `row`. A wait below [`MIN_PASSES`] quiet passes there has no entry (unset).
pub fn needs(records: &[Record], row: &str) -> BTreeMap<String, u64> {
    stats(records, &Joins::default())
        .into_iter()
        .filter(|s| s.row == row)
        .filter_map(|s| Some((s.wait, s.need?)))
        .collect()
}

/// `row`'s derived needs in workspace `ws`'s ledger: what the harness reads once per process (xtask-harness wait.rs).
pub fn derived(ws: &Path, row: &str) -> Result<BTreeMap<String, u64>> {
    Ok(needs(&resource_ledger::read(ws)?, row))
}

/// `--migrate`: the old `out/pool/gate-need.tsv` (`run commit row wait took_ms bound_ms stall`) into the ledger, once. Quiet lines become records; commit and bound are projected away (the archive keeps them). Retired seed lines (`seed:*` run, row `-`) and not-quiet lines are dropped; a malformed line refuses the whole file. Dry run unless `execute`: reports, writes nothing. Execute: merges in front of the ledger's records (the migrated runs are older; a repeat changes nothing), then renames the old file to `<task>/`[`MIGRATED`].
pub fn migrate(ws: &Path, task: &Path, execute: bool) -> Result<String> {
    let old = ws.join(out::POOL).join(OLD_LEDGER);
    let text = match fs::read_to_string(&old) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("migrate: no {} to migrate", old.display()));
        }
        Err(e) => return Err(e).with_context(|| old.display().to_string()),
    };
    let (mut kept, mut retired, mut busy, mut malformed) = (Vec::new(), 0, 0, 0);
    for line in text.lines() {
        match OldLine::parse(line) {
            None => malformed += 1,
            Some(o) if o.row == "-" || o.run.starts_with("seed:") => retired += 1,
            Some(o) if !quiet(o.stall) => busy += 1,
            Some(o) => kept.push(Record {
                kind: GATE.to_owned(),
                name: o.row,
                wait: o.wait,
                run: o.run,
                took_ms: o.took_ms,
                stall: o.stall,
            }),
        }
    }
    if malformed > 0 {
        bail!(
            "migrate: {malformed} malformed line(s) in {}; nothing written",
            old.display()
        );
    }
    let before = resource_ledger::read(ws)?;
    let after = resource_ledger::older_first(&kept, before.clone(), WINDOW);
    let report = format!(
        "migrate: {} quiet records kept ({retired} retired and {busy} not quiet dropped); ledger {} -> {} records",
        kept.len(),
        before.len(),
        after.len()
    );
    if !execute {
        return Ok(format!(
            "{report}; dry run, nothing written (--execute writes)"
        ));
    }
    resource_ledger::append_older(ws, &kept, WINDOW)?;
    fs::create_dir_all(task).with_context(|| format!("creating {}", task.display()))?;
    let archive = task.join(MIGRATED);
    fs::rename(&old, &archive)
        .with_context(|| format!("archiving {} to {}", old.display(), archive.display()))?;
    Ok(format!(
        "{report}; written; old file archived to {}",
        archive.display()
    ))
}

/// One line of the old ledger, read by [`migrate`] only: `run commit row wait took_ms bound_ms stall`. Commit and bound are checked, then dropped.
struct OldLine {
    run: String,
    row: String,
    wait: String,
    took_ms: u64,
    stall: Option<f64>,
}

impl OldLine {
    fn parse(line: &str) -> Option<Self> {
        let cols: Vec<&str> = line.split('\t').collect();
        let &[run, _commit, row, wait, took, bound, stall] = cols.as_slice() else {
            return None;
        };
        bound.parse::<u64>().ok()?;
        let stall = match stall {
            "-" => None,
            s => Some(s.parse::<f64>().ok().filter(|v| v.is_finite())?),
        };
        Some(Self {
            run: run.to_owned(),
            row: row.to_owned(),
            wait: wait.to_owned(),
            took_ms: took.parse().ok()?,
            stall,
        })
    }
}

/// `cargo xtask gate-need` arguments.
#[derive(clap::Args)]
pub struct Args {
    /// One-time migration of the old `out/pool/gate-need.tsv` into the resource ledger. A dry run without `--execute`.
    #[arg(long)]
    migrate: bool,
    /// With `--migrate`: write the ledger and archive the old file under the checkout's task dir.
    #[arg(long)]
    execute: bool,
}

/// `cargo xtask gate-need`: each (row, wait)'s window (samples, passes, largest took, need or unset, largest took / bound), then each row's reruns in the newest check runs. Read-only, except `--migrate --execute`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ws = atl_layout::Layout::of(root)?.workspace().to_owned();
    let task = red_logs::task_dir(root)?;
    if args.migrate {
        println!("{}", migrate(&ws, &task, args.execute)?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.execute {
        bail!("--execute goes with --migrate");
    }
    let records = resource_ledger::read(&ws)?;
    let joins = Joins {
        bounds: red_logs::read_waits(&task)?,
        commits: red_logs::commits(&task)?,
    };
    println!(
        "gate-need: {} records, {} in the windows (newest {WINDOW} passes, stall <= {QUIET_STALL_PCT} %); {}",
        records.len(),
        window(&records).len(),
        resource_ledger::ledger_path(&ws).display()
    );
    println!("row\twait\tsamples\tpasses\tmax_ms\tneed_ms\tmax/bound\tbound_ms\tcommit");
    for st in stats(&records, &joins) {
        let need = st
            .need
            .map_or_else(|| "unset".to_owned(), |n| n.to_string());
        let ratio = st
            .ratio
            .map_or_else(|| "-".to_owned(), |r| format!("{r:.3}"));
        let bound = st
            .bounds
            .map_or_else(|| "-".to_owned(), |(lo, hi)| format!("{lo}-{hi}"));
        let commit = st.commit.as_deref().unwrap_or("-");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{need}\t{ratio}\t{bound}\t{commit}",
            st.row, st.wait, st.samples, st.passes, st.max_ms
        );
    }
    let gaps = join_gaps(&records, &joins);
    println!(
        "gate-need: {} window records with no logged bound (max/bound shown as -); {} logged waits with no ledger record (not quiet, not the deciding pass, or past the window): not shown; {} window records whose (run, row) is not in gate-ledger.tsv (commit shown as -)",
        gaps.unbounded, gaps.unledgered, gaps.uncommitted
    );
    println!(
        "gate-need: {} malformed gate-ledger.tsv lines skipped",
        joins.commits.malformed
    );
    let retries = red_logs::retries(&task, WINDOW)?;
    println!(
        "retries: row\treruns\tpass\tfail\tundecided (newest {WINDOW} check runs of this checkout)"
    );
    for (row, r) in retries.iter().filter(|(_, r)| r.reruns > 0) {
        println!(
            "{row}\t{}\t{}\t{}\t{}",
            r.reruns, r.pass, r.fail, r.undecided
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gate-need-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn rec(run: &str, row: &str, wait: &str, took_ms: u64, stall: Option<f64>) -> Record {
        Record {
            kind: GATE.to_owned(),
            name: row.to_owned(),
            wait: wait.to_owned(),
            run: run.to_owned(),
            took_ms,
            stall,
        }
    }

    #[test]
    fn wait_lines_are_samples_only_below_their_bound() {
        let log = "wait: a took=100 bound=100 need=unset\nwait: a took=99 bound=100 need=unset\nwait: b took=5 bound=10 need=7\nnoise took=1 bound=2 need=unset\nwait: c took=3 bound=10\n";
        let got: Vec<(String, u64)> = wait_samples(log)
            .into_iter()
            .map(|w| (w.name, w.took_ms))
            .collect();
        assert_eq!(got, vec![("a".to_owned(), 99), ("b".to_owned(), 5)]);
    }

    #[test]
    fn quiet_is_a_stall_share_at_most_five_percent() {
        assert!(quiet(Some(0.0)));
        assert!(quiet(Some(5.0)));
        assert!(!quiet(Some(5.1)));
        assert!(!quiet(None));
    }

    /// 20 distinct quiet runs with waits 20..1 ms: the need is 20 ms.
    #[test]
    fn twenty_quiet_runs_with_waits_twenty_to_one_need_twenty() {
        let records: Vec<Record> = (1..=20)
            .map(|i| rec(&format!("r{i}"), "app", "marker", 21 - i, Some(0.1)))
            .collect();
        assert_eq!(needs(&records, "app").get("marker"), Some(&20));
    }

    /// 19 quiet runs derive nothing: the need waits for MIN_PASSES.
    #[test]
    fn nineteen_quiet_runs_derive_no_need() {
        let records: Vec<Record> = (1..=19)
            .map(|i| rec(&format!("r{i}"), "app", "marker", 20 - i, Some(0.1)))
            .collect();
        assert!(needs(&records, "app").is_empty());
    }

    /// A 21st run of 1 ms evicts the oldest (r1, 20 ms): the need drops to 19 ms.
    #[test]
    fn the_21st_run_evicts_the_oldest_and_the_need_drops_to_19() {
        let mut records: Vec<Record> = (1..=20)
            .map(|i| rec(&format!("r{i}"), "app", "marker", 21 - i, Some(0.1)))
            .collect();
        records.push(rec("r21", "app", "marker", 1, Some(0.1)));
        assert_eq!(needs(&records, "app").get("marker"), Some(&19));
    }

    /// The same eviction through the store: 21 appends keep the newest 20 runs, so the derived need is 19 ms.
    #[test]
    fn the_store_evicts_the_oldest_run_the_need_reads() {
        let ws = scratch("store");
        for i in 1..=21u64 {
            let took = if i == 21 { 1 } else { 21 - i };
            let r = rec(&format!("r{i}"), "app", "marker", took, Some(0.1));
            resource_ledger::append(&ws, &[r], WINDOW).expect("append");
        }
        assert_eq!(
            derived(&ws, "app").expect("derived").get("marker"),
            Some(&19)
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Records of one run are one pass: 20 records of r1 are not 20 passes.
    #[test]
    fn repeated_records_of_one_run_count_once() {
        let records: Vec<Record> = (1..=20)
            .map(|i| rec("r1", "app", "marker", i, Some(0.1)))
            .collect();
        let stat = stats(&records, &Joins::default())
            .into_iter()
            .find(|s| s.wait == "marker")
            .expect("marker");
        assert_eq!((stat.passes, stat.need), (1, None));
    }

    /// The window reads quiet passes only: a 50 ms spike at 6 % stall is no pass and no maximum.
    #[test]
    fn a_50ms_spike_outside_the_quiet_filter_is_excluded() {
        let mut records: Vec<Record> = (1..=19)
            .map(|i| rec(&format!("r{i}"), "app", "marker", i, Some(0.1)))
            .collect();
        records.push(rec("busy", "app", "marker", 50, Some(6.0)));
        assert!(
            needs(&records, "app").is_empty(),
            "19 quiet passes and a busy one"
        );
        records.push(rec("r20", "app", "marker", 20, Some(0.1)));
        assert_eq!(
            needs(&records, "app").get("marker"),
            Some(&20),
            "the spike is neither a pass nor the maximum"
        );
    }

    /// An outlier is the oldest pass: it holds the need while 19 passes follow it, and the 20th drops it.
    #[test]
    fn an_outlier_leaves_with_the_twentieth_pass_after_it() {
        let mut records = vec![rec("run0", "app", "marker", 50_000, Some(0.1))];
        for i in 1..=19 {
            records.push(rec(&format!("run{i}"), "app", "marker", 100 + i, Some(0.1)));
        }
        assert_eq!(needs(&records, "app").get("marker"), Some(&50_000));
        records.push(rec("busy", "app", "marker", 70_000, Some(9.0)));
        assert_eq!(
            needs(&records, "app").get("marker"),
            Some(&50_000),
            "a busy pass does not move the window"
        );
        records.push(rec("run20", "app", "marker", 110, Some(0.1)));
        assert_eq!(
            needs(&records, "app").get("marker"),
            Some(&119),
            "run19's 119: the largest of runs 1..=20"
        );
    }

    /// Rows do not share a need: each has its own 20 passes.
    #[test]
    fn rows_do_not_share_a_need() {
        let mut records = Vec::new();
        for i in 1..=10 {
            let run = format!("run{i}");
            records.push(rec(&run, "ime", "marker", 100, Some(0.1)));
            records.push(rec(&run, "tiled", "marker", 900, Some(0.1)));
        }
        assert!(needs(&records, "ime").is_empty());
        assert!(needs(&records, "tiled").is_empty());
        for i in 11..=20 {
            records.push(rec(&format!("run{i}"), "ime", "marker", 200, Some(0.1)));
        }
        assert_eq!(needs(&records, "ime").get("marker"), Some(&200));
        assert!(needs(&records, "tiled").is_empty());
    }

    /// A deciding passing attempt's records: per wait its largest took; a busy or unknown-stall attempt has none; a rerun has its own attempt id.
    #[test]
    fn a_passing_attempt_records_each_waits_largest_took() {
        let wait = |name: &str, took_ms: u64| WaitLine {
            name: name.to_owned(),
            took_ms,
            bound_ms: 60_000,
        };
        let waits = [
            wait("marker", 100),
            wait("marker", 300),
            wait("app-progress", 5),
        ];
        let got: Vec<(String, u64, String)> = records("r1", "app", Some(0.04), &waits)
            .into_iter()
            .map(|r| (r.wait, r.took_ms, r.run))
            .collect();
        assert_eq!(
            got,
            [
                ("app-progress".to_owned(), 5, "r1".to_owned()),
                ("marker".to_owned(), 300, "r1".to_owned()),
            ]
        );
        assert!(
            records("r1", "app", Some(6.0), &waits).is_empty(),
            "a busy pass"
        );
        assert!(
            records("r1", "app", None, &waits).is_empty(),
            "an unknown stall"
        );
        assert_eq!(attempt("r1", true), "r1.rerun");
        assert_eq!(attempt("r1", false), "r1");
    }

    /// The query joins each window record's bound by (attempt, row, wait): a pass with no logged bound counts its took, not the ratio.
    #[test]
    fn the_query_joins_the_bounds_of_the_window_attempts() {
        let records = vec![
            rec("r1", "app", "marker", 100, Some(0.1)),
            rec("r2", "app", "marker", 300, Some(0.1)),
            rec("r3", "app", "marker", 900, Some(0.1)),
        ];
        let mut bounds = Bounds::new();
        bounds.insert(
            ("r1".to_owned(), "app".to_owned(), "marker".to_owned()),
            1_000,
        );
        bounds.insert(
            ("r2".to_owned(), "app".to_owned(), "marker".to_owned()),
            600,
        );
        let joins = Joins {
            bounds,
            ..Joins::default()
        };
        let stat = stats(&records, &joins).into_iter().next().expect("marker");
        assert_eq!(stat.bounds, Some((600, 1_000)));
        assert_eq!(stat.ratio, Some(0.5), "300 / 600");
        assert_eq!(stat.max_ms, 900, "the unlogged pass still counts its took");
    }

    /// `--migrate`: quiet old lines become records (commit and bound projected away); seed, busy and unknown-stall lines are dropped; a malformed line refuses the file; a dry run writes nothing; execute writes, archives the old file, and a repeat changes nothing.
    #[test]
    fn migration_keeps_the_quiet_records_and_archives_the_old_file() {
        let ws = scratch("migrate");
        let task = ws.join("task");
        let old = ws.join(out::POOL).join(OLD_LEDGER);
        fs::create_dir_all(old.parent().expect("pool dir")).expect("pool dir");
        let text = "r1\tc0ffee\tapp\tmarker\t100\t60000\t0.10\n\
                    seed:out/a.log\tseed\t-\tconfigure-ack\t8755\t60000\t0.04\n\
                    r1\tc0ffee\tapp\tbusy\t5\t10\t9.00\n\
                    r2\tc0ffee\tapp\tmarker\t200\t60000\t-\n\
                    r2\tc0ffee\tapp\tmarker\t300\t60000\t0.20\n";
        fs::write(&old, text).expect("old ledger");
        let dry = migrate(&ws, &task, false).expect("dry run");
        assert!(dry.contains("2 quiet records"), "{dry}");
        assert!(
            old.exists() && !resource_ledger::ledger_path(&ws).exists(),
            "a dry run writes nothing"
        );
        migrate(&ws, &task, true).expect("execute");
        let got = resource_ledger::read(&ws).expect("ledger");
        assert_eq!(
            got,
            vec![
                rec("r1", "app", "marker", 100, Some(0.10)),
                rec("r2", "app", "marker", 300, Some(0.20)),
            ]
        );
        assert!(!old.exists(), "the old file left the pool");
        assert_eq!(
            fs::read_to_string(task.join(MIGRATED)).expect("archive"),
            text
        );
        fs::write(&old, text).expect("old ledger again");
        migrate(&ws, &task, true).expect("repeat");
        assert_eq!(resource_ledger::read(&ws).expect("ledger"), got);
        fs::write(&old, format!("{text}oops\n")).expect("bad old ledger");
        assert!(
            migrate(&ws, &task, true).is_err(),
            "a malformed line refuses"
        );
        assert_eq!(
            resource_ledger::read(&ws).expect("ledger"),
            got,
            "nothing written on refusal"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The new binary's verify check ran first, so the ledger holds r20..r25. The migrated runs are older: they go first, in age order; a shared key keeps one record, the larger took.
    #[test]
    fn migration_into_a_nonempty_ledger_puts_the_older_runs_first_and_dedupes() {
        let ws = scratch("migrate-nonempty");
        let task = ws.join("task");
        let old = ws.join(out::POOL).join(OLD_LEDGER);
        fs::create_dir_all(old.parent().expect("pool dir")).expect("pool dir");
        let fresh: Vec<Record> = (20..=25)
            .map(|i| rec(&format!("r{i}"), "app", "marker", 100 + i, Some(0.1)))
            .collect();
        resource_ledger::append(&ws, &fresh, WINDOW).expect("fresh records");
        let mut text: String = (1..=5)
            .map(|i| format!("r{i}\tc0ffee\tapp\tmarker\t{}\t60000\t0.10\n", 10 + i))
            .collect();
        text.push_str("r20\tc0ffee\tapp\tmarker\t999\t60000\t0.10\n");
        fs::write(&old, &text).expect("old ledger");
        migrate(&ws, &task, true).expect("migrate");
        let got = resource_ledger::read(&ws).expect("ledger");
        let runs: Vec<&str> = got.iter().map(|r| r.run.as_str()).collect();
        assert_eq!(
            runs,
            [
                "r1", "r2", "r3", "r4", "r5", "r20", "r21", "r22", "r23", "r24", "r25"
            ],
            "the older runs come first, in age order"
        );
        let r20: Vec<u64> = got
            .iter()
            .filter(|r| r.run == "r20")
            .map(|r| r.took_ms)
            .collect();
        assert_eq!(r20, [999], "a shared key is one record, the larger took");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A full window: the migrated runs are older than the ledger's twenty, so the window cuts them, not the fresh ones.
    #[test]
    fn migration_into_a_full_window_cuts_the_migrated_runs_first() {
        let ws = scratch("migrate-full");
        let task = ws.join("task");
        let old = ws.join(out::POOL).join(OLD_LEDGER);
        fs::create_dir_all(old.parent().expect("pool dir")).expect("pool dir");
        let fresh: Vec<Record> = (6..=25)
            .map(|i| rec(&format!("r{i}"), "app", "marker", i, Some(0.1)))
            .collect();
        resource_ledger::append(&ws, &fresh, WINDOW).expect("fresh records");
        let text: String = (1..=5)
            .map(|i| format!("r{i}\tc0ffee\tapp\tmarker\t{i}\t60000\t0.10\n"))
            .collect();
        fs::write(&old, &text).expect("old ledger");
        migrate(&ws, &task, true).expect("migrate");
        let runs: Vec<String> = resource_ledger::read(&ws)
            .expect("ledger")
            .into_iter()
            .map(|r| r.run)
            .collect();
        assert_eq!(runs, (6..=25).map(|i| format!("r{i}")).collect::<Vec<_>>());
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The join shows what it can and counts the rest: a window record with no logged bound, and a logged wait with no ledger record.
    #[test]
    fn the_join_counts_what_it_cannot_show() {
        let records = vec![
            rec("r1", "app", "marker", 100, Some(0.1)),
            rec("r2", "app", "marker", 300, Some(0.1)),
        ];
        let mut bounds = Bounds::new();
        bounds.insert(
            ("r1".to_owned(), "app".to_owned(), "marker".to_owned()),
            1_000,
        );
        // a busy pass: logged, no ledger record
        bounds.insert(
            ("r9".to_owned(), "app".to_owned(), "marker".to_owned()),
            600,
        );
        let mut commits = red_logs::Commits::default();
        commits
            .by_run_row
            .insert(("r1".to_owned(), "app".to_owned()), "aaa111".to_owned());
        let joins = Joins { bounds, commits };
        assert_eq!(
            join_gaps(&records, &joins),
            JoinGaps {
                unbounded: 1,
                unledgered: 1,
                uncommitted: 1,
            },
            "r2 has no bound and no commit; r9 has no record"
        );
    }

    /// gate-need takes each row's commit from gate-ledger.tsv by (run, row): a rerun's record maps to its run's line; the commit shown is the one of the window's largest took.
    #[test]
    fn the_query_takes_each_commit_from_the_gate_ledger_by_run_and_row() {
        let records = vec![
            rec("r1", "app", "marker", 300, Some(0.1)),
            rec("r2.rerun", "app", "marker", 900, Some(0.1)),
        ];
        let mut commits = red_logs::Commits::default();
        commits
            .by_run_row
            .insert(("r1".to_owned(), "app".to_owned()), "aaa111".to_owned());
        commits
            .by_run_row
            .insert(("r2".to_owned(), "app".to_owned()), "bbb222".to_owned());
        let joins = Joins {
            bounds: Bounds::new(),
            commits,
        };
        let stat = stats(&records, &joins).into_iter().next().expect("marker");
        assert_eq!(
            stat.commit.as_deref(),
            Some("bbb222"),
            "the largest took is the rerun of r2"
        );
    }

    /// A rerun's bound is its own: the join keys on the attempt, so `<run>.rerun` does not take `<run>`'s bound.
    #[test]
    fn a_rerun_joins_its_own_bound() {
        let records = vec![rec("r1.rerun", "app", "marker", 300, Some(0.1))];
        let mut bounds = Bounds::new();
        bounds.insert(
            ("r1".to_owned(), "app".to_owned(), "marker".to_owned()),
            1_000,
        );
        bounds.insert(
            ("r1.rerun".to_owned(), "app".to_owned(), "marker".to_owned()),
            600,
        );
        let joins = Joins {
            bounds,
            ..Joins::default()
        };
        let stat = stats(&records, &joins).into_iter().next().expect("marker");
        assert_eq!(stat.bounds, Some((600, 600)));
        assert_eq!(stat.ratio, Some(0.5), "300 / 600");
    }
}
