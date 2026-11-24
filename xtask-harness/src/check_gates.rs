// SPDX-License-Identifier: MIT
//! Check step `gates`: runtime gates `cargo xtask check` runs on the checkout's build dir, each as `cargo xtask test` runs it (its own harness process, its own verdict line), so a regression in
//! one of them cannot land green. A row is a gate that passes on a build machine without the laptop's hardware, leaves the desktop session as it found it, needs no session PipeWire (not audio-pcm,
//! audio-app, webview) and takes under ~35 s; slower, load-flaky (widgets, dialog) and laptop-only gates stay `xtask test` runs. A host that cannot run a row's gate says so in the gate's own verdict
//! (`host lacks ...`), never by dropping the row.
//! Before the lanes the step builds every cargo product the rows run, once (gate_build.rs `ensure`, the function each gate calls itself); a product whose identity changes while the lanes run fails
//! the step, naming it and the row whose end first saw it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use xtask::build_pool::{self, Class};
use xtask::gate_need;
use xtask::harness::{Ruling, Starvation, Supply, Undecided, judged_in, undecided_verdict};
use xtask::red_logs;

use crate::gate_build::{self, Group, Moved, Snapshot};
use crate::verdict::{Outcome, UNDECIDED_WORD};
use crate::window::{PHASES_LINE, WatchEnd};

/// When a row runs: beside other rows, each under its own pool grant, or alone after the parallel lane drained (a timing assertion that other rows' load would turn undecided or red).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Parallel,
    Serial,
}

impl Lane {
    fn word(self) -> &'static str {
        match self {
            Lane::Parallel => "parallel",
            Lane::Serial => "serial",
        }
    }
}

/// One gate run: `xtask test <args>`, plus `--host <build machine>` when `host` and `--out <build dir>` when `out` (each only where the gate's own arguments have it).
/// `lane`: [`Lane`]; `after`: row names that must have ended first (the row reads what they stage); `class`: the pool class of its grant, sized from its measured `peak=` (build_pool.rs `Class::default_weight`).
pub struct Row {
    pub args: &'static [&'static str],
    pub host: bool,
    pub out: bool,
    pub lane: Lane,
    pub after: &'static [&'static str],
    pub class: Class,
    /// its waits feed the need ledger (gate_need.rs); off for the sampler's self-test, whose waits starve on purpose
    samples: bool,
}

impl Row {
    const fn new(args: &'static [&'static str], host: bool, out: bool) -> Self {
        Self {
            args,
            host,
            out,
            lane: Lane::Parallel,
            after: &[],
            class: Class::Harness,
            samples: true,
        }
    }

    const fn self_test(self) -> Self {
        Self {
            samples: false,
            ..self
        }
    }

    const fn serial(self) -> Self {
        Self {
            lane: Lane::Serial,
            ..self
        }
    }

    const fn after(self, after: &'static [&'static str]) -> Self {
        Self { after, ..self }
    }

    const fn class(self, class: Class) -> Self {
        Self { class, ..self }
    }

    /// The gate's verdict-line name (`window: ...` for every window stage).
    fn cmd(&self) -> &'static str {
        self.args[0]
    }

    /// The row's log name: its words joined (`window-ime`).
    fn name(&self) -> String {
        self.args
            .iter()
            .filter(|a| !a.starts_with("--"))
            .copied()
            .collect::<Vec<_>>()
            .join("-")
    }

    fn argv<'a>(&self, host: &'a str, out: &'a str) -> Vec<&'a str> {
        let mut argv: Vec<&str> = self.args.to_vec();
        if self.host {
            argv.extend(["--host", host]);
        }
        if self.out {
            argv.extend(["--out", out]);
        }
        argv
    }
}

/// The gates `check` runs, in report order (cheapest first). Serial rows are the timing assertions (time bounds that other rows' load would break).
pub const ROWS: &[Row] = &[
    Row::new(&["task-bind"], false, false),
    // the starvation rule's own gate: a starved wait undecided, every real red still red (about 10 s on one CPU)
    Row::new(&["host-load"], false, false).self_test(),
    Row::new(&["sandbox"], true, true),
    Row::new(&["signing"], true, true),
    Row::new(&["sepolicy"], true, true),
    Row::new(&["gl-dispatch"], false, true),
    // reads the workspace's APK cache (default APKs); a missing APK is its own red verdict
    Row::new(&["app-imports"], false, false),
    // its --out is its own output dir (default under the workspace, never the build dir); runs on the host's ICD (lavapipe on WSL)
    Row::new(&["vk-driver"], false, false),
    Row::new(&["nb-isa"], true, true),
    Row::new(&["ui-module"], false, true),
    // ping-to-pong time bounds
    Row::new(&["platform-ui"], false, false).serial(),
    Row::new(&["compositor"], true, true),
    Row::new(&["parcel"], true, true),
    Row::new(&["provider"], true, true),
    Row::new(&["system-scan"], true, true),
    Row::new(&["broadcast"], true, true),
    Row::new(&["billing"], true, true),
    Row::new(&["keystore"], true, true),
    Row::new(&["gralloc"], true, true),
    // frame-time bounds
    Row::new(&["composer"], true, true).serial(),
    Row::new(&["instrument"], true, true),
    // its tree peaks above the `harness` reserve
    Row::new(&["app"], true, true).class(Class::HarnessHeavy),
    // reads the `app/` tree the app gate stages (its refusal otherwise: "missing <out>/app/root/system/lib64/x86_64 ... run `xtask test app`")
    Row::new(&["nb-identity"], false, true).after(&["app"]),
    // needs what nb-identity needs (the staged `app/`)
    Row::new(&["sles-bridge"], false, true).after(&["app"]),
    Row::new(&["window", "--stage", "surface-view"], true, true),
    Row::new(&["window", "--stage", "ime"], true, true),
    // a GPU-starved host (window render over the hold bound) makes it `undecided`: neither pass nor fail, named on check's verdict line
    Row::new(&["window", "--stage", "slow-display"], true, true).serial(),
    // the WebView's focus and key routing without the webview stage's sound route: a mouse click focuses the page, a touch tap does not; Esc -> Back reaches the game after the dismissal
    Row::new(
        &["window", "--stage", "webview-overlay", "--esc-back"],
        true,
        true,
    ),
    Row::new(
        &[
            "window",
            "--stage",
            "webview-overlay",
            // AOSP map: with --esc-back a touch run's Escs reach BACK faster than WebKit commits goBack, so canGoBack still answers true (a provider race, not this row's subject)
            "--tap-origin",
            "touch",
        ],
        true,
        true,
    ),
    // block-time bounds
    Row::new(&["window", "--stage", "ui-block"], true, true).serial(),
    // a tiling compositor's first configure differs from the requested size: the app must lay out at the configured one
    Row::new(&["window", "--stage", "tiled"], true, true),
];

/// The name of the row a harness command line (`test <row args> ...`) runs: the row with the most matching leading arguments. None: no row's.
pub(crate) fn row_of(argv: &[String]) -> Option<String> {
    let (first, rest) = argv.split_first()?;
    if first != "test" {
        return None;
    }
    ROWS.iter()
        .filter(|r| rest.len() >= r.args.len() && rest.iter().zip(r.args).all(|(a, b)| a == b))
        .max_by_key(|r| r.args.len())
        .map(Row::name)
}

/// One row's run: its `gates:` line, its outcome, and the evidence the step's counters and ledger read. The lines stay in table order whatever ends first.
struct Done {
    outcome: Outcome,
    took: f64,
    /// the `gates:` line less its lane fields ([`run_lanes`] adds them)
    line: String,
    /// `<row> (log <path>): <verdict>` when red; empty when undecided (`verdict` names it)
    note: String,
    /// what ended a window row's watch
    end: Option<String>,
    /// rows holding their grant when this one was admitted, itself included (0: never admitted)
    running: usize,
    /// the grant class the run held (`quiet` = the whole pool); `-` when it never got one
    grant: &'static str,
    /// host stall share of the run's span, percent (`host load:`); None when unknown
    stall: Option<f64>,
    /// an undecided run's verdict (`undecided_verdict`); None unless undecided
    verdict: Option<Ruling>,
    /// judged late waits in the run's log
    judged: usize,
    /// the run's host-load record is unknown
    host_unknown: bool,
    /// the attempt an undecided row was rerun after: this run's outcome is the row's, that attempt is the first
    first: Option<Box<Done>>,
    /// this run's wait samples (gate_need.rs); kept in the need ledger when the row passes
    waits: Vec<gate_need::WaitLine>,
}

impl Done {
    /// A run with no measurements: a row whose process could not start, or a test fake.
    fn bare(outcome: Outcome, line: String, note: String) -> Self {
        Self {
            outcome,
            took: 0.0,
            line,
            note,
            end: None,
            running: 0,
            grant: "-",
            stall: None,
            verdict: None,
            judged: 0,
            host_unknown: false,
            first: None,
            waits: Vec::new(),
        }
    }

    /// This run, then the attempt it was rerun after.
    fn attempts(&self) -> impl Iterator<Item = &Done> {
        std::iter::once(self).chain(self.first.as_deref())
    }

    /// The supply this run's verdict names as starved (None: no verdict, or one that names none).
    fn supply(&self) -> Option<Supply> {
        self.verdict.as_ref().and_then(Ruling::supply)
    }
}

/// A row's attempt: its first run, or the rerun after an undecided first (`starved`: the supply that first verdict named).
#[derive(Clone, Copy)]
enum Attempt {
    First,
    Rerun { starved: Option<Supply> },
}

impl Attempt {
    /// The row's log in `check-gates/`: the rerun writes its own file, so a kept first log is never overwritten.
    fn log_name(self, row: &Row) -> String {
        match self {
            Attempt::First => format!("{}.log", row.name()),
            Attempt::Rerun { .. } => format!("{}.rerun.log", row.name()),
        }
    }

    /// The lead of the row line's field group: empty for a first run; a rerun's says what the first attempt was and what the rerun ended as (`starved, rerun ok; `, `undecided, rerun ok; ` when the first verdict named no supply).
    fn lead(self, outcome: Outcome) -> String {
        let Attempt::Rerun { starved } = self else {
            return String::new();
        };
        let first = starved.map_or(UNDECIDED, |_| "starved");
        let ended = match outcome {
            Outcome::Pass => "ok",
            Outcome::Fail => "red",
            Outcome::Undecided => UNDECIDED,
        };
        format!("{first}, rerun {ended}; ")
    }
}

/// An undecided outcome's word in a row line.
const UNDECIDED: &str = "undecided";

/// Reruns each undecided row once: `rerun(i, first)` runs row `i` alone and its outcome is the row's; the first attempt moves into `first`. A red or passing row is never rerun.
fn settle(done: &mut [Done], rerun: &mut dyn FnMut(usize, &Done) -> Done) {
    for (i, d) in done.iter_mut().enumerate() {
        if d.outcome != Outcome::Undecided {
            continue;
        }
        let again = rerun(i, d);
        let first = std::mem::replace(d, again);
        d.first = Some(Box::new(first));
    }
}

/// The starvation counters over a step's rows: first attempts that ended undecided (by supply), their reruns (by grant and outcome), and every attempt's judged waits and unknown host loads.
fn starvation_of(done: &[Done]) -> Starvation {
    let mut counts = Starvation::default();
    for d in done {
        for a in d.attempts() {
            counts.record_attempt(a.judged, a.host_unknown);
        }
        if let Some(first) = d.first.as_deref() {
            counts.record_starved(first.supply());
            counts.record_rerun(d.grant, d.outcome);
        }
    }
    counts
}

/// The need-ledger samples of one step: each passing row's wait samples from its deciding run (the rerun, when there was one). A pass is one `run` of a row; the self-test row's waits are not gate waits.
fn need_samples(run: &str, commit: &str, rows: &[(&Row, &Done)]) -> Vec<gate_need::Sample> {
    rows.iter()
        .filter(|(row, d)| row.samples && d.outcome == Outcome::Pass)
        .flat_map(|(row, d)| {
            d.waits.iter().map(move |w| gate_need::Sample {
                run: run.to_owned(),
                commit: commit.to_owned(),
                row: row.name(),
                wait: w.name.clone(),
                took_ms: w.took_ms,
                bound_ms: w.bound_ms,
                stall: d.stall,
            })
        })
        .collect()
}

/// The failure note of a row undecided in [`red_logs::CEILING`] consecutive runs of this checkout, its last runs named. None below the ceiling.
fn ceiling_note(streaks: &BTreeMap<String, Vec<String>>, row: &str) -> Option<String> {
    let runs = streaks
        .get(row)
        .filter(|runs| runs.len() >= red_logs::CEILING)?;
    let last = &runs[runs.len() - red_logs::CEILING..];
    Some(format!(
        "{row} undecided in {} consecutive runs of this checkout ({}): the ceiling fails the step",
        red_logs::CEILING,
        last.join(", ")
    ))
}

/// What a gates step's rows ended as: the failure notes (red rows, rows at the ceiling, a ledger that could not be kept), the rows left undecided, the starvation counters.
struct Judged {
    failed: Vec<String>,
    undecided: Vec<Undecided>,
    starvation: Starvation,
}

/// Ledgers the rows' outcomes (`task`, red_logs.rs), then judges them with the ledger's streaks. A ledger error fails the step after its rows' notes, never in their place.
fn judge_rows(task: &Path, run: &str, commit: &str, names: &[String], done: &[Done]) -> Judged {
    let entries: Vec<red_logs::Entry<'_>> = done
        .iter()
        .zip(names)
        .map(|(d, name)| red_logs::Entry {
            run,
            commit,
            row: name.as_str(),
            outcome: d.outcome,
            first: d.first.as_deref().map(|f| f.outcome),
            stall: d.stall,
            grant: d.grant,
            supply: d.first.as_deref().and_then(Done::supply),
        })
        .collect();
    let ledger =
        red_logs::ledger_append(task, &entries).and_then(|()| red_logs::trailing_undecided(task));
    let (trailing, broken) = match ledger {
        Ok(trailing) => (trailing, None),
        Err(e) => (
            red_logs::Trailing::default(),
            Some(format!("outcome ledger: {e:#}")),
        ),
    };
    eprintln!(
        "gates: outcome ledger malformed lines {}",
        trailing.malformed
    );
    let mut starvation = starvation_of(done);
    let (mut failed, mut undecided) = (Vec::new(), Vec::new());
    for (name, d) in names.iter().zip(done) {
        match d.outcome {
            Outcome::Pass => {}
            // the path leads: check quotes its first error at 160 chars (verdict.rs FIRST_ERROR_CHARS), and the verdict line lands in it
            Outcome::Fail => failed.push(d.note.clone()),
            Outcome::Undecided => match ceiling_note(&trailing.streaks, name) {
                Some(note) => {
                    starvation.ceiling += 1;
                    failed.push(note);
                }
                None => undecided.push(Undecided {
                    row: name.clone(),
                    ruling: d.verdict.clone(),
                }),
            },
        }
    }
    failed.extend(broken);
    Judged {
        failed,
        undecided,
        starvation,
    }
}

/// A row whose process could not start (a pool or spawn error): its red verdict, so the other rows still report.
fn errored(row: &Row, e: &anyhow::Error) -> Done {
    let line = format!("error: {e:#}");
    Done::bare(
        Outcome::Fail,
        format!("gates: {} FAILED (0.0s) {line}", row.name()),
        format!("{}: {line}", row.name()),
    )
}

/// Rows admitted (grant held) at the moment, and the most at once: a row enters after its grant, so rows queued for the pool are not counted.
#[derive(Default)]
struct Admitted {
    live: AtomicUsize,
    max: AtomicUsize,
}

impl Admitted {
    fn enter(&self) -> Entered<'_> {
        let count = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(count, Ordering::SeqCst);
        Entered { count, of: self }
    }
}

/// One admitted row; leaves on drop.
struct Entered<'a> {
    count: usize,
    of: &'a Admitted,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.of.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs every row on build dir `out` for host `host`, logs under `<out>/check-gates/`; fails naming each red gate's verdict line. Ok: the undecided rows, neither pass nor fail ([`Outcome`]).
/// Each row takes its own pool grant (memory admission decides how many run at once); no grant is held around the step ([`run_lanes`]).
pub fn run(root: &Path, host: &str, out: &Path) -> Result<Vec<Undecided>> {
    let exe = std::env::current_exe().context("the harness's own path")?;
    let ws = Layout::of(root)?.workspace().to_owned();
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let out_arg = out.to_str().context("build dir path is not UTF-8")?;
    // every cargo product a row runs, built and staged once before any row starts (gate_build.rs), under a cargo grant released before the rows take theirs; rows then find them current
    let (products, waited, took) = {
        let grant = build_pool::acquire_quiet_stdout(&ws, root, Class::Cargo)?;
        let started = Instant::now();
        let files = gate_build::ensure(root, out, &Group::ALL)?.files();
        (files, grant.waited, started.elapsed())
    };
    let snapshot = Snapshot::take(&products);
    eprintln!(
        "gates: prebuild {:.1}s (pool wait {:.1}s, {} products)",
        took.as_secs_f64(),
        waited.as_secs_f64(),
        products.len()
    );
    let moved = Mutex::new(Moved::default());
    let run = red_logs::run_id();
    let task = red_logs::task_dir(root)?;
    let commit = xtask::git::git(Some(root), &["rev-parse", "HEAD"])?;
    let at = RowRun {
        exe: &exe,
        root,
        logs: &logs,
        task: &task,
        run: &run,
        host,
        out: out_arg,
    };
    // after a row's process ended: a product it (or a row beside it) re-linked or re-staged is named now, with this row
    let note_moved = |row: &Row| {
        moved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .note(&snapshot, Some(&row.name()));
    };
    let run_one = |row: &Row, admitted: &Admitted| -> Done {
        // the grant lives on this thread until the row's process ended; a pool or spawn error is the row's red verdict, so the other rows still report
        let gated = build_pool::acquire_quiet_stdout(&ws, root, row.class).and_then(|grant| {
            let entered = admitted.enter();
            let mut done = at.row(row, Attempt::First)?;
            done.running = entered.count;
            done.grant = grant.class;
            Ok((done, grant))
        });
        note_moved(row);
        gated
            .map(|(done, _grant)| done)
            .unwrap_or_else(|e| errored(row, &e))
    };
    // an undecided row's rerun: alone after the lanes, under the pool's quiet grant when it is idle, else the row's class
    let rerun = |row: &Row, first: &Done| -> Done {
        let gated = build_pool::acquire_rerun(&ws, root, row.class).and_then(|grant| {
            let starved = first.supply();
            let mut done = at.row(row, Attempt::Rerun { starved })?;
            done.running = 1;
            done.grant = grant.class;
            Ok((done, grant))
        });
        note_moved(row);
        let mut done = gated
            .map(|(done, _grant)| done)
            .unwrap_or_else(|e| errored(row, &e));
        done.line = with_lane(&done.line, Lane::Serial, done.running);
        eprintln!("{}", done.line);
        done
    };
    let (stats, mut done) = run_lanes(ROWS, &run_one, &mut |first| eprintln!("{}", first.line));
    settle(&mut done, &mut |i, first| rerun(&ROWS[i], first));
    let names: Vec<String> = ROWS.iter().map(Row::name).collect();
    let Judged {
        mut failed,
        undecided,
        starvation,
    } = judge_rows(&task, &run, commit.trim(), &names, &done);
    // the need ledger (gate_need.rs): every passing row's wait samples, machine-wide, after the step's rows ended
    let pairs: Vec<(&Row, &Done)> = ROWS.iter().zip(&done).collect();
    let samples = need_samples(&run, commit.trim(), &pairs);
    match gate_need::append(&ws, &samples) {
        Ok(()) => eprintln!("gates: need ledger +{} samples", samples.len()),
        Err(e) => failed.push(format!("need ledger: {e:#}")),
    }
    let mut ends = WatchTally::default();
    for d in &done {
        for a in d.attempts() {
            ends.count(a.end.as_deref());
        }
    }
    eprintln!("gates: watch ends {}", ends.line());
    eprintln!("gates: {}", stats.line());
    eprintln!("{}", starvation.line("gates"));
    let mut moved = moved.into_inner().unwrap_or_else(|p| p.into_inner());
    moved.note(&snapshot, None);
    let moved = moved.lines();
    eprintln!("gates: products moved during the lanes: {}", moved.len());
    for line in &moved {
        eprintln!("{line}");
    }
    // a moved product fails the step as a red row does: a running row may have bound or mapped the file it replaced
    failed.extend(moved);
    if failed.is_empty() {
        return Ok(undecided);
    }
    let open = if undecided.is_empty() {
        String::new()
    } else {
        let notes: Vec<String> = undecided.iter().map(ToString::to_string).collect();
        format!(" ({} undecided: {})", undecided.len(), notes.join("; "))
    };
    let summary = format!(
        "{} of {} gates failed{open}: {}",
        failed.len(),
        ROWS.len(),
        failed.join("; ")
    );
    // stdout leads the step's failure text (xtask harness.rs check_step), so check's first-error line is this one, not the first gate's progress line
    println!("gates: FAILED {summary}");
    bail!("{summary}")
}

/// Wall time of a gates step against the sum of its rows' times, and the widest the parallel lane ran.
#[derive(Debug, Default)]
struct LaneStats {
    wall: f64,
    sum: f64,
    parallel: usize,
    serial: usize,
    max_at_once: usize,
}

impl LaneStats {
    fn line(&self) -> String {
        format!(
            "lanes wall {:.1}s, row sum {:.1}s (parallel {} rows, max {} at once; serial {} rows)",
            self.wall, self.sum, self.parallel, self.max_at_once, self.serial
        )
    }
}

/// `line` with `lane=<lane> running=<k>` added at the end of its first parenthesised field group (`gates: <row> ok (<s> ... lane=parallel running=3) ...`).
fn with_lane(line: &str, lane: Lane, running: usize) -> String {
    let field = format!(" lane={} running={running}", lane.word());
    match line
        .find(" (")
        .and_then(|open| line[open..].find(')').map(|close| open + close))
    {
        Some(close) => format!("{}{field}{}", &line[..close], &line[close..]),
        None => format!("{line}{field}"),
    }
}

/// Runs `rows` with `run_one` (which takes the row's grant itself, then enters `Admitted`): parallel rows concurrently, each once its `after` rows ended, then serial rows one at a time, alone.
/// `emit` gets each result in table order as soon as every earlier row has one; each line gains `lane=` and `running=` ([`Done::running`]). Returns the stats and every result, in table order.
fn run_lanes(
    rows: &[Row],
    run_one: &(dyn Fn(&Row, &Admitted) -> Done + Sync),
    emit: &mut dyn FnMut(&Done),
) -> (LaneStats, Vec<Done>) {
    let started = Instant::now();
    let names: Vec<String> = rows.iter().map(Row::name).collect();
    let ended = Mutex::new(vec![false; rows.len()]);
    let wake = Condvar::new();
    let admitted = Admitted::default();
    let start_one = |i: usize| -> Done {
        let mut done = run_one(&rows[i], &admitted);
        done.line = with_lane(&done.line, rows[i].lane, done.running);
        done
    };
    let mut results: Vec<Option<Done>> = (0..rows.len()).map(|_| None).collect();
    let mut next = 0;
    let mut flush = |results: &[Option<Done>], next: &mut usize| {
        while let Some(Some(done)) = results.get(*next) {
            emit(done);
            *next += 1;
        }
    };
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        for (i, row) in rows.iter().enumerate() {
            if row.lane != Lane::Parallel {
                continue;
            }
            let tx = tx.clone();
            let (ended, wake, names, start_one) = (&ended, &wake, &names, &start_one);
            scope.spawn(move || {
                // `after` names are checked against the table by a test (`after_names_earlier_parallel_rows`)
                let deps: Vec<usize> = row
                    .after
                    .iter()
                    .filter_map(|a| names.iter().position(|n| n == a))
                    .collect();
                let mut e = ended.lock().unwrap_or_else(|p| p.into_inner());
                while deps.iter().any(|&d| !e[d]) {
                    e = wake.wait(e).unwrap_or_else(|p| p.into_inner());
                }
                drop(e);
                let done = start_one(i);
                ended.lock().unwrap_or_else(|p| p.into_inner())[i] = true;
                wake.notify_all();
                let _ = tx.send((i, done));
            });
        }
        drop(tx);
        for (i, done) in rx {
            results[i] = Some(done);
            flush(&results, &mut next);
        }
    });
    for (i, row) in rows.iter().enumerate() {
        if row.lane == Lane::Serial {
            results[i] = Some(start_one(i));
            flush(&results, &mut next);
        }
    }
    let stats = LaneStats {
        wall: started.elapsed().as_secs_f64(),
        sum: results.iter().flatten().map(|d| d.took).sum(),
        parallel: rows.iter().filter(|r| r.lane == Lane::Parallel).count(),
        serial: rows.iter().filter(|r| r.lane == Lane::Serial).count(),
        max_at_once: admitted.max.load(Ordering::SeqCst),
    };
    (stats, results.into_iter().flatten().collect())
}

/// What every row's run shares: the harness binary, the checkout, the log dir, the red-log task dir and run id, the host and build dir arguments.
struct RowRun<'a> {
    exe: &'a Path,
    root: &'a Path,
    logs: &'a Path,
    task: &'a Path,
    run: &'a str,
    host: &'a str,
    out: &'a str,
}

impl RowRun<'_> {
    /// One row as its own `xtask test` process, its output in `<logs>/<row>.log` (a rerun's in `<row>.rerun.log`); a red or undecided log is kept ([`keep_unless_pass`]).
    /// Called on the thread holding the row's grant: its job count reaches the gate's cargo runs (xtask `grant_jobs`).
    fn row(&self, row: &Row, attempt: Attempt) -> Result<Done> {
        let log = self.logs.join(attempt.log_name(row));
        let file = std::fs::File::create(&log).with_context(|| log.display().to_string())?;
        let argv = row.argv(self.host, self.out);
        let started = Instant::now();
        let mut cmd = Command::new(self.exe);
        xtask::grant_jobs(&mut cmd);
        let mut child = cmd
            .current_dir(self.root)
            .arg("test")
            .args(&argv)
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .with_context(|| format!("spawning xtask-harness test {}", argv.join(" ")))?;
        let status = child.wait().context("waiting for the gate")?;
        let took = started.elapsed().as_secs_f64();
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let phases = phases_field(&text);
        // the gate's own record (atl-host-load), its last `host load:` line: the scope's peak and the stall share of its span
        let seen = atl_host_load::Seen::read(&text);
        let measured = format!(
            "peak={}{} work={} stall={}",
            seen.peak_mib
                .map_or("unknown".to_owned(), |m| format!("{m}MiB")),
            phases
                .as_ref()
                .map(|p| format!(" {}", p.field))
                .unwrap_or_default(),
            runtime_work(&text).unwrap_or("-"),
            seen.stall_pct
                .map_or("unknown".to_owned(), |p| format!("{p:.0}%")),
        );
        let outcome = Outcome::of_exit(status.code());
        // the undecided verdict's ruling, typed: the gate printed it as a `starvation-verdict:` line (harness `undecided_verdict`)
        let verdict = match outcome {
            Outcome::Undecided => undecided_verdict(&text)?,
            Outcome::Pass | Outcome::Fail => None,
        };
        // the gate's own verdict line, shown as printed
        let shown = match outcome {
            Outcome::Undecided => undecided_line(&text, row.cmd()),
            Outcome::Pass | Outcome::Fail => verdict_line(&text, row.cmd()),
        }
        .unwrap_or("no verdict line");
        let kept = keep_unless_pass(self.task, self.run, row, &log, outcome);
        let at = kept.as_ref().unwrap_or(&log);
        let kept_note = kept
            .as_ref()
            .map(|k| format!(" (log {})", k.display()))
            .unwrap_or_default();
        let word = match outcome {
            Outcome::Pass => "ok",
            Outcome::Fail => "FAILED",
            Outcome::Undecided => UNDECIDED,
        };
        let lead = attempt.lead(outcome);
        let line = format!(
            "gates: {} {word} ({lead}{took:.1}s {measured}) {shown}{kept_note}",
            row.name()
        );
        let note = match outcome {
            Outcome::Undecided => String::new(),
            Outcome::Pass | Outcome::Fail => {
                format!("{} (log {}): {shown}", row.name(), at.display())
            }
        };
        Ok(Done {
            outcome,
            took,
            line,
            note,
            end: phases.map(|p| p.end),
            running: 0,
            grant: "-",
            stall: seen.stall_pct,
            verdict,
            judged: judged_in(&text)?,
            host_unknown: seen.stall_pct.is_none(),
            first: None,
            waits: gate_need::wait_samples(&text),
        })
    }
}

/// A window row's phases (window.rs `PHASES_LINE`): the `key=value` fields for the gates line, and the word naming what ended its watch.
struct Phases {
    field: String,
    end: String,
}

/// The last phases line in a gate's log, without its head and its parenthesised note.
fn phases_field(text: &str) -> Option<Phases> {
    let line = text.lines().rev().find(|l| l.starts_with(PHASES_LINE))?;
    let field = line[PHASES_LINE.len()..]
        .split(" (")
        .next()?
        .trim()
        .to_owned();
    let end = field
        .split_whitespace()
        .find_map(|f| f.strip_prefix("end="))?
        .to_owned();
    Some(Phases { field, end })
}

/// What ended each window row's watch, over one gates step: a path whose count stays 0 is visible on `gates: watch ends`.
#[derive(Default)]
struct WatchTally {
    counts: [usize; WatchEnd::ALL.len()],
    unknown: usize,
}

impl WatchTally {
    fn count(&mut self, end: Option<&str>) {
        let Some(end) = end else { return };
        match WatchEnd::ALL.iter().position(|w| w.word() == end) {
            Some(i) => self.counts[i] += 1,
            None => self.unknown += 1,
        }
    }

    fn line(&self) -> String {
        let mut parts: Vec<String> = WatchEnd::ALL
            .iter()
            .zip(self.counts)
            .map(|(w, n)| format!("{}={n}", w.word()))
            .collect();
        parts.push(format!("unknown={}", self.unknown));
        parts.join(" ")
    }
}

/// A red or undecided gate's log is kept under the checkout's task dir (`task`, red_logs::task_dir) in its bucket, so the next check's overwrite of `check-gates/<gate>.log` cannot reach it. A pass keeps nothing. A failed copy names the log where it stands, as a note.
fn keep_unless_pass(
    task: &Path,
    run: &str,
    row: &Row,
    log: &Path,
    outcome: Outcome,
) -> Option<PathBuf> {
    let bucket = red_logs::bucket_of(outcome)?;
    Some(red_logs::keep(task, bucket, run, log).unwrap_or_else(|e| {
        eprintln!("gates: {} log not kept: {e:#}", row.name());
        log.to_path_buf()
    }))
}

/// The work dir a row's runtime staged under: its `runtime: work=<key>` line (atld_run.rs `Runtime::new`). None for a row that runs no atld runtime.
fn runtime_work(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("runtime: work="))
        .map(str::trim)
}

/// An undecided gate's own verdict line (`<cmd>: undecided <stage>: <why>`, window.rs `judge`), echoed on the gates line. The typed verdict is `undecided_verdict`.
fn undecided_line<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let prefix = format!("{cmd}: {UNDECIDED_WORD}");
    text.lines().rev().find(|l| l.starts_with(&prefix))
}

/// The gate's verdict among its `<cmd>: ` lines (a gate also prints counters and log tails under its name): its `FAILED` line, else the last pass / ok line.
pub(crate) fn verdict_line<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let prefix = format!("{cmd}: ");
    let named = || text.lines().filter(|l| l.starts_with(&prefix));
    named().find(|l| l.contains("FAILED")).or_else(|| {
        named().rev().find(|l| {
            let rest = &l[prefix.len()..];
            // `PASS`: signing and task-bind print one per check, no summary line
            ["ok", "pass", "PASS", "all checks"]
                .iter()
                .any(|p| rest.starts_with(p))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::time::Duration;

    /// A step's need samples: each passing row's own waits, from its deciding run; a red row and the self-test row add none. Expected values written out.
    #[test]
    fn need_samples_are_the_passing_rows_waits_and_the_self_test_is_not_one() {
        let wait = |name: &str, took_ms: u64| gate_need::WaitLine {
            name: name.to_owned(),
            took_ms,
            bound_ms: 60_000,
        };
        let step = |outcome: Outcome, stall: Option<f64>, waits: Vec<gate_need::WaitLine>| Done {
            stall,
            waits,
            ..Done::bare(outcome, String::new(), String::new())
        };
        let app = Row::new(&["app"], true, true);
        let ime = Row::new(&["ime"], true, true);
        let self_test = Row::new(&["host-load"], false, false).self_test();
        let passed = step(
            Outcome::Pass,
            Some(0.04),
            vec![wait("app-progress", 521), wait("marker", 11_773)],
        );
        let red = step(Outcome::Fail, Some(0.0), vec![wait("marker", 900)]);
        let quiet_self_test = step(Outcome::Pass, Some(0.0), vec![wait("draw", 3_100)]);
        let pairs = [
            (&app, &passed),
            (&ime, &red),
            (&self_test, &quiet_self_test),
        ];
        let samples = need_samples("20261124-120000-000", "c0ffee", &pairs);
        let got: Vec<(&str, &str, &str, u64, Option<f64>)> = samples
            .iter()
            .map(|s| {
                (
                    s.run.as_str(),
                    s.row.as_str(),
                    s.wait.as_str(),
                    s.took_ms,
                    s.stall,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "20261124-120000-000",
                    "app",
                    "app-progress",
                    521,
                    Some(0.04)
                ),
                ("20261124-120000-000", "app", "marker", 11_773, Some(0.04)),
            ]
        );
    }

    /// Lines as the window and composer gates printed them (job rgm-done1).
    #[test]
    fn the_verdict_not_the_last_named_line() {
        let pass = "window: pass ime (`AtlWindow: onResume reached` logged 11521 ms)\nwindow: +227 ms after onResume: unimplemented IWindowSession.x (total 7)\n";
        assert_eq!(
            verdict_line(pass, "window"),
            Some("window: pass ime (`AtlWindow: onResume reached` logged 11521 ms)")
        );
        let red = "composer: FAIL quads readable: 0 quads\ncomposer: FAILED: quads readable; first error\ncomposer: services counted x\n";
        assert_eq!(
            verdict_line(red, "composer"),
            Some("composer: FAILED: quads readable; first error")
        );
        assert_eq!(verdict_line("other: ok\n", "composer"), None);
        let signing = "signing: PASS framework-res: platform role\nsigning: PASS AtlSigningFixture: testkey role\n";
        assert_eq!(
            verdict_line(signing, "signing"),
            Some("signing: PASS AtlSigningFixture: testkey role")
        );
    }

    /// Every row is a command line `xtask test` accepts, as `run` builds it: a renamed gate, stage or flag, or a host/out the gate does not take, fails here rather than in a check run.
    #[test]
    fn every_row_parses_as_xtask_test() {
        for row in ROWS {
            let mut argv = vec!["xtask", "test"];
            argv.extend(row.argv("x86_64", "/tmp/out"));
            if let Err(e) = crate::Cli::try_parse_from(&argv) {
                panic!("{}: {e}", argv.join(" "));
            }
        }
    }

    /// Every `after` name is a parallel row earlier in the table: a misspelt name would be dropped silently and the row would run before what it reads.
    #[test]
    fn after_names_earlier_parallel_rows() {
        for (i, row) in ROWS.iter().enumerate() {
            for a in row.after {
                let at = ROWS.iter().position(|r| r.name() == *a);
                assert!(
                    at.is_some_and(|at| at < i && ROWS[at].lane == Lane::Parallel),
                    "{}: after {a}",
                    row.name()
                );
            }
        }
    }

    /// A fake row set through the scheduler: the ordering edge holds, the serial row runs alone after the parallel lane, the lines come in table order, a red row stays red.
    #[test]
    fn lanes_order_edges_and_report_in_table_order() {
        const FAKE: &[Row] = &[
            Row::new(&["slow-stager"], false, false),
            Row::new(&["reader"], false, false).after(&["slow-stager"]),
            Row::new(&["timed"], false, false).serial(),
            Row::new(&["red"], false, false),
        ];
        let t0 = Instant::now();
        let spans = Mutex::new(Vec::new());
        let run_one = |row: &Row, admitted: &Admitted| -> Done {
            let entered = admitted.enter();
            let from = t0.elapsed();
            // the stager is slowest, so a reader that ignored its edge would start first
            std::thread::sleep(Duration::from_millis(if row.name() == "slow-stager" {
                150
            } else {
                30
            }));
            spans
                .lock()
                .expect("spans")
                .push((row.name(), from, t0.elapsed()));
            let outcome = if row.name() == "red" {
                Outcome::Fail
            } else {
                Outcome::Pass
            };
            Done {
                took: 0.1,
                running: entered.count,
                ..Done::bare(
                    outcome,
                    format!("gates: {} x (0.1s peak=0MiB) v", row.name()),
                    row.name(),
                )
            }
        };
        let mut seen = Vec::new();
        let (stats, _) = run_lanes(FAKE, &run_one, &mut |d| {
            seen.push((d.line.clone(), d.outcome))
        });
        let names: Vec<String> = seen
            .iter()
            .map(|(l, _)| l.split_whitespace().nth(1).expect("name").to_owned())
            .collect();
        assert_eq!(names, ["slow-stager", "reader", "timed", "red"]);
        assert_eq!(seen[3].1, Outcome::Fail);
        assert!(
            seen[2].0.contains("lane=serial running=1)"),
            "{}",
            seen[2].0
        );
        assert!(
            seen[0].0.contains("lane=parallel running="),
            "{}",
            seen[0].0
        );
        let spans = spans.into_inner().expect("spans");
        let span = |n: &str| {
            let s = spans.iter().find(|s| s.0 == n).expect("span");
            (s.1, s.2)
        };
        assert!(
            span("reader").0 >= span("slow-stager").1,
            "reader ran before its stager"
        );
        let timed = span("timed");
        for other in ["slow-stager", "reader", "red"] {
            assert!(timed.0 >= span(other).1, "timed overlapped {other}");
        }
        assert_eq!((stats.parallel, stats.serial), (3, 1));
        assert!(
            stats.max_at_once >= 2,
            "parallel rows did not overlap: {stats:?}"
        );
        assert!((stats.sum - 0.4).abs() < 1e-9);
    }

    #[test]
    fn lane_fields_close_the_first_group() {
        assert_eq!(
            with_lane(
                "gates: a ok (1.0s peak=3MiB work=-) a: ok (log x)",
                Lane::Parallel,
                2
            ),
            "gates: a ok (1.0s peak=3MiB work=- lane=parallel running=2) a: ok (log x)"
        );
        assert_eq!(
            with_lane("odd", Lane::Serial, 1),
            "odd lane=serial running=1"
        );
    }

    /// An undecided run's note for check's line is the row name and its verdict's ruling, read from the gate's `starvation-verdict:` line; the verdict line beside it is the echo only.
    #[test]
    fn undecided_names_row_and_its_ruling() {
        let ruling = Ruling::starved(Supply::Gpu, "render max 205 ms > 100 ms");
        let text = format!(
            "window: held: undecided host GPU starved: x\nwindow: undecided slow-display: {ruling}\n{}{}\n",
            xtask::harness::VERDICT_PREFIX,
            serde_json::to_string(&ruling).expect("json")
        );
        let row = Row::new(&["window", "--stage", "slow-display"], true, true);
        let note = Undecided {
            row: row.name(),
            ruling: undecided_verdict(&text).expect("readable"),
        };
        assert_eq!(
            note.to_string(),
            "window-slow-display host GPU starved (render max 205 ms > 100 ms)"
        );
        assert_eq!(
            undecided_line(&text, "window"),
            Some("window: undecided slow-display: host GPU starved (render max 205 ms > 100 ms)")
        );
        assert_eq!(
            undecided_verdict("window: pass x\n").expect("readable"),
            None
        );
        assert_eq!(undecided_line("window: pass x\n", "window"), None);
    }

    /// The phases field as window.rs prints it, and the tally of what ended each watch; a word the tally does not know is counted, not dropped.
    #[test]
    fn phases_and_watch_tally() {
        let text = "window: pass x\nwindow: phases start->marker=12000 watch=4100 teardown=900 end=probe (watch cap 10000 ms, probe bounds the watch)\n";
        let p = phases_field(text).expect("phases");
        assert_eq!(
            p.field,
            "start->marker=12000 watch=4100 teardown=900 end=probe"
        );
        let mut t = WatchTally::default();
        t.count(Some(&p.end));
        for text in [
            "window: phases watch=1 end=window",
            "window: phases watch=1 end=odd",
            "sandbox: ok",
        ] {
            t.count(phases_field(text).map(|p| p.end).as_deref());
        }
        assert_eq!(t.line(), "probe=1 window=1 ended=0 limit=0 unknown=1");
    }

    /// A row's own command line names it (the need lookup key is the ledger key); any other command names none.
    #[test]
    fn a_rows_command_line_names_its_row() {
        for row in ROWS {
            let mut argv = vec!["test".to_owned()];
            argv.extend(row.argv("h", "o").into_iter().map(str::to_owned));
            assert_eq!(row_of(&argv), Some(row.name()));
        }
        let others: [&[&str]; 3] = [
            &["test", "window", "--stage", "draw"],
            &["run-apk", "app"],
            &[],
        ];
        for other in others {
            let argv: Vec<String> = other.iter().map(|&a| a.to_owned()).collect();
            assert_eq!(row_of(&argv), None, "{other:?}");
        }
    }

    #[test]
    fn row_names_and_argv() {
        let ime = Row::new(&["window", "--stage", "ime"], true, true);
        assert_eq!((ime.cmd(), ime.name().as_str()), ("window", "window-ime"));
        assert_eq!(
            ime.argv("h", "o"),
            ["window", "--stage", "ime", "--host", "h", "--out", "o"]
        );
        assert_eq!(
            Row::new(&["platform-ui"], false, false).argv("h", "o"),
            ["platform-ui"]
        );
    }

    /// A window run's argv parsed as `window::Args`: the `window` word is the bin name, as in `xtask test window ...`.
    #[derive(clap::Parser)]
    struct WindowRow {
        #[command(flatten)]
        args: crate::window::Args,
    }

    fn window_key(tail: &[&str]) -> String {
        let argv: Vec<&str> = ["window", "--host", "x86_64", "--out", "/tmp/out"]
            .into_iter()
            .chain(tail.iter().copied())
            .collect();
        let row = WindowRow::try_parse_from(argv).unwrap_or_else(|e| panic!("{e}"));
        crate::window::work_key(&row.args)
    }

    /// Each window row's work key is its row name, and no two rows share one: rows run side by side only in distinct dirs.
    #[test]
    fn window_rows_name_their_own_work_dirs() {
        let rows: Vec<&Row> = ROWS.iter().filter(|r| r.cmd() == "window").collect();
        let mut keys = std::collections::BTreeSet::new();
        for row in &rows {
            let argv = row.argv("x86_64", "/tmp/out");
            let parsed =
                WindowRow::try_parse_from(argv).unwrap_or_else(|e| panic!("{}: {e}", row.name()));
            let key = crate::window::work_key(&parsed.args);
            assert_eq!(key, row.name());
            assert!(keys.insert(key), "{}: shares its work dir", row.name());
        }
        assert_eq!(keys.len(), rows.len());
    }

    /// A tap origin other than the mouse default and a hold each change the key; `--esc-back` does not, as in `Row::name`.
    #[test]
    fn work_keys_name_tap_origin_and_hold() {
        assert_eq!(
            window_key(&["--stage", "webview-overlay"]),
            "window-webview-overlay"
        );
        assert_eq!(
            window_key(&["--stage", "webview-overlay", "--esc-back"]),
            "window-webview-overlay"
        );
        assert_eq!(
            window_key(&["--stage", "webview-overlay", "--tap-origin", "mouse-finger"]),
            "window-webview-overlay-mouse-finger"
        );
        assert_eq!(
            window_key(&["--stage", "gtk", "--hold", "5"]),
            "window-gtk-hold"
        );
    }

    #[test]
    fn runtime_work_reads_the_runtime_line() {
        assert_eq!(
            runtime_work("runtime: host art 1.2s\nruntime: work=window-ime\nwindow: pass\n"),
            Some("window-ime")
        );
        assert_eq!(runtime_work("sandbox: ok\n"), None);
    }

    /// The red log outlives the next check's overwrite of the gate's log; an undecided one goes to its own bucket; a pass keeps nothing.
    #[test]
    fn red_and_undecided_logs_are_kept_and_a_pass_is_not() {
        let out = std::env::temp_dir().join(format!("xtask-gates-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let logs = out.join("check-gates");
        std::fs::create_dir_all(&logs).expect("logs");
        let row = Row::new(&["window", "--stage", "ime"], true, true);
        let log = logs.join("window-ime.log");
        std::fs::write(&log, "window: FAILED ime: copy lost\n").expect("red log");
        let kept = keep_unless_pass(&out, "20261122-100000-000", &row, &log, Outcome::Fail)
            .expect("a red log is kept");
        assert_eq!(
            kept,
            out.join(red_logs::RED)
                .join("20261122-100000-000")
                .join("window-ime.log")
        );
        // the next check passes and overwrites the gate's log
        std::fs::write(&log, "window: pass ime\n").expect("pass log");
        assert_eq!(
            std::fs::read_to_string(&kept).expect("kept log"),
            "window: FAILED ime: copy lost\n"
        );
        assert_eq!(
            keep_unless_pass(&out, "20261122-100200-000", &row, &log, Outcome::Pass),
            None
        );
        assert!(
            !out.join(red_logs::RED).join("20261122-100200-000").exists(),
            "a pass kept its log"
        );
        std::fs::write(&log, "window: undecided ime: host GPU starved\n").expect("undecided log");
        let undecided =
            keep_unless_pass(&out, "20261122-100300-000", &row, &log, Outcome::Undecided)
                .expect("an undecided log is kept");
        assert_eq!(
            undecided,
            out.join(red_logs::UNDECIDED)
                .join("20261122-100300-000")
                .join("window-ime.log")
        );
        std::fs::remove_dir_all(&out).expect("cleanup");
    }

    /// A rerun's line calls its first attempt starved only when that verdict named a starved supply.
    #[test]
    fn a_rerun_line_names_starvation_only_when_ruled() {
        let starved = Some(Supply::Cpu);
        assert_eq!(
            Attempt::Rerun { starved }.lead(Outcome::Pass),
            "starved, rerun ok; "
        );
        assert_eq!(
            Attempt::Rerun { starved: None }.lead(Outcome::Fail),
            "undecided, rerun red; "
        );
        assert_eq!(Attempt::First.lead(Outcome::Pass), "");
    }

    /// A ledger that cannot be written (its dir's path is a file) fails the step beside the red row's note, not in its place.
    #[test]
    fn an_unwritable_ledger_keeps_the_red_rows_notes() {
        let task =
            std::env::temp_dir().join(format!("xtask-gates-noledger-{}", std::process::id()));
        std::fs::write(&task, "a file, not a dir").expect("the blocking file");
        let names = ["app".to_owned(), "ime".to_owned()];
        let done = [
            Done::bare(Outcome::Fail, String::new(), "app: FAILED".to_owned()),
            Done::bare(Outcome::Undecided, String::new(), String::new()),
        ];
        let judged = judge_rows(&task, "r1", "c0ffee", &names, &done);
        std::fs::remove_file(&task).expect("cleanup");
        assert_eq!(judged.failed.len(), 2, "{:?}", judged.failed);
        assert_eq!(judged.failed[0], "app: FAILED");
        assert!(
            judged.failed[1].starts_with("outcome ledger: "),
            "{:?}",
            judged.failed
        );
        assert_eq!(judged.undecided.len(), 1);
    }

    /// The fake-row scheduler test, extended: an undecided row reruns once, after the lanes. Starved then pass is ok and counted; starved then red FAILED; starved twice undecided; a red row is never rerun.
    #[test]
    fn starved_rows_rerun_once_after_the_lanes() {
        const FAKE: &[Row] = &[
            Row::new(&["steady"], false, false),
            Row::new(&["starved-pass"], false, false),
            Row::new(&["starved-red"], false, false),
            Row::new(&["starved-twice"], false, false),
            Row::new(&["red"], false, false),
        ];
        let first_of = |name: &str| match name {
            "starved-pass" | "starved-red" | "starved-twice" => Outcome::Undecided,
            "red" => Outcome::Fail,
            _ => Outcome::Pass,
        };
        let again_of = |name: &str| match name {
            "starved-pass" => Outcome::Pass,
            "starved-red" => Outcome::Fail,
            _ => Outcome::Undecided,
        };
        let run_one = |row: &Row, admitted: &Admitted| -> Done {
            let entered = admitted.enter();
            let outcome = first_of(&row.name());
            Done {
                running: entered.count,
                grant: "harness",
                verdict: (outcome == Outcome::Undecided).then(|| {
                    Ruling::starved(
                        Supply::Cpu,
                        "event bound 1000 ms need 100 ms stalled 960 ms",
                    )
                }),
                ..Done::bare(outcome, format!("gates: {} first", row.name()), row.name())
            }
        };
        let (_, mut done) = run_lanes(FAKE, &run_one, &mut |_| {});
        let mut reran = Vec::new();
        settle(&mut done, &mut |i, _| {
            let name = FAKE[i].name();
            reran.push(name.clone());
            Done {
                grant: "quiet",
                running: 1,
                ..Done::bare(
                    again_of(&name),
                    format!("gates: {name} rerun"),
                    name.clone(),
                )
            }
        });
        assert_eq!(reran, ["starved-pass", "starved-red", "starved-twice"]);
        let outcomes: Vec<Outcome> = done.iter().map(|d| d.outcome).collect();
        assert_eq!(
            outcomes,
            [
                Outcome::Pass,
                Outcome::Pass,
                Outcome::Fail,
                Outcome::Undecided,
                Outcome::Fail
            ]
        );
        assert!(
            done[1]
                .first
                .as_deref()
                .is_some_and(|f| f.outcome == Outcome::Undecided),
            "the starved attempt is kept as the first"
        );
        assert!(done[4].first.is_none(), "a red row was rerun");
        assert_eq!(
            starvation_of(&done).line("gates"),
            "gates: starvation starved=3 (cpu=3 gpu=0) rerun quiet=3 shared=0 -> ok=1 failed=1 undecided=1; judged late=0; unknown=0; ceiling=0"
        );
    }

    /// The ceiling: a row undecided in three consecutive runs of this checkout fails, naming those runs. A deciding run resets the streak; other rows' runs do not count.
    #[test]
    fn a_third_consecutive_undecided_run_hits_the_ceiling() {
        let task = std::env::temp_dir().join(format!("xtask-gates-ceiling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&task);
        let entry = |run: &'static str, row: &'static str, outcome: Outcome| red_logs::Entry {
            run,
            commit: "c0ffee",
            row,
            outcome,
            first: Some(Outcome::Undecided),
            stall: Some(31.0),
            grant: "quiet",
            supply: Some(Supply::Cpu),
        };
        let row = "window-slow-display";
        let step = |entries: &[red_logs::Entry<'_>]| -> Option<String> {
            red_logs::ledger_append(&task, entries).expect("ledger");
            let trailing = red_logs::trailing_undecided(&task).expect("streaks");
            ceiling_note(&trailing.streaks, row)
        };
        assert_eq!(step(&[entry("r1", row, Outcome::Undecided)]), None);
        assert_eq!(
            step(&[
                entry("r2", row, Outcome::Undecided),
                entry("r2", "app", Outcome::Pass)
            ]),
            None
        );
        assert_eq!(step(&[entry("r3", row, Outcome::Pass)]), None);
        assert_eq!(step(&[entry("r4", row, Outcome::Undecided)]), None);
        assert_eq!(step(&[entry("r5", row, Outcome::Undecided)]), None);
        assert_eq!(
            step(&[entry("r6", row, Outcome::Undecided)]).as_deref(),
            Some(
                "window-slow-display undecided in 3 consecutive runs of this checkout (r4, r5, r6): the ceiling fails the step"
            )
        );
        std::fs::remove_dir_all(&task).expect("cleanup");
    }
}
