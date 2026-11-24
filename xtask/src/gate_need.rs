// SPDX-License-Identifier: MIT
//! The need ledger: each wait's newest quiet samples on this machine, per gate row, the input of each wait's `need` (gate-starved-red-design 2.2).
//! One file for every checkout: `<workspace>/out/pool/gate-need.tsv`, one line per sample (took < bound) of a row that passed: `run commit row wait took_ms bound_ms stall`, tab-separated.
//! `stall` = the row's host stall share (`host load:` stalled / span, percent), `-` when unknown. A pass is one `run` (one check step) of a row: rows of one step share the machine's load, so they are not independent.
//! Quiet = stall <= [`QUIET_STALL_PCT`]. A wait's `need` in a row = the largest sample of its newest [`WINDOW`] quiet passes there, once [`MIN_PASSES`] hold it; below that unset.
//! Line order is age (appends are serialized). Retention = the window: an append rewrites the ledger with only what [`windowed`] reads, so it stays within WINDOW passes per (row, wait). Raising QUIET_STALL_PCT or WINDOW cannot bring dropped lines back.
//! Appends take the exclusive flock [`LOCK`] and replace the file by rename (a killed writer leaves the old ledger whole), reads take the shared one: every checkout's gates step writes here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::build_pool;
use crate::flock::{Flock, Mode};
use crate::out;

/// Quiet row: host stall share at most this (percent). Basis (gate-need.md, 2026-11-24): quiet runs peaked at 0.4 %; parallel lanes put 5-6 % on sibling-loaded rows, which this drops. The retired STARVED_STALL_SHARE was 10 %.
pub const QUIET_STALL_PCT: f64 = 5.0;
/// Quiet passes before a wait's `need` is derived. Basis: design 2.2, `need` = max over n >= 20 quiet passes (independent runs: one run repeats a wait, app-progress 72 samples in 8 passes).
pub const MIN_PASSES: usize = 20;
/// Newest quiet passes of a (row, wait) its `need` is the maximum of. Basis: the rule asks for at least [`MIN_PASSES`]. Every pass more keeps an outlier longer and raises the need, which excuses more reds; a need too low only leaves a red standing, the rule's default. So the smallest window the rule allows.
pub const WINDOW: usize = MIN_PASSES;
/// The ledger's file name under [`out::POOL`].
pub const LEDGER: &str = "gate-need.tsv";
/// The lock file, beside the pool's slot and dir locks (`build_pool::dir`).
pub const LOCK: &str = "gate-need.lock";

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

/// One ledger line: a wait sample of one row's pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub run: String,
    pub commit: String,
    pub row: String,
    pub wait: String,
    pub took_ms: u64,
    pub bound_ms: u64,
    /// the row's host stall share, percent; None when unknown (never quiet)
    pub stall: Option<f64>,
}

impl Sample {
    /// Quiet: the row's stall share is known and at most [`QUIET_STALL_PCT`].
    pub fn quiet(&self) -> bool {
        self.stall.is_some_and(|s| s <= QUIET_STALL_PCT)
    }

    /// The group a window is taken of: `(row, wait)`.
    fn key(&self) -> (&str, &str) {
        (&self.row, &self.wait)
    }

    fn line(&self) -> String {
        let stall = self.stall.map_or("-".to_owned(), |s| format!("{s:.2}"));
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{stall}\n",
            self.run, self.commit, self.row, self.wait, self.took_ms, self.bound_ms
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let cols: Vec<&str> = line.split('\t').collect();
        let &[run, commit, row, wait, took, bound, stall] = cols.as_slice() else {
            return None;
        };
        Some(Self {
            run: run.to_owned(),
            commit: commit.to_owned(),
            row: row.to_owned(),
            wait: wait.to_owned(),
            took_ms: took.parse().ok()?,
            bound_ms: bound.parse().ok()?,
            stall: match stall {
                "-" => None,
                s => Some(s.parse().ok()?),
            },
        })
    }
}

/// The ledger's path under workspace `ws`: the pool's output dir, which every checkout shares.
pub fn ledger_path(ws: &Path) -> PathBuf {
    ws.join(out::POOL).join(LEDGER)
}

/// Takes the ledger lock: exclusive to rewrite, shared to read.
fn lock(ws: &Path, mode: Mode) -> Result<Flock> {
    let dir = build_pool::dir(ws);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| path.display().to_string())?;
    Flock::take(file, mode).with_context(|| format!("flock {}", path.display()))
}

/// Adds `samples` as the newest and rewrites the ledger to its window, under the exclusive lock.
pub fn append(ws: &Path, samples: &[Sample]) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let _held = lock(ws, Mode::Exclusive)?;
    let mut all = load(ws)?.samples;
    // retired harvest seed lines (run `seed:*`, row `-`): no row reads them, the first rewrite drops them
    all.retain(|s| s.row != "-" && !s.run.starts_with("seed:"));
    all.extend_from_slice(samples);
    replace(ws, &windowed(&all))
}

/// Replaces the ledger with `samples`: temp file, sync, rename. The caller holds the exclusive lock, so one temp name is enough; a killed writer's leftover is truncated by the next.
fn replace(ws: &Path, samples: &[&Sample]) -> Result<()> {
    let path = ledger_path(ws);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tsv.tmp");
    let text: String = samples.iter().map(|s| s.line()).collect();
    let mut file = File::create(&tmp).with_context(|| tmp.display().to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))
}

/// The ledger's samples, and the lines that do not parse (a writer that died mid-line; readers skip them, the next append drops them).
#[derive(Debug, Default)]
pub struct Ledger {
    pub samples: Vec<Sample>,
    pub malformed: usize,
}

/// Reads the whole ledger under the shared lock. No ledger yet: empty.
pub fn read(ws: &Path) -> Result<Ledger> {
    let _held = lock(ws, Mode::Shared)?;
    load(ws)
}

/// Reads the ledger; the caller holds the lock.
fn load(ws: &Path) -> Result<Ledger> {
    let path = ledger_path(ws);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    let mut ledger = Ledger::default();
    for line in text.lines() {
        match Sample::parse(line) {
            Some(sample) => ledger.samples.push(sample),
            None => ledger.malformed += 1,
        }
    }
    Ok(ledger)
}

/// The samples the window reads, in ledger order: per (row, wait), every quiet sample of its newest [`WINDOW`] quiet runs.
pub fn windowed(samples: &[Sample]) -> Vec<&Sample> {
    let mut runs: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    let mut kept: Vec<&Sample> = samples
        .iter()
        .rev()
        .filter(|s| {
            if !s.quiet() {
                return false;
            }
            let seen = runs.entry(s.key()).or_default();
            if seen.len() < WINDOW {
                seen.insert(&s.run);
                return true;
            }
            seen.contains(s.run.as_str())
        })
        .collect();
    kept.reverse();
    kept
}

/// One (row, wait)'s line of the view, over its window.
#[derive(Debug, PartialEq)]
pub struct Stat {
    pub row: String,
    pub wait: String,
    pub samples: usize,
    /// distinct quiet runs in the window: the count [`MIN_PASSES`] tests
    pub passes: usize,
    /// largest sample in the window
    pub max_ms: u64,
    /// `max_ms` once [`MIN_PASSES`] passes hold it
    pub need: Option<u64>,
    /// smallest and largest bound over the samples
    pub bound_ms: (u64, u64),
    /// largest took / bound over the samples
    pub max_ratio: f64,
}

/// Every (row, wait) with a quiet sample, largest took / bound first.
pub fn stats(samples: &[Sample]) -> Vec<Stat> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Sample>> = BTreeMap::new();
    for s in windowed(samples) {
        groups.entry(s.key()).or_default().push(s);
    }
    let mut stats: Vec<Stat> = groups
        .into_iter()
        .map(|((row, wait), group)| {
            let runs: BTreeSet<&str> = group.iter().map(|s| s.run.as_str()).collect();
            let max_ms = group.iter().map(|s| s.took_ms).fold(0, u64::max);
            let bound_ms = group.iter().fold((u64::MAX, 0), |(lo, hi), s| {
                (lo.min(s.bound_ms), hi.max(s.bound_ms))
            });
            let max_ratio = group
                .iter()
                .map(|s| s.took_ms as f64 / s.bound_ms as f64)
                .fold(0.0, f64::max);
            Stat {
                row: row.to_owned(),
                wait: wait.to_owned(),
                samples: group.len(),
                passes: runs.len(),
                max_ms,
                need: (runs.len() >= MIN_PASSES).then_some(max_ms),
                bound_ms,
                max_ratio,
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.max_ratio
            .total_cmp(&a.max_ratio)
            .then_with(|| (&a.row, &a.wait).cmp(&(&b.row, &b.wait)))
    });
    stats
}

/// Each wait's derived `need` (ms) in `row`. A wait below [`MIN_PASSES`] quiet passes there has no entry (unset).
pub fn needs(samples: &[Sample], row: &str) -> BTreeMap<String, u64> {
    stats(samples)
        .into_iter()
        .filter(|s| s.row == row)
        .filter_map(|s| Some((s.wait, s.need?)))
        .collect()
}

/// `row`'s derived needs in workspace `ws`'s ledger: what the harness reads once per process (xtask-harness wait.rs).
pub fn derived(ws: &Path, row: &str) -> Result<BTreeMap<String, u64>> {
    Ok(needs(&read(ws)?.samples, row))
}

/// `cargo xtask gate-need`: each (row, wait)'s window: its samples, quiet passes, largest sample, need (or unset) and largest took / bound. Read-only.
pub fn run(root: &Path) -> Result<ExitCode> {
    let ws = atl_layout::Layout::of(root)?.workspace().to_owned();
    let ledger = read(&ws)?;
    println!(
        "gate-need: {} samples, {} in the windows (newest {WINDOW} passes, stall <= {QUIET_STALL_PCT} %), {} malformed lines; {}",
        ledger.samples.len(),
        windowed(&ledger.samples).len(),
        ledger.malformed,
        ledger_path(&ws).display()
    );
    println!("row\twait\tsamples\tpasses\tmax_ms\tneed_ms\tmax/bound\tbound_ms");
    for st in stats(&ledger.samples) {
        let need = st
            .need
            .map_or_else(|| "unset".to_owned(), |n| n.to_string());
        println!(
            "{}\t{}\t{}\t{}\t{}\t{need}\t{:.3}\t{}-{}",
            st.row,
            st.wait,
            st.samples,
            st.passes,
            st.max_ms,
            st.max_ratio,
            st.bound_ms.0,
            st.bound_ms.1
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gate-need-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn sample(run: &str, row: &str, wait: &str, took_ms: u64, stall: Option<f64>) -> Sample {
        Sample {
            run: run.to_owned(),
            commit: "c0ffee".to_owned(),
            row: row.to_owned(),
            wait: wait.to_owned(),
            took_ms,
            bound_ms: 60_000,
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
        assert!(sample("r", "app", "w", 1, Some(0.0)).quiet());
        assert!(sample("r", "app", "w", 1, Some(5.0)).quiet());
        assert!(!sample("r", "app", "w", 1, Some(5.1)).quiet());
        assert!(!sample("r", "app", "w", 1, None).quiet());
    }

    #[test]
    fn a_wait_needs_twenty_quiet_passes_and_takes_the_largest_quiet_sample() {
        let mut samples: Vec<Sample> = (1..=19)
            .map(|i| sample(&format!("run{i}"), "app", "marker", i * 100, Some(0.1)))
            .collect();
        assert!(
            needs(&samples, "app").is_empty(),
            "19 passes derive nothing"
        );
        samples.push(sample("run20", "app", "marker", 2_500, Some(0.1)));
        assert_eq!(needs(&samples, "app").get("marker"), Some(&2_500));
    }

    #[test]
    fn passes_count_runs_not_samples() {
        let samples: Vec<Sample> = (1..=20)
            .map(|i| sample("run1", "app", "wait-x", i, Some(0.1)))
            .collect();
        assert!(
            needs(&samples, "app").is_empty(),
            "one pass of 20 samples is not 20 passes"
        );
    }

    /// Two rows in each of 10 runs: 20 `(run, row)`, 10 runs. Neither row has its 20, and one row's samples never set the other's need.
    #[test]
    fn passes_are_runs_and_rows_do_not_share_a_need() {
        let mut samples = Vec::new();
        for i in 1..=10 {
            let run = format!("run{i}");
            samples.push(sample(&run, "ime", "marker", 100, Some(0.1)));
            samples.push(sample(&run, "tiled", "marker", 900, Some(0.1)));
        }
        assert!(needs(&samples, "ime").is_empty());
        assert!(needs(&samples, "tiled").is_empty());
        for i in 11..=20 {
            samples.push(sample(&format!("run{i}"), "ime", "marker", 200, Some(0.1)));
        }
        assert_eq!(needs(&samples, "ime").get("marker"), Some(&200));
        assert!(needs(&samples, "tiled").is_empty());
    }

    #[test]
    fn non_quiet_passes_do_not_count_and_never_set_the_need() {
        let mut samples: Vec<Sample> = (1..=19)
            .map(|i| sample(&format!("run{i}"), "app", "marker", 100 * i, Some(0.1)))
            .collect();
        samples.push(sample("run-busy", "app", "marker", 99_000, Some(6.0)));
        assert!(
            needs(&samples, "app").is_empty(),
            "19 quiet passes and one busy one"
        );
        samples.push(sample("run20", "app", "marker", 1_200, Some(0.2)));
        assert_eq!(needs(&samples, "app").get("marker"), Some(&1_900));
    }

    /// The outlier is the oldest pass: it holds the need while 19 passes follow it, and the 20th drops it.
    #[test]
    fn an_outlier_leaves_with_the_twentieth_pass_after_it() {
        let mut samples = vec![sample("run0", "app", "marker", 50_000, Some(0.1))];
        for i in 1..=19 {
            let run = format!("run{i}");
            samples.push(sample(&run, "app", "marker", 100 + i, Some(0.1)));
        }
        assert_eq!(needs(&samples, "app").get("marker"), Some(&50_000));
        // a busy pass does not move the window
        samples.push(sample("busy", "app", "marker", 70_000, Some(9.0)));
        assert_eq!(needs(&samples, "app").get("marker"), Some(&50_000));
        samples.push(sample("run20", "app", "marker", 110, Some(0.1)));
        // the largest of runs 1..=20: run19's 100 + 19
        assert_eq!(needs(&samples, "app").get("marker"), Some(&119));
    }

    #[test]
    fn stats_count_the_windows_samples_and_passes() {
        let samples = vec![
            sample("r1", "app", "marker", 100, Some(0.1)),
            sample("r1", "app", "marker", 300, Some(9.0)),
            sample("r2", "app", "marker", 200, Some(0.2)),
            sample("r2", "app", "marker", 150, Some(0.2)),
            sample("r2", "ime", "marker", 400, Some(0.2)),
        ];
        let stats = stats(&samples);
        let app = stats.iter().find(|s| s.row == "app").expect("app");
        assert_eq!((app.samples, app.passes, app.max_ms), (3, 2, 200));
        assert_eq!(app.need, None);
        assert_eq!(app.bound_ms, (60_000, 60_000));
    }

    /// 30 runs of two marker samples and a busy one: the newest 20 runs stay, both samples of each, in order; another wait's window is its own.
    #[test]
    fn the_ledger_keeps_only_the_window() {
        let ws = scratch("retention");
        append(&ws, &[sample("run-rare", "app", "rare", 7, Some(0.1))]).expect("append");
        for i in 0..30 {
            let run = format!("run{i}");
            let batch = [
                sample(&run, "app", "marker", i, Some(0.1)),
                sample(&run, "app", "marker", i + 1, Some(0.1)),
                sample(&run, "app", "busy", 5, Some(9.0)),
            ];
            append(&ws, &batch).expect("append");
        }
        let runs: Vec<String> = read(&ws)
            .expect("read")
            .samples
            .into_iter()
            .map(|s| s.run)
            .collect();
        let mut want = vec!["run-rare".to_owned()];
        want.extend((10..30).flat_map(|i| [format!("run{i}"), format!("run{i}")]));
        assert_eq!(runs, want);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A rewrite is a new file renamed into place: a handle on the old one still reads it whole, a killed writer's leftover temp is not read, and no temp stays.
    #[test]
    fn a_rewrite_replaces_the_ledger_by_rename() {
        let ws = scratch("rename");
        append(&ws, &[sample("r1", "app", "marker", 100, Some(0.1))]).expect("append");
        let path = ledger_path(&ws);
        let tmp = path.with_extension("tsv.tmp");
        fs::write(&tmp, "r9\tc0\tapp\tmar").expect("leftover");
        let mut old = File::open(&path).expect("open");
        append(&ws, &[sample("r2", "app", "marker", 200, Some(0.1))]).expect("append");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut old, &mut text).expect("read old");
        assert_eq!(text, "r1\tc0ffee\tapp\tmarker\t100\t60000\t0.10\n");
        let ledger = read(&ws).expect("read");
        assert_eq!((ledger.samples.len(), ledger.malformed), (2, 0));
        assert!(!tmp.exists());
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn a_ledger_line_round_trips_and_a_torn_one_is_counted_not_read() {
        let ws = scratch("torn");
        append(&ws, &[sample("r1", "app", "marker", 100, Some(0.25))]).expect("append");
        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger_path(&ws))
            .expect("open");
        file.write_all(b"r1\tc0\tapp\tpong\t5\t10\t-\nr2\tc0\tapp\tmark")
            .expect("torn line");
        let ledger = read(&ws).expect("read");
        assert_eq!((ledger.samples.len(), ledger.malformed), (2, 1));
        assert_eq!(ledger.samples[0].stall, Some(0.25));
        assert_eq!(ledger.samples[1].stall, None);
        // the next append starts on its own line, not on the torn one; the unknown-stall line goes with it
        append(&ws, &[sample("r3", "app", "marker", 7, Some(0.1))]).expect("append");
        let ledger = read(&ws).expect("read");
        assert_eq!((ledger.samples.len(), ledger.malformed), (2, 0));
        assert_eq!(ledger.samples[1].run, "r3");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Eight writers on a full window, so every append drops lines as it rewrites: each writer's own rows all stay, and the full row holds its newest 20.
    #[test]
    fn concurrent_appends_survive_each_others_rewrites() {
        let ws = scratch("concurrent");
        let full: Vec<Sample> = (0..20)
            .map(|i| sample(&format!("full{i}"), "app", "marker", i, Some(0.1)))
            .collect();
        append(&ws, &full).expect("append");
        std::thread::scope(|scope| {
            for t in 0..8 {
                let ws = &ws;
                scope.spawn(move || {
                    for i in 0..10 {
                        let run = format!("t{t}-{i}");
                        let batch = [
                            sample(&run, "app", "marker", 1, Some(0.1)),
                            sample(&run, &format!("row{t}"), "marker", 1, Some(0.1)),
                        ];
                        append(ws, &batch).expect("append");
                    }
                });
            }
        });
        let ledger = read(&ws).expect("read");
        assert_eq!(ledger.malformed, 0, "a torn line from two writers");
        let of = |row: &str| ledger.samples.iter().filter(|s| s.row == row).count();
        for t in 0..8 {
            assert_eq!(of(&format!("row{t}")), 10, "writer {t}");
        }
        assert_eq!(of("app"), 20);
        assert!(!ledger.samples.iter().any(|s| s.run.starts_with("full")));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A retired seed line (run `seed:*`, row `-`) stays until the next append, which drops it; the real rows stay.
    #[test]
    fn a_seed_line_is_gone_after_the_next_append() {
        let ws = scratch("seed-gone");
        fs::create_dir_all(ledger_path(&ws).parent().expect("pool dir")).expect("pool dir");
        fs::write(
            ledger_path(&ws),
            "seed:out/a.log\tseed\t-\tconfigure-ack\t8755\t60000\t0.04\n",
        )
        .expect("seed line");
        assert_eq!(read(&ws).expect("read").samples.len(), 1);
        append(&ws, &[sample("r1", "app", "marker", 100, Some(0.1))]).expect("append");
        let kept: Vec<(String, String)> = read(&ws)
            .expect("read")
            .samples
            .into_iter()
            .map(|s| (s.run, s.row))
            .collect();
        assert_eq!(kept, vec![("r1".to_owned(), "app".to_owned())]);
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
