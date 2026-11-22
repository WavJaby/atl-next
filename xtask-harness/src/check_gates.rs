// SPDX-License-Identifier: MIT
//! Check step `gates`: runtime gates `cargo xtask check` runs on the checkout's build dir, each as `cargo xtask test` runs it (its own harness process, its own verdict line), so a regression in
//! one of them cannot land green. A row is a gate that passes on a build machine without the laptop's hardware, leaves the desktop session as it found it, needs no session PipeWire (not audio-pcm,
//! audio-app, webview) and takes under ~35 s; slower, load-flaky (widgets, dialog) and laptop-only gates stay `xtask test` runs. A host that cannot run a row's gate says so in the gate's own verdict
//! (`host lacks ...`), never by dropping the row.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use xtask::build_pool::{self, Class};
use xtask::red_logs;

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

/// One row's result, held until every row before it in the table has reported: the `gates:` lines stay in table order whatever ends first.
struct Done {
    outcome: Outcome,
    took: f64,
    /// the `gates:` line less its lane fields ([`run_lanes`] adds them)
    line: String,
    /// `<row> (log <path>): <verdict>` when red, `<row> <why>` when undecided
    note: String,
    /// what ended a window row's watch
    end: Option<String>,
    /// rows holding their grant when this one was admitted, itself included (0: never admitted)
    running: usize,
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

/// Runs every row on build dir `out` for host `host`, logs under `<out>/check-gates/`; fails naming each red gate's verdict line. Ok: the undecided rows (`<row> <why>`), neither pass nor fail ([`Outcome`]).
/// Each row takes its own pool grant (memory admission decides how many run at once); no grant is held around the step ([`run_lanes`]).
pub fn run(root: &Path, host: &str, out: &Path) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("the harness's own path")?;
    let ws = Layout::of(root)?.workspace().to_owned();
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let out_arg = out.to_str().context("build dir path is not UTF-8")?;
    let run = red_logs::run_id();
    let task = red_logs::task_dir(root)?;
    let at = RowRun {
        exe: &exe,
        root,
        logs: &logs,
        task: &task,
        run: &run,
        host,
        out: out_arg,
    };
    let run_one = |row: &Row, admitted: &Admitted| -> Done {
        // the grant lives on this thread until the row's process ended; a pool or spawn error is the row's red verdict, so the other rows still report
        let gated = build_pool::acquire_quiet_stdout(&ws, root, row.class).and_then(|grant| {
            let entered = admitted.enter();
            let mut done = at.row(row)?;
            done.running = entered.count;
            Ok((done, grant))
        });
        gated.map(|(done, _grant)| done).unwrap_or_else(|e| {
            let line = format!("error: {e:#}");
            Done {
                outcome: Outcome::Fail,
                took: 0.0,
                note: format!("{}: {line}", row.name()),
                line: format!("gates: {} FAILED (0.0s) {line}", row.name()),
                end: None,
                running: 0,
            }
        })
    };
    let mut failed = Vec::new();
    let mut undecided = Vec::new();
    let mut ends = WatchTally::default();
    let stats = run_lanes(ROWS, &run_one, &mut |done| {
        eprintln!("{}", done.line);
        ends.count(done.end.as_deref());
        match done.outcome {
            Outcome::Pass => {}
            // the path leads: check quotes its first error at 160 chars (verdict.rs FIRST_ERROR_CHARS), and the verdict line lands in it
            Outcome::Fail => failed.push(done.note.clone()),
            Outcome::Undecided => undecided.push(done.note.clone()),
        }
    });
    eprintln!("gates: watch ends {}", ends.line());
    eprintln!("gates: {}", stats.line());
    if failed.is_empty() {
        return Ok(undecided);
    }
    let open = if undecided.is_empty() {
        String::new()
    } else {
        format!(" ({} undecided: {})", undecided.len(), undecided.join("; "))
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
/// `emit` gets each result in table order as soon as every earlier row has one; each line gains `lane=` and `running=` ([`Done::running`]).
fn run_lanes(
    rows: &[Row],
    run_one: &(dyn Fn(&Row, &Admitted) -> Done + Sync),
    emit: &mut dyn FnMut(&Done),
) -> LaneStats {
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
    LaneStats {
        wall: started.elapsed().as_secs_f64(),
        sum: results.iter().flatten().map(|d| d.took).sum(),
        parallel: rows.iter().filter(|r| r.lane == Lane::Parallel).count(),
        serial: rows.iter().filter(|r| r.lane == Lane::Serial).count(),
        max_at_once: admitted.max.load(Ordering::SeqCst),
    }
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
    /// One row as its own `xtask test` process, its output in `<logs>/<row>.log`; a red or undecided log is kept ([`keep_unless_pass`]).
    /// Called on the thread holding the row's grant: its job count reaches the gate's cargo runs (xtask `grant_jobs`).
    fn row(&self, row: &Row) -> Result<Done> {
        let log = self.logs.join(format!("{}.log", row.name()));
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
        let peak = peak_while(&mut child)?;
        let took = started.elapsed().as_secs_f64();
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let phases = phases_field(&text);
        let measured = format!(
            "peak={}MiB{} work={}",
            peak.kib / 1024,
            phases
                .as_ref()
                .map(|p| format!(" {}", p.field))
                .unwrap_or_default(),
            runtime_work(&text).unwrap_or("-")
        );
        let outcome = Outcome::of_exit(peak.status.code());
        let verdict = match outcome {
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
        let line = format!(
            "gates: {} {} ({took:.1}s {measured}) {verdict}{kept_note}",
            row.name(),
            match outcome {
                Outcome::Pass => "ok",
                Outcome::Fail => "FAILED",
                Outcome::Undecided => "undecided",
            }
        );
        let note = match outcome {
            Outcome::Undecided => undecided_note(row, verdict),
            Outcome::Pass | Outcome::Fail => {
                format!("{} (log {}): {verdict}", row.name(), at.display())
            }
        };
        Ok(Done {
            outcome,
            took,
            line,
            note,
            end: phases.map(|p| p.end),
            running: 0,
        })
    }
}

/// How often a row's process tree is sampled for its memory. A gate runs for seconds (a cold start alone 12-15 s), so 5 samples a second see its plateau; a spike shorter than this is missed,
/// which makes the printed peak a lower bound of the true one.
const PEAK_SAMPLE: Duration = Duration::from_millis(200);

/// A row's exit and the largest summed resident memory of its process tree seen while it ran.
struct Peak {
    status: ExitStatus,
    kib: u64,
}

/// Waits for `child`, sampling its process tree's summed VmRSS every [`PEAK_SAMPLE`]. The tree is the child and its descendants by parent pid: a process that double-forks away from it
/// (reparented to init) is not counted. No cgroup per row: this host's processes sit in the root cgroup (`0::/init.scope`, no delegation), so `memory.peak` of a row scope is not available.
fn peak_while(child: &mut Child) -> Result<Peak> {
    let root = child.id();
    let mut kib = 0;
    loop {
        if let Some(status) = child.try_wait().context("waiting for the gate")? {
            return Ok(Peak { status, kib });
        }
        kib = kib.max(tree_rss_kib(root));
        std::thread::sleep(PEAK_SAMPLE);
    }
}

/// The summed VmRSS (KiB) of `root` and its descendants, from `/proc`; a process gone between listing and reading counts 0.
fn tree_rss_kib(root: u32) -> u64 {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let procs: Vec<(u32, u32)> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            Some((pid, ppid_of_stat(&stat)?))
        })
        .collect();
    descendants(root, &procs)
        .into_iter()
        .filter_map(|pid| vm_rss_kib(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?))
        .sum()
}

/// The parent pid in a `/proc/<pid>/stat` line (proc(5): `pid (comm) state ppid ...`; comm may hold spaces and parentheses, so the fields after the last `)`).
fn ppid_of_stat(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `VmRSS:` in a `/proc/<pid>/status` text, KiB (proc(5)); a kernel thread has none.
fn vm_rss_kib(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// `root` and every pid whose parent chain in `procs` (`(pid, ppid)`) reaches it.
fn descendants(root: u32, procs: &[(u32, u32)]) -> Vec<u32> {
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&parent) = tree.get(i) {
        tree.extend(
            procs
                .iter()
                .filter(|(_, pp)| *pp == parent)
                .map(|(p, _)| *p),
        );
        i += 1;
    }
    tree
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

/// An undecided gate's own line (`<cmd>: undecided <stage>: <why>`, window.rs `judge`).
fn undecided_line<'a>(text: &'a str, cmd: &str) -> Option<&'a str> {
    let prefix = format!("{cmd}: {UNDECIDED_WORD}");
    text.lines().rev().find(|l| l.starts_with(&prefix))
}

/// `<row> <why>` for check's verdict line: the gate's undecided line less its `<cmd>: undecided <stage>: ` head.
fn undecided_note(row: &Row, line: &str) -> String {
    let why = line
        .split_once(UNDECIDED_WORD)
        .map(|(_, rest)| rest.split_once(": ").map_or(rest, |(_, why)| why))
        .unwrap_or(line);
    format!("{} {why}", row.name())
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
                outcome,
                took: 0.1,
                line: format!("gates: {} x (0.1s peak=0MiB) v", row.name()),
                note: row.name(),
                end: None,
                running: entered.count,
            }
        };
        let mut seen = Vec::new();
        let stats = run_lanes(FAKE, &run_one, &mut |d| {
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

    /// An undecided run's note for check's line: the row name and the stage's why, from the line the window judge prints.
    #[test]
    fn undecided_note_names_row_and_why() {
        let text = "window: held: undecided host GPU starved: x\nwindow: undecided slow-display: host GPU starved (render max 205 ms > 100 ms)\n";
        let row = Row::new(&["window", "--stage", "slow-display"], true, true);
        let line = undecided_line(text, "window").expect("line");
        assert_eq!(
            undecided_note(&row, line),
            "window-slow-display host GPU starved (render max 205 ms > 100 ms)"
        );
        assert_eq!(undecided_line("window: pass x\n", "window"), None);
    }

    /// proc(5) lines by hand: a comm with spaces and a `)` must not shift the ppid field; VmRSS is the second word of its line, in kB.
    #[test]
    fn proc_fields_by_hand() {
        assert_eq!(
            ppid_of_stat("4242 (a) b) (c) S 17 4242 4242 0 -1"),
            Some(17)
        );
        assert_eq!(ppid_of_stat("1 (init) S 0 1 1"), Some(0));
        assert_eq!(ppid_of_stat("garbage"), None);
        let status = "Name:\tatld\nVmHWM:\t  9000 kB\nVmRSS:\t  5120 kB\nThreads:\t4\n";
        assert_eq!(vm_rss_kib(status), Some(5120));
        assert_eq!(vm_rss_kib("Name:\tkthreadd\n"), None);
    }

    /// The tree is the root and every pid whose parent chain reaches it; a sibling of the root and its child are not in it.
    #[test]
    fn descendants_follow_parent_chains() {
        let procs = [(10, 1), (11, 10), (12, 11), (13, 10), (20, 1), (21, 20)];
        let mut tree = descendants(10, &procs);
        tree.sort_unstable();
        assert_eq!(tree, [10, 11, 12, 13]);
    }

    /// The sampler responds: this test process's own tree has resident memory (a dead probe would read 0 for every row).
    #[test]
    fn the_sampler_sees_this_process() {
        assert!(tree_rss_kib(std::process::id()) > 0);
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
}
