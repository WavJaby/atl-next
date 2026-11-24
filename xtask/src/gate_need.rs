// SPDX-License-Identifier: MIT
//! The need ledger: every passing gate row's wait samples on this machine, the input of each wait's `need` (gate-starved-red-design 2.2).
//! One file for every checkout: `<workspace>/out/pool/gate-need.tsv`, one line per sample (took < bound) of a row that passed: `run commit row wait took_ms bound_ms stall`, tab-separated.
//! `stall` = the row's host stall share (`host load:` stalled / span, percent), `-` when unknown. A pass is one `(run, row)`.
//! Quiet = stall <= [`QUIET_STALL_PCT`], applied on read, so the constant can change without a reseed. A wait's `need` = largest quiet sample once [`MIN_PASSES`] quiet passes hold it; below that unset.
//! Appends take the exclusive flock [`LOCK`], reads the shared one: every checkout's gates step writes here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::build_pool;
use crate::flock::{Flock, Mode};
use crate::out;

/// Quiet row: host stall share at most this (percent). Basis (gate-need.md, 2026-11-24): quiet runs peaked at 0.4 %; parallel lanes put 5-6 % on sibling-loaded rows, which this drops. The retired STARVED_STALL_SHARE was 10 %.
pub const QUIET_STALL_PCT: f64 = 5.0;
/// Quiet passes before a wait's `need` is derived. Basis: design 2.2, `need` = max over n >= 20 quiet passes (independent runs: one run repeats a wait, app-progress 72 samples in 8 passes).
pub const MIN_PASSES: usize = 20;
/// The ledger's file name under [`out::POOL`].
pub const LEDGER: &str = "gate-need.tsv";
/// The lock file, beside the pool's slot and dir locks (`build_pool::dir`).
pub const LOCK: &str = "gate-need.lock";
/// The name 14 `wait_limited` call sites shared until each named its own (binder.rs): a harvest cannot tell their samples apart, so the seed leaves them out.
const SHARED_EXIT: &str = "exit-limited";
const HARVEST_HEADER: &str = "file\tshare_pct\twait\ttook_ms\tbound_ms\tneed";

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

    fn pass(&self) -> (&str, &str) {
        (&self.run, &self.row)
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

/// Takes the ledger lock: exclusive to append, shared to read.
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

/// Appends `samples` in one write under the exclusive lock.
pub fn append(ws: &Path, samples: &[Sample]) -> Result<()> {
    let _held = lock(ws, Mode::Exclusive)?;
    write(ws, samples)
}

/// Writes `samples` to the ledger; the caller holds the lock.
fn write(ws: &Path, samples: &[Sample]) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let path = ledger_path(ws);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text: String = samples.iter().map(Sample::line).collect();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| path.display().to_string())?;
    file.write_all(text.as_bytes())
        .with_context(|| path.display().to_string())
}

/// The ledger's samples, and the lines that do not parse (a writer that died mid-line; readers skip them).
#[derive(Debug, Default)]
pub struct Ledger {
    pub samples: Vec<Sample>,
    pub malformed: usize,
}

/// Reads the whole ledger under the shared lock. No ledger yet: empty.
pub fn read(ws: &Path) -> Result<Ledger> {
    let _held = lock(ws, Mode::Shared)?;
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

/// A wait's quiet passes (distinct `(run, row)`) and the largest quiet sample among them.
type Quiet<'a> = (BTreeSet<(&'a str, &'a str)>, u64);

/// Each wait's derived `need` (ms): the largest quiet sample, once [`MIN_PASSES`] quiet passes hold the wait. A wait below that has no entry (unset).
pub fn needs(samples: &[Sample]) -> BTreeMap<String, u64> {
    let mut passes: BTreeMap<&str, Quiet> = BTreeMap::new();
    for s in samples.iter().filter(|s| s.quiet()) {
        let (set, max) = passes.entry(s.wait.as_str()).or_default();
        set.insert(s.pass());
        *max = (*max).max(s.took_ms);
    }
    passes
        .into_iter()
        .filter(|(_, (set, _))| set.len() >= MIN_PASSES)
        .map(|(wait, (_, max))| (wait.to_owned(), max))
        .collect()
}

/// One wait's line of the view.
#[derive(Debug, PartialEq)]
pub struct Stat {
    pub wait: String,
    pub samples: usize,
    pub quiet: usize,
    /// distinct quiet `(run, row)` passes: the count [`MIN_PASSES`] tests
    pub passes: usize,
    pub need: Option<u64>,
    /// smallest and largest bound over the samples
    pub bound_ms: (u64, u64),
    /// largest took / bound over all samples
    pub max_ratio: f64,
}

/// Every wait in the ledger, largest took / bound first.
pub fn stats(samples: &[Sample]) -> Vec<Stat> {
    let needs = needs(samples);
    let mut by_wait: BTreeMap<&str, Vec<&Sample>> = BTreeMap::new();
    for s in samples {
        by_wait.entry(s.wait.as_str()).or_default().push(s);
    }
    let mut stats: Vec<Stat> = by_wait
        .into_iter()
        .map(|(wait, group)| {
            let quiet: Vec<&&Sample> = group.iter().filter(|s| s.quiet()).collect();
            let passes: BTreeSet<(&str, &str)> = quiet.iter().map(|s| s.pass()).collect();
            let bound_ms = group.iter().fold((u64::MAX, 0), |(lo, hi), s| {
                (lo.min(s.bound_ms), hi.max(s.bound_ms))
            });
            let max_ratio = group
                .iter()
                .map(|s| s.took_ms as f64 / s.bound_ms as f64)
                .fold(0.0, f64::max);
            Stat {
                wait: wait.to_owned(),
                samples: group.len(),
                quiet: quiet.len(),
                passes: passes.len(),
                need: needs.get(wait).copied(),
                bound_ms,
                max_ratio,
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.max_ratio
            .total_cmp(&a.max_ratio)
            .then_with(|| a.wait.cmp(&b.wait))
    });
    stats
}

/// The derived needs of workspace `ws`'s ledger: what the harness reads once per process (xtask-harness wait.rs).
pub fn derived(ws: &Path) -> Result<BTreeMap<String, u64>> {
    Ok(needs(&read(ws)?.samples))
}

/// What a harvest import wrote, and what it left out.
#[derive(Debug, Default, PartialEq)]
pub struct Seeded {
    pub imported: usize,
    /// [`SHARED_EXIT`] samples: no call site to name them by
    pub unattributed: usize,
    /// took >= bound: not samples
    pub not_samples: usize,
}

/// Imports a harvest TSV (header `file share_pct wait took_ms bound_ms need`) as the ledger's first passes, once: refused when the ledger exists.
/// Each harvest file is one pass (run `seed:<file>`, row `-`); its share is the stall. The harvest's own dedupe already ran.
pub fn import_harvest(ws: &Path, tsv: &str) -> Result<Seeded> {
    let _held = lock(ws, Mode::Exclusive)?;
    let path = ledger_path(ws);
    if path.exists() {
        bail!(
            "refused: {} exists; the harvest seeds the ledger once",
            path.display()
        );
    }
    let mut lines = tsv.lines();
    if lines.next() != Some(HARVEST_HEADER) {
        bail!("harvest header is not `{HARVEST_HEADER}`");
    }
    let mut seeded = Seeded::default();
    let mut samples = Vec::new();
    for line in lines {
        let cols: Vec<&str> = line.split('\t').collect();
        let &[file, share, wait, took, bound, _need] = cols.as_slice() else {
            bail!("harvest line is not 6 columns: {line}");
        };
        if wait == SHARED_EXIT {
            seeded.unattributed += 1;
            continue;
        }
        let (took_ms, bound_ms): (u64, u64) = (took.parse()?, bound.parse()?);
        if took_ms >= bound_ms {
            seeded.not_samples += 1;
            continue;
        }
        samples.push(Sample {
            run: format!("seed:{file}"),
            commit: "seed".to_owned(),
            row: "-".to_owned(),
            wait: wait.to_owned(),
            took_ms,
            bound_ms,
            stall: Some(share.parse()?),
        });
    }
    seeded.imported = samples.len();
    write(ws, &samples)?;
    Ok(seeded)
}

#[derive(clap::Args)]
pub struct Args {
    /// Imports this harvest TSV as the ledger's first passes (once: refused when the ledger exists).
    #[arg(long, value_name = "TSV")]
    seed: Option<PathBuf>,
}

/// `cargo xtask gate-need`: each gate wait's samples, quiet samples, quiet passes, need (or unset) and largest took / bound. Read-only unless `--seed`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ws = atl_layout::Layout::of(root)?.workspace().to_owned();
    if let Some(tsv) = &args.seed {
        let text = fs::read_to_string(tsv).with_context(|| tsv.display().to_string())?;
        let seeded = import_harvest(&ws, &text)?;
        println!(
            "gate-need: seeded {} samples; left out {} {SHARED_EXIT} (no call site) and {} timeouts",
            seeded.imported, seeded.unattributed, seeded.not_samples
        );
    }
    let ledger = read(&ws)?;
    let quiet = ledger.samples.iter().filter(|s| s.quiet()).count();
    println!(
        "gate-need: {} samples, {quiet} quiet (stall <= {QUIET_STALL_PCT} %), {} malformed lines; {}",
        ledger.samples.len(),
        ledger.malformed,
        ledger_path(&ws).display()
    );
    println!("wait\tsamples\tquiet\tpasses\tneed_ms\tmax/bound\tbound_ms");
    for st in stats(&ledger.samples) {
        let need = st
            .need
            .map_or_else(|| "unset".to_owned(), |n| n.to_string());
        println!(
            "{}\t{}\t{}\t{}\t{need}\t{:.3}\t{}-{}",
            st.wait, st.samples, st.quiet, st.passes, st.max_ratio, st.bound_ms.0, st.bound_ms.1
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
        assert!(needs(&samples).is_empty(), "19 passes derive nothing");
        samples.push(sample("run20", "app", "marker", 2_500, Some(0.1)));
        assert_eq!(needs(&samples).get("marker"), Some(&2_500));
    }

    #[test]
    fn passes_count_runs_not_samples() {
        let samples: Vec<Sample> = (1..=20)
            .map(|i| sample("run1", "app", "wait-x", i, Some(0.1)))
            .collect();
        assert!(
            needs(&samples).is_empty(),
            "one pass of 20 samples is not 20 passes"
        );
    }

    #[test]
    fn non_quiet_passes_do_not_count_and_never_set_the_need() {
        let mut samples: Vec<Sample> = (1..=19)
            .map(|i| sample(&format!("run{i}"), "app", "marker", 100 * i, Some(0.1)))
            .collect();
        samples.push(sample("run-busy", "app", "marker", 99_000, Some(6.0)));
        assert!(
            needs(&samples).is_empty(),
            "19 quiet passes and one busy one"
        );
        samples.push(sample("run20", "app", "marker", 1_200, Some(0.2)));
        assert_eq!(needs(&samples).get("marker"), Some(&1_900));
    }

    #[test]
    fn stats_count_samples_quiet_passes_and_need() {
        let samples = vec![
            sample("r1", "app", "marker", 100, Some(0.1)),
            sample("r1", "app", "marker", 300, Some(9.0)),
            sample("r2", "app", "marker", 200, Some(0.2)),
            sample("r2", "ime", "marker", 200, Some(0.2)),
        ];
        let stats = stats(&samples);
        let marker = stats.iter().find(|s| s.wait == "marker").expect("marker");
        assert_eq!((marker.samples, marker.quiet, marker.passes), (4, 3, 3));
        assert_eq!(marker.need, None);
        assert_eq!(marker.bound_ms, (60_000, 60_000));
    }

    #[test]
    fn a_ledger_line_round_trips_and_a_torn_one_is_counted_not_read() {
        let ws = scratch("torn");
        append(
            &ws,
            &[
                sample("r1", "app", "marker", 100, Some(0.25)),
                sample("r1", "app", "pong", 5, None),
            ],
        )
        .expect("append");
        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger_path(&ws))
            .expect("open");
        file.write_all(b"r2\tc0\tapp\tmark").expect("torn line");
        let ledger = read(&ws).expect("read");
        assert_eq!(ledger.samples.len(), 2);
        assert_eq!(ledger.malformed, 1);
        assert_eq!(ledger.samples[0].stall, Some(0.25));
        assert_eq!(ledger.samples[1].stall, None);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn concurrent_appends_keep_every_line_whole() {
        let ws = scratch("concurrent");
        std::thread::scope(|scope| {
            for t in 0..8_u64 {
                let ws = ws.clone();
                scope.spawn(move || {
                    let batch: Vec<Sample> = (0..25)
                        .map(|i| {
                            sample(
                                &format!("t{t}-{i}"),
                                "app",
                                "marker",
                                t * 100 + i,
                                Some(0.1),
                            )
                        })
                        .collect();
                    append(&ws, &batch).expect("append");
                });
            }
        });
        let ledger = read(&ws).expect("read");
        assert_eq!(ledger.malformed, 0, "a torn line from two writers");
        assert_eq!(ledger.samples.len(), 200);
        for t in 0..8 {
            let mine = ledger
                .samples
                .iter()
                .filter(|s| s.run.starts_with(&format!("t{t}-")))
                .count();
            assert_eq!(mine, 25, "writer {t}");
        }
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn the_harvest_seeds_the_ledger_once_and_leaves_out_the_shared_name() {
        let ws = scratch("seed");
        let tsv = format!(
            "{HARVEST_HEADER}\n\
             out/a.log\t0.04\tconfigure-ack\t8755\t60000\tunset\n\
             out/a.log\t0.04\texit-limited\t2116\t5000\tunset\n\
             out/b.log\t0.00\tline\t5000\t5000\tunset\n\
             out/b.log\t0.00\tline\t50\t5000\tunset\n"
        );
        let seeded = import_harvest(&ws, &tsv).expect("seed");
        assert_eq!(
            seeded,
            Seeded {
                imported: 2,
                unattributed: 1,
                not_samples: 1
            }
        );
        assert!(import_harvest(&ws, &tsv).is_err(), "the seed runs once");
        let ledger = read(&ws).expect("read");
        assert_eq!(ledger.samples.len(), 2);
        assert_eq!(ledger.samples[0].run, "seed:out/a.log");
        assert_eq!(ledger.samples[0].stall, Some(0.04));
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
