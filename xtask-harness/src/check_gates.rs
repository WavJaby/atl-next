// SPDX-License-Identifier: MIT
//! Check step `gates`: runtime gates `cargo xtask check` runs on the checkout's build dir, each as `cargo xtask test` runs it (its own harness process, its own verdict line), so a regression in
//! one of them cannot land green. A row is a gate that passes on a build machine without the laptop's hardware, leaves the desktop session as it found it, needs no session PipeWire (not audio-pcm,
//! audio-app, webview) and takes under ~35 s; slower, load-flaky (widgets, dialog) and laptop-only gates stay `xtask test` runs. A host that cannot run a row's gate says so in the gate's own verdict
//! (`host lacks ...`), never by dropping the row.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use xtask::red_logs;

use crate::verdict::{Outcome, UNDECIDED_WORD};
use crate::window::{PHASES_LINE, WatchEnd};

/// One gate run: `xtask test <args>`, plus `--host <build machine>` when `host` and `--out <build dir>` when `out` (each only where the gate's own arguments have it).
pub struct Row {
    pub args: &'static [&'static str],
    pub host: bool,
    pub out: bool,
}

impl Row {
    const fn new(args: &'static [&'static str], host: bool, out: bool) -> Self {
        Self { args, host, out }
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

/// The gates `check` runs, cheapest first, except where one reads what another stages.
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
    Row::new(&["platform-ui"], false, false),
    Row::new(&["compositor"], true, true),
    Row::new(&["parcel"], true, true),
    Row::new(&["provider"], true, true),
    Row::new(&["system-scan"], true, true),
    Row::new(&["broadcast"], true, true),
    Row::new(&["billing"], true, true),
    Row::new(&["keystore"], true, true),
    Row::new(&["gralloc"], true, true),
    Row::new(&["composer"], true, true),
    Row::new(&["instrument"], true, true),
    Row::new(&["app"], true, true),
    // reads the `app/` tree the app gate stages (its refusal otherwise: "missing <out>/app/root/system/lib64/x86_64 ... run `xtask test app`")
    Row::new(&["nb-identity"], false, true),
    // needs what nb-identity needs (the staged `app/`)
    Row::new(&["sles-bridge"], false, true),
    Row::new(&["window", "--stage", "surface-view"], true, true),
    Row::new(&["window", "--stage", "ime"], true, true),
    // a GPU-starved host (window render over the hold bound) makes it `undecided`: neither pass nor fail, named on check's verdict line
    Row::new(&["window", "--stage", "slow-display"], true, true),
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
    Row::new(&["window", "--stage", "ui-block"], true, true),
    // a tiling compositor's first configure differs from the requested size: the app must lay out at the configured one
    Row::new(&["window", "--stage", "tiled"], true, true),
];

/// Runs every row on build dir `out` for host `host`, logs under `<out>/check-gates/`; fails naming each red gate's verdict line. Ok: the undecided rows (`<row> <why>`), neither pass nor fail ([`Outcome`]).
pub fn run(root: &Path, host: &str, out: &Path) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("the harness's own path")?;
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let out_arg = out.to_str().context("build dir path is not UTF-8")?;
    let run = red_logs::run_id();
    let task = red_logs::task_dir(root)?;
    let mut failed = Vec::new();
    let mut undecided = Vec::new();
    let mut ends = WatchTally::default();
    for row in ROWS {
        let log = logs.join(format!("{}.log", row.name()));
        let file = std::fs::File::create(&log).with_context(|| log.display().to_string())?;
        let argv = row.argv(host, out_arg);
        let started = Instant::now();
        let mut child = Command::new(&exe)
            .current_dir(root)
            .arg("test")
            .args(&argv)
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .with_context(|| format!("spawning xtask-harness test {}", argv.join(" ")))?;
        let peak = peak_while(&mut child)?;
        let status = peak.status;
        let took = started.elapsed().as_secs_f64();
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let phases = phases_field(&text);
        ends.count(phases.as_ref());
        let measured = format!(
            "peak={}MiB{} work={}",
            peak.kib / 1024,
            phases.map(|p| format!(" {}", p.field)).unwrap_or_default(),
            runtime_work(&text).unwrap_or("-")
        );
        let outcome = Outcome::of_exit(status.code());
        let line = match outcome {
            Outcome::Undecided => undecided_line(&text, row.cmd()),
            Outcome::Pass | Outcome::Fail => verdict_line(&text, row.cmd()),
        }
        .unwrap_or("no verdict line");
        let kept = keep_unless_pass(&task, &run, row, &log, outcome);
        let at = kept.as_ref().unwrap_or(&log);
        let note = kept
            .as_ref()
            .map(|k| format!(" (log {})", k.display()))
            .unwrap_or_default();
        eprintln!(
            "gates: {} {} ({took:.1}s {measured}) {line}{note}",
            row.name(),
            match outcome {
                Outcome::Pass => "ok",
                Outcome::Fail => "FAILED",
                Outcome::Undecided => "undecided",
            }
        );
        match outcome {
            Outcome::Pass => {}
            // the path leads: check quotes its first error at 160 chars (verdict.rs FIRST_ERROR_CHARS), and the verdict line lands in it
            Outcome::Fail => failed.push(format!("{} (log {}): {line}", row.name(), at.display())),
            Outcome::Undecided => undecided.push(undecided_note(row, line)),
        }
    }
    eprintln!("gates: watch ends {}", ends.line());
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
    fn count(&mut self, phases: Option<&Phases>) {
        let Some(p) = phases else { return };
        match WatchEnd::ALL.iter().position(|w| w.word() == p.end) {
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

/// An undecided gate's own line (`<cmd>: undecided <stage>: <why>`, window.rs `judge`).
/// The work dir a row's runtime staged under: its `runtime: work=<key>` line (atld_run.rs `Runtime::new`). None for a row that runs no atld runtime.
fn runtime_work(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("runtime: work="))
        .map(str::trim)
}

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
        t.count(Some(&p));
        t.count(phases_field("window: phases watch=1 end=window").as_ref());
        t.count(phases_field("window: phases watch=1 end=odd").as_ref());
        t.count(phases_field("sandbox: ok").as_ref());
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
