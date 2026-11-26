// SPDX-License-Identifier: MIT
//! Check step `gates`: runtime gates `cargo xtask check` runs on the checkout's build dir, each as `cargo xtask test` runs it (its own harness process, its own verdict line), so a regression in
//! one of them cannot land green. A row is a gate that passes on a build machine without the laptop's hardware, leaves the desktop session as it found it, needs no session PipeWire (not audio-pcm,
//! audio-app, webview) and takes under ~35 s; slower, load-flaky (widgets, dialog) and laptop-only gates stay `xtask test` runs. A host that cannot run a row's gate says so in the gate's own verdict
//! (`host lacks ...`), never by dropping the row.
//! Before the lanes the step builds every cargo product the rows run, once (gate_build.rs `ensure`, the function each gate calls itself); a product whose identity changes while the lanes run fails
//! the step, naming it and the row whose end first saw it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use xtask::build_pool::{self, Class};
use xtask::gate_need;
use xtask::harness::{Ruling, Starvation, Supply, Undecided, judged_in, undecided_verdict};
use xtask::red_logs;
use xtask::resource_ledger;

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
    Row::new(&["platform-ui", "--map", "pinned"], false, false),
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
    // a floating compositor that keeps the first commit's size for good: that commit must be the Task's dp size
    Row::new(&["window", "--stage", "opening"], true, true),
    Row::new(
        &[
            "window",
            "--stage",
            "opening",
            // taller than GDK's guess of the monitor
            "--declare-display",
            "411x914",
        ],
        true,
        true,
    ),
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

/// The resource-ledger records of one step: each passing row's deciding attempt (its rerun, when there was one), quiet, largest took per wait (gate_need.rs `records`). The self-test row's waits are not gate waits.
fn need_records(run: &str, rows: &[(&Row, &Done)]) -> Vec<resource_ledger::Record> {
    rows.iter()
        .filter(|(row, d)| row.samples && d.outcome == Outcome::Pass)
        .flat_map(|(row, d)| {
            let attempt = gate_need::attempt(run, d.first.is_some());
            gate_need::records(&attempt, &row.name(), d.stall, &d.waits)
        })
        .collect()
}

/// The check log's entries of one step: each wait's bound per attempt (red_logs.rs `waits_append`). Took and outcome are not logged: the ledger and gate-ledger.tsv hold them. A rerun's first attempt is kept too. Samples of one wait with one bound are one line.
fn wait_entries(
    run: &str,
    commit: &str,
    names: &[String],
    done: &[Done],
) -> Vec<red_logs::WaitEntry> {
    let mut bounds: BTreeSet<(String, String, String, u64)> = BTreeSet::new();
    for (name, d) in names.iter().zip(done) {
        let deciding = (gate_need::attempt(run, d.first.is_some()), d);
        let first = d
            .first
            .as_deref()
            .map(|f| (gate_need::attempt(run, false), f));
        for (attempt, a) in std::iter::once(deciding).chain(first) {
            for w in &a.waits {
                bounds.insert((attempt.clone(), name.clone(), w.name.clone(), w.bound_ms));
            }
        }
    }
    bounds
        .into_iter()
        .map(|(attempt, row, wait, bound_ms)| red_logs::WaitEntry {
            attempt,
            row,
            wait,
            commit: commit.to_owned(),
            bound_ms,
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
    let mut broken: Vec<String> = Vec::new();
    let trailing = match ledger {
        Ok(trailing) => trailing,
        Err(e) => {
            broken.push(format!("outcome ledger: {e:#}"));
            red_logs::Trailing::default()
        }
    };
    // the check log of waits (red_logs.rs): every attempt's samples with commit and bound, written before the step's logs are cleaned up
    let waits = wait_entries(run, commit, names, done);
    if let Err(e) = red_logs::waits_append(task, &waits, red_logs::KEEP_RUNS) {
        broken.push(format!("gate waits log: {e:#}"));
    }
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
    // every cargo product a row runs, built once before any row starts (gate_build.rs `Group::ALL`), under a cargo grant released before the rows take theirs. The rows read them from the
    // record (`test --prebuilt`) and run no cargo: the guard (`guard_dir`) refuses any cargo call they or their processes make
    let (built, waited, took) = {
        let grant = build_pool::acquire_quiet_stdout(&ws, root, Class::Cargo)?;
        let started = Instant::now();
        let built = gate_build::ensure(root, out, &Group::ALL)?;
        (built, grant.waited, started.elapsed())
    };
    let record_file = logs.join(RECORD_FILE);
    built.record().write(&record_file)?;
    let products = built.files();
    let snapshot = Snapshot::take(&products);
    let guard = guard_dir(&logs)?;
    let path_env = std::env::join_paths(std::iter::once(guard.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    eprintln!(
        "gates: prebuild {:.1}s (pool wait {:.1}s, {} products, {} record entries)",
        took.as_secs_f64(),
        waited.as_secs_f64(),
        products.len(),
        built.record().entries()
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
        record: &record_file,
        guard: &guard,
        path: &path_env,
    };
    // after a row's process ended: a product it (or a row beside it) re-linked or re-staged is named now, with this row
    let note_moved = |row: &Row| {
        moved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .note(&snapshot, Some(&row.name()));
    };
    // a row's place is taken on the lanes' thread, in start order; its grant is waited for on the row's own thread
    let take = |row: &Row| build_pool::enqueue(&ws, root, row.class);
    let run_one = |row: &Row, place: Result<build_pool::Queued>, admitted: &Admitted| -> Done {
        // the grant lives on this thread until the row's process ended; a pool or spawn error is the row's red verdict, so the other rows still report
        let gated = place
            .and_then(|queued| queued.wait(build_pool::POLL, &mut |line: &str| eprintln!("{line}")))
            .and_then(|grant| {
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
    let (stats, mut done) = run_lanes(ROWS, &take, &run_one, &mut |first| {
        eprintln!("{}", first.line)
    });
    settle(&mut done, &mut |i, first| rerun(&ROWS[i], first));
    let names: Vec<String> = ROWS.iter().map(Row::name).collect();
    let Judged {
        mut failed,
        undecided,
        starvation,
    } = judge_rows(&task, &run, commit.trim(), &names, &done);
    // the resource ledger (gate_need.rs): every passing row's deciding attempt's quiet waits, machine-wide, after the step's rows ended
    let pairs: Vec<(&Row, &Done)> = ROWS.iter().zip(&done).collect();
    let records = need_records(&run, &pairs);
    match resource_ledger::append(&ws, &records, gate_need::WINDOW) {
        Ok(()) => eprintln!("gates: resource ledger +{} records", records.len()),
        Err(e) => failed.push(format!("resource ledger: {e:#}")),
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
    // a cargo call that reached the guard fails the step, whichever row (or process of a row's tree) made it: the rows' own verdicts name the call too
    let calls = std::fs::read_to_string(guard.join(REFUSED_LOG)).unwrap_or_default();
    let calls: Vec<&str> = calls.lines().collect();
    eprintln!("gates: cargo calls from the rows: {}", calls.len());
    if let Some(first) = calls.first() {
        failed.push(format!(
            "{} cargo call(s) from the gate rows, first: {first}",
            calls.len()
        ));
    }
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

/// A row's result on its way to the lanes: sent when the row's thread ends, or as `None` when it unwinds, so the lanes never wait for a result that will not come.
struct Sent {
    tx: mpsc::Sender<(usize, Option<Done>)>,
    i: usize,
    done: Option<Done>,
}

impl Drop for Sent {
    fn drop(&mut self) {
        let _ = self.tx.send((self.i, self.done.take()));
    }
}

/// Runs `rows` with `run_one`: parallel rows concurrently, then serial rows one at a time, alone.
/// `take` makes a row's pool place on this thread, in start order (the parallel rows others wait for first, then table order), each once its `after` rows ended; `run_one` waits for that place on the row's own thread. So the queue serves the start order, not the order the threads reach the pool.
/// `emit` gets each result in table order as soon as every earlier row has one; each line gains `lane=` and `running=` ([`Done::running`]). Returns the stats and every result, in table order.
fn run_lanes<P: Send>(
    rows: &[Row],
    take: &(dyn Fn(&Row) -> P + Sync),
    run_one: &(dyn Fn(&Row, P, &Admitted) -> Done + Sync),
    emit: &mut dyn FnMut(&Done),
) -> (LaneStats, Vec<Done>) {
    let started = Instant::now();
    let names: Vec<String> = rows.iter().map(Row::name).collect();
    // `after` names are checked against the table by a test (`after_names_earlier_parallel_rows`); a name that is no row is ignored
    let deps: Vec<Vec<usize>> = rows
        .iter()
        .map(|row| {
            row.after
                .iter()
                .filter_map(|a| names.iter().position(|n| n == a))
                .collect()
        })
        .collect();
    let depended: Vec<bool> = (0..rows.len())
        .map(|i| deps.iter().any(|d| d.contains(&i)))
        .collect();
    let mut waiting: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].lane == Lane::Parallel)
        .collect();
    // start order: the rows others wait for first, then table order (a stable sort)
    waiting.sort_by_key(|&i| !depended[i]);
    let admitted = Admitted::default();
    let start_one = |i: usize, place: P| -> Done {
        let mut done = run_one(&rows[i], place, &admitted);
        done.line = with_lane(&done.line, rows[i].lane, done.running);
        done
    };
    let mut results: Vec<Option<Done>> = (0..rows.len()).map(|_| None).collect();
    let mut ended = vec![false; rows.len()];
    let mut next = 0;
    let mut flush = |results: &[Option<Done>], next: &mut usize| {
        while let Some(Some(done)) = results.get(*next) {
            emit(done);
            *next += 1;
        }
    };
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let mut running = 0;
        while !waiting.is_empty() || running > 0 {
            let mut k = 0;
            while k < waiting.len() {
                let i = waiting[k];
                if deps[i].iter().all(|&d| ended[d]) {
                    waiting.remove(k);
                    let place = take(&rows[i]);
                    let (tx, run) = (tx.clone(), &start_one);
                    scope.spawn(move || {
                        let mut sent = Sent { tx, i, done: None };
                        sent.done = Some(run(i, place));
                    });
                    running += 1;
                } else {
                    k += 1;
                }
            }
            assert!(running > 0, "the parallel rows' `after` edges form a cycle");
            let (i, done) = rx.recv().expect("the lanes hold a sender");
            let Some(done) = done else {
                // a row unwound: stop dispatching; the scope re-raises its panic when it ends
                break;
            };
            running -= 1;
            ended[i] = true;
            results[i] = Some(done);
            flush(&results, &mut next);
        }
    });
    for (i, row) in rows.iter().enumerate() {
        if row.lane == Lane::Serial {
            let place = take(row);
            results[i] = Some(start_one(i, place));
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
    /// the record check's prebuild wrote (`test --prebuilt`)
    record: &'a Path,
    /// the guard dir (`guard_dir`): `CARGO` and the row's first PATH entry
    guard: &'a Path,
    path: &'a std::ffi::OsStr,
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
        // the products are the prebuild's (`--prebuilt`); cargo is the guard's: a cargo call from the row or its tree is refused (refuse_cargo)
        let mut child = cmd
            .current_dir(self.root)
            .arg("test")
            .arg("--prebuilt")
            .arg(self.record)
            .args(&argv)
            .env("CARGO", self.guard.join("cargo"))
            .env("PATH", self.path)
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
        // a refused cargo call fails the row whatever its exit status: the row ran cargo, which no row may
        let refused = refused_calls(&text);
        let outcome = if refused.is_empty() {
            Outcome::of_exit(status.code())
        } else {
            Outcome::Fail
        };
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
        let shown = if refused.is_empty() {
            shown.to_owned()
        } else {
            format!("ran cargo: {}", refused.join("; "))
        };
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

/// The record check's prebuild writes (`out/check-gates/`), read by each row as `test --prebuilt`.
pub const RECORD_FILE: &str = "build-record.tsv";
/// The guard's log in its dir: one line per refused cargo call (`refuse_cargo`).
pub const REFUSED_LOG: &str = "refused.tsv";
/// Printed by the guard on the row's stderr (a row's log keeps it): `refused_calls` finds the row by it.
const REFUSAL: &str = "gate-no-cargo: refused";

/// check's guard, no cargo in a gate row: a dir with this harness binary as `cargo` (a hard link, else a copy), first on each row's PATH and set as its `CARGO`.
/// Run under that name, the binary refuses the call (`refuse_cargo`). `refused.tsv` is emptied here.
fn guard_dir(logs: &Path) -> Result<PathBuf> {
    let dir = logs.join("guard");
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let exe = std::env::current_exe().context("the harness's own path")?;
    let link = dir.join("cargo");
    let _ = std::fs::remove_file(&link);
    if std::fs::hard_link(&exe, &link).is_err() {
        std::fs::copy(&exe, &link)
            .with_context(|| format!("copying {} to {}", exe.display(), link.display()))?;
    }
    let log = dir.join(REFUSED_LOG);
    std::fs::write(&log, "").with_context(|| log.display().to_string())?;
    Ok(dir)
}

/// The guard's behaviour when run as `cargo` (main.rs): the call is logged beside it and fails (exit 97) with a line the row's log keeps.
pub fn refuse_cargo() -> ExitCode {
    let call = format!(
        "cargo {}",
        std::env::args().skip(1).collect::<Vec<_>>().join(" ")
    );
    let log = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(REFUSED_LOG)))
        .and_then(|log| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(log)
                .ok()
        });
    if let Some(mut f) = log {
        let _ = std::io::Write::write_all(&mut f, format!("{call}\n").as_bytes());
    }
    eprintln!(
        "{REFUSAL} `{call}`: a gate row runs no cargo; its products come from check's prebuilt record"
    );
    ExitCode::from(97)
}

/// The refused calls a row's log holds (`refuse_cargo` lines).
fn refused_calls(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains(REFUSAL))
        .map(|l| l.trim().to_owned())
        .collect()
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
    use xtask::gate_goals::{GATES, OUTSIDE_CHECK_ROWS};

    // Break: add a gate without a row or reason, schedule a listed exception, or leave an exception after removing its gate.
    #[test]
    fn each_native_gate_has_a_check_row_or_a_reason() {
        for &(name, _) in GATES {
            let scheduled = ROWS.iter().any(|row| row.cmd() == name);
            let exceptions = OUTSIDE_CHECK_ROWS
                .iter()
                .filter(|&&(n, _)| n == name)
                .count();
            assert_eq!(
                usize::from(scheduled) + exceptions,
                1,
                "{name}: expected a check row or one exception"
            );
        }
        for &(name, reason) in OUTSIDE_CHECK_ROWS {
            assert!(
                GATES.iter().any(|&(n, _)| n == name),
                "{name}: stale exception"
            );
            assert!(
                !reason.trim().is_empty(),
                "{name}: missing exception reason"
            );
        }
    }

    /// A step's need samples: each passing row's own waits, from its deciding run; a red row and the self-test row add none. Expected values written out.
    #[test]
    fn need_records_are_the_passing_rows_quiet_waits_and_the_self_test_is_not_one() {
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
        let records = need_records("20261124-120000-000", &pairs);
        let got: Vec<(&str, &str, &str, u64, Option<f64>)> = records
            .iter()
            .map(|r| {
                (
                    r.run.as_str(),
                    r.name.as_str(),
                    r.wait.as_str(),
                    r.took_ms,
                    r.stall,
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

    /// A rerun is its own attempt: the ledger keeps its deciding pass as `<run>.rerun`; the check log keeps each attempt's waits under its own id and outcome.
    #[test]
    fn a_rerun_is_its_own_attempt_in_the_ledger_and_the_check_log() {
        let wait = |name: &str, took_ms: u64| gate_need::WaitLine {
            name: name.to_owned(),
            took_ms,
            bound_ms: 60_000,
        };
        let first = Done {
            stall: Some(0.1),
            waits: vec![wait("marker", 900)],
            ..Done::bare(Outcome::Undecided, String::new(), String::new())
        };
        let rerun = Done {
            stall: Some(0.1),
            waits: vec![wait("marker", 40)],
            first: Some(Box::new(first)),
            ..Done::bare(Outcome::Pass, String::new(), String::new())
        };
        let app = Row::new(&["app"], true, true);
        let records = need_records("r1", &[(&app, &rerun)]);
        let got: Vec<(&str, u64)> = records
            .iter()
            .map(|r| (r.run.as_str(), r.took_ms))
            .collect();
        assert_eq!(got, [("r1.rerun", 40)]);
        let names = ["app".to_owned()];
        let entries = wait_entries("r1", "c0ffee", &names, std::slice::from_ref(&rerun));
        let got: Vec<(&str, &str, u64)> = entries
            .iter()
            .map(|e| (e.attempt.as_str(), e.wait.as_str(), e.bound_ms))
            .collect();
        assert_eq!(
            got,
            [("r1", "marker", 60_000), ("r1.rerun", "marker", 60_000)]
        );
    }

    /// The check log keeps one line per wait and bound of an attempt: three samples of one wait with one bound are one line.
    #[test]
    fn a_wait_samples_one_line_per_bound_of_its_attempt() {
        let wait = |name: &str, took_ms: u64| gate_need::WaitLine {
            name: name.to_owned(),
            took_ms,
            bound_ms: 60_000,
        };
        let passed = Done {
            stall: Some(0.1),
            waits: vec![
                wait("app-progress", 5),
                wait("app-progress", 9),
                wait("app-progress", 7),
            ],
            ..Done::bare(Outcome::Pass, String::new(), String::new())
        };
        let names = ["app".to_owned()];
        let entries = wait_entries("r1", "c0ffee", &names, std::slice::from_ref(&passed));
        assert_eq!(entries.len(), 1, "{entries:?}");
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
        let run_one = |row: &Row, _: (), admitted: &Admitted| -> Done {
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
        let (stats, _) = run_lanes(FAKE, &|_: &Row| (), &run_one, &mut |d| {
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

    /// The lanes admit `app` (the row others wait for) first, whatever the table order: width 1 admits it first; width 3 admits `app` and the two rows after it in table order. The first wave holds its grants until `width` rows are admitted, so the set is exact, not a timing guess.
    #[test]
    fn lanes_admit_the_dependency_first() {
        const FAKE: &[Row] = &[
            Row::new(&["reader-a"], false, false),
            Row::new(&["reader-b"], false, false),
            Row::new(&["reader-c"], false, false),
            Row::new(&["reader-d"], false, false),
            Row::new(&["app"], false, false),
            Row::new(&["follower"], false, false).after(&["app"]),
        ];
        const FAST: Duration = Duration::from_millis(5);
        for width in [1, 3] {
            let ws =
                std::env::temp_dir().join(format!("atl-lanes-w{width}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&ws);
            std::fs::create_dir_all(&ws).expect("scratch ws");
            let toml = format!(
                "[pool]\ncpu_slots = {width}\nmem_gib = 20\n[pool.harness]\ncpu_min = 1\ncpu_max = 1\nmem_gib = 1\n"
            );
            std::fs::write(ws.join("workspace.toml"), toml).expect("workspace.toml");
            let who = ws.join("checkout");
            let t0 = Instant::now();
            let order: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let spans: Mutex<Vec<(String, Duration, Duration)>> = Mutex::new(Vec::new());
            let take = |row: &Row| build_pool::enqueue(&ws, &who, row.class);
            let run_one = |row: &Row, place: Result<build_pool::Queued>, admitted: &Admitted| {
                let grant = place
                    .and_then(|queued| queued.wait(FAST, &mut |_: &str| {}))
                    .expect("grant");
                let entered = admitted.enter();
                let from = t0.elapsed();
                order.lock().expect("order").push(row.name());
                let deadline = Instant::now() + Duration::from_secs(10);
                while order.lock().expect("order").len() < width && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(30));
                spans
                    .lock()
                    .expect("spans")
                    .push((row.name(), from, t0.elapsed()));
                let running = entered.count;
                drop(entered);
                drop(grant);
                Done {
                    took: 0.03,
                    running,
                    ..Done::bare(
                        Outcome::Pass,
                        format!("gates: {} x (0.0s peak=0MiB) v", row.name()),
                        row.name(),
                    )
                }
            };
            let _ = run_lanes(FAKE, &take, &run_one, &mut |_| {});
            let order = order.into_inner().expect("order");
            let mut first: Vec<&str> = order[..width].iter().map(String::as_str).collect();
            first.sort_unstable();
            let expect: &[&str] = if width == 1 {
                &["app"]
            } else {
                &["app", "reader-a", "reader-b"]
            };
            assert_eq!(first, expect, "width {width}: admitted {order:?}");
            let spans = spans.into_inner().expect("spans");
            let span = |n: &str| {
                let s = spans.iter().find(|s| s.0 == n).expect("span");
                (s.1, s.2)
            };
            assert!(
                span("follower").0 >= span("app").1,
                "width {width}: follower admitted before app ended"
            );
            let _ = std::fs::remove_dir_all(&ws);
        }
    }

    /// A row that unwinds does not leave the lanes waiting for its result: the lanes stop dispatching and the panic surfaces (without the `Sent` guard this test hangs).
    #[test]
    #[should_panic(expected = "a scoped thread panicked")]
    fn lanes_do_not_wait_for_an_unwound_row() {
        const FAKE: &[Row] = &[
            Row::new(&["fails"], false, false),
            Row::new(&["after-fail"], false, false).after(&["fails"]),
        ];
        let run_one = |row: &Row, _: (), _: &Admitted| -> Done {
            assert_ne!(row.name(), "fails", "the row unwinds");
            Done::bare(
                Outcome::Pass,
                format!("gates: {} x", row.name()),
                row.name(),
            )
        };
        let _ = run_lanes(FAKE, &|_: &Row| (), &run_one, &mut |_| {});
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
        assert_eq!(judged.failed.len(), 3, "{:?}", judged.failed);
        assert_eq!(judged.failed[0], "app: FAILED");
        assert!(
            judged.failed[1].starts_with("outcome ledger: "),
            "{:?}",
            judged.failed
        );
        assert!(
            judged.failed[2].starts_with("gate waits log: "),
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
        let run_one = |row: &Row, _: (), admitted: &Admitted| -> Done {
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
        let (_, mut done) = run_lanes(FAKE, &|_: &Row| (), &run_one, &mut |_| {});
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
