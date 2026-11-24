// SPDX-License-Identifier: MIT
//! The runtime harness (crate xtask-harness) as a process: `cargo xtask test|run-apk|remote` and check step `gl-proxies` run its binary.
//! It links the product crates, whose build scripts read fetched trees (bionic headers, native_bridge.h), so it builds only after `xtask fetch`; this tool must not link it.

use std::ffi::OsString;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Instant;

use crate::build_pool::{self, Class};
use crate::red_logs;
use crate::timing::secs;
use crate::verdict::Outcome;

use anyhow::{Context, Result, bail};
use atl_layout::Layout;
use serde::{Deserialize, Serialize};

/// The harness package; its binary has the same name.
const PACKAGE: &str = "xtask-harness";

/// Builds the harness (a no-op when current) and returns its binary.
/// The binary is in the running xtask's profile dir of the target dir cargo writes (out::cargo_target_dir).
fn build(root: &Path) -> Result<PathBuf> {
    let status = crate::cargo_command()
        .args(["build", "--quiet", "--package", PACKAGE])
        .current_dir(root)
        .status()
        .with_context(|| format!("spawning cargo build --package {PACKAGE}"))?;
    if !status.success() {
        let fetched = Layout::of(root)?.locate(Path::new("aosp")).dir.is_dir();
        let hint = if fetched {
            ""
        } else {
            " (no fetched AOSP tree: the product crates' build scripts read it; run `cargo xtask fetch --execute` first)"
        };
        bail!("cargo build --package {PACKAGE} exited with {status}{hint}");
    }
    // same profile dir as the running xtask, under the target dir cargo just wrote (a new checkout's xtask runs from its real `target/`, cargo writes the build dir's: out::pending_target)
    let exe = std::env::current_exe().context("locating the running xtask binary")?;
    let profile = exe
        .parent()
        .and_then(Path::file_name)
        .context("xtask binary has no profile dir")?;
    let bin = crate::out::cargo_target_dir(root)
        .join(profile)
        .join(PACKAGE);
    if !bin.is_file() {
        bail!(
            "{} missing after cargo build --package {PACKAGE}",
            bin.display()
        );
    }
    Ok(bin)
}

/// The harness command running `args` in `root`.
pub fn command<I, S>(root: &Path, args: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut cmd = Command::new(build(root)?);
    // a check step's grant reaches the harness's nested cargo runs through the environment (lib.rs grant_jobs)
    crate::grant_jobs(&mut cmd);
    cmd.args(args.into_iter().map(Into::into)).current_dir(root);
    Ok(cmd)
}

/// Runs harness command `name args...` with this process's stdio; its exit code is ours, its verdict line its own.
pub fn pass_through(root: &Path, name: &str, args: &[OsString]) -> Result<ExitCode> {
    let status = command(
        root,
        std::iter::once(OsString::from(name)).chain(args.iter().cloned()),
    )?
    .status()
    .context("spawning xtask-harness")?;
    // a signal leaves no code: report it as a failure, as cargo does
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

/// The line a harness check step prints for "not checked: <why>"; absent = checked.
pub const SKIP_PREFIX: &str = "skip: ";
/// The line a harness check step prints per gate it ran whose outcome was [`crate::verdict::Outcome::Undecided`]: an [`Undecided`] as JSON, named on check's verdict line.
pub const UNDECIDED_PREFIX: &str = "undecided-gate: ";
/// The line a gate prints per judgement the starvation rule makes (xtask-harness starvation.rs `print_ruling`): a [`Ruling`] as JSON. Read back by [`rulings`].
pub const RULING_PREFIX: &str = "starvation-ruling: ";
/// The line an undecided gate prints as its verdict's [`Ruling`] as JSON (starvation.rs `undecided`). Read back by [`undecided_verdict`].
pub const VERDICT_PREFIX: &str = "starvation-verdict: ";

/// A step that ran without failing: `skipped` = checked only part of its input (why); `undecided` = gates it ran that could not judge on this host, neither pass nor fail.
/// `log`: the harness run's output (stdout, stderr), kept by [`settle_step`] when the run is undecided.
#[derive(Debug, Default)]
pub struct StepDone {
    pub skipped: Option<String>,
    pub undecided: Vec<Undecided>,
    pub log: String,
}

impl From<Option<String>> for StepDone {
    fn from(skipped: Option<String>) -> Self {
        Self {
            skipped,
            ..Self::default()
        }
    }
}

/// Runs harness check step `step` on build dir `out`: its skip and undecided gates, an error with its output when it failed.
/// The harness's stderr (its phase report, lock waits, cargo's lines) shows as it arrives, so the check log has it; a failure's error carries it too.
pub fn check_step(root: &Path, step: &str, out: &Path) -> Result<StepDone> {
    let built = Instant::now();
    let mut cmd = command(
        root,
        [
            OsString::from("check-step"),
            step.into(),
            "--out".into(),
            out.into(),
        ],
    )?;
    let build_took = built.elapsed();
    let started = Instant::now();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning xtask-harness check-step {step}"))?;
    let stderr = child
        .stderr
        .take()
        .context("xtask-harness stderr is not piped")?;
    let shown = thread::spawn(move || show_and_keep(stderr));
    let run = child
        .wait_with_output()
        .with_context(|| format!("waiting for xtask-harness check-step {step}"))?;
    let stderr = shown.join().unwrap_or_default();
    // outside the harness's own phase report: the xtask-side build of the harness (a no-op when current) and the process's wall time
    eprintln!(
        "check-step {step}: harness build {}, harness process {}",
        secs(build_took),
        secs(started.elapsed())
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    if !run.status.success() {
        bail!("{}\n{}", stdout.trim_end(), stderr.trim_end());
    }
    Ok(StepDone {
        skipped: stdout
            .lines()
            .find_map(|l| l.strip_prefix(SKIP_PREFIX))
            .map(str::to_owned),
        undecided: stdout
            .lines()
            .filter_map(|l| l.strip_prefix(UNDECIDED_PREFIX))
            .map(|json| {
                serde_json::from_str::<Undecided>(json)
                    .with_context(|| format!("{step}: unreadable undecided line: {json}"))
            })
            .collect::<Result<_>>()?,
        log: format!("{stdout}{stderr}"),
    })
}

/// A harness step whose run ended with undecided gates is rerun once, alone (gate-starved-red-design 2.7). `first` is that run's result. check.rs runs the step under its own grant and must have released it before this call (a nested grant is refused).
/// The rerun takes the pool's quiet grant when the pool is idle, else the harness class; its outcome is the step's. The first log is kept in the undecided bucket, the rerun's in its outcome's bucket (red_logs.rs). The ledger line and the ceiling are as for the gate rows (check_gates.rs).
pub fn settle_step(
    root: &Path,
    ws: &Path,
    step: &str,
    out: &Path,
    first: StepDone,
) -> Result<StepDone> {
    if first.undecided.is_empty() {
        return Ok(first);
    }
    let task = red_logs::task_dir(root)?;
    let commit = crate::git::git(Some(root), &["rev-parse", "HEAD"])?;
    settle_once(
        &task,
        &red_logs::run_id(),
        commit.trim(),
        step,
        out,
        first,
        || {
            let grant = build_pool::acquire_rerun(ws, root, Class::Harness)?;
            let class = grant.class;
            let second = check_step(root, step, out);
            drop(grant);
            Ok((class, second))
        },
    )
}

/// [`settle_step`] without the grant and the process: `rerun` runs the step once more and returns the grant's class with the result. The first log and the rerun's go under `out`, the ledger line and the ceiling under `task`
/// (red_logs.rs); `commit` is the checkout's HEAD. A test drives it with a fake `rerun`, the only part that spawns.
fn settle_once(
    task: &Path,
    run: &str,
    commit: &str,
    step: &str,
    out: &Path,
    first: StepDone,
    rerun: impl FnOnce() -> Result<(&'static str, Result<StepDone>)>,
) -> Result<StepDone> {
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let supply = first.undecided.iter().find_map(Undecided::supply);
    let mut starvation = Starvation::default();
    starvation.record_starved(supply);
    starvation.record_attempt(judged_in(&first.log)?, false);
    let first_log = logs.join(format!("{step}.log"));
    std::fs::write(&first_log, &first.log).with_context(|| first_log.display().to_string())?;
    red_logs::keep(task, red_logs::UNDECIDED, run, &first_log)?;
    let (class, second) = rerun()?;
    let (outcome, rerun) = match second {
        Ok(done) if done.undecided.is_empty() => (Outcome::Pass, done),
        Ok(done) => (Outcome::Undecided, done),
        Err(e) => (
            Outcome::Fail,
            StepDone {
                log: format!("{e:#}"),
                ..StepDone::default()
            },
        ),
    };
    starvation.record_attempt(judged_in(&rerun.log)?, false);
    starvation.record_rerun(class, outcome);
    let rerun_log = logs.join(format!("{step}.rerun.log"));
    std::fs::write(&rerun_log, &rerun.log).with_context(|| rerun_log.display().to_string())?;
    let kept = match red_logs::bucket_of(outcome) {
        Some(bucket) => Some(red_logs::keep(task, bucket, run, &rerun_log)?),
        None => None,
    };
    red_logs::ledger_append(
        task,
        &[red_logs::Entry {
            run,
            commit,
            row: step,
            outcome,
            first: Some(Outcome::Undecided),
            stall: None,
            grant: class,
            supply,
        }],
    )?;
    let streak = red_logs::trailing_undecided(task)?
        .remove(step)
        .unwrap_or_default();
    let ceiling = streak.len() >= red_logs::CEILING;
    starvation.ceiling = usize::from(ceiling);
    eprintln!("{}", starvation.line(step));
    if ceiling {
        let last = &streak[streak.len() - red_logs::CEILING..];
        bail!(
            "{step} undecided in {} consecutive runs of this checkout ({}): the ceiling fails the step",
            red_logs::CEILING,
            last.join(", ")
        );
    }
    match outcome {
        Outcome::Pass => Ok(StepDone {
            skipped: rerun.skipped,
            ..StepDone::default()
        }),
        Outcome::Undecided => Ok(StepDone {
            skipped: rerun.skipped,
            undecided: rerun.undecided,
            log: String::new(),
        }),
        Outcome::Fail => {
            let at = kept.as_deref().unwrap_or(&rerun_log);
            bail!("{step}: starved, rerun FAILED (log {})", at.display())
        }
    }
}

/// Which supply starved a gate (starvation.rs): the host's CPU over a declared wait, or the window's GPU render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Supply {
    Cpu,
    Gpu,
}

impl Supply {
    /// The ledger's word for it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }
}

/// The starvation rule's judgement on a miss (starvation.rs): `Starved` = undecided, by the supply that took the margin; `Judged` (a real miss) and `Unknown` (nothing measured) = refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Judgement {
    Starved(Supply),
    Judged,
    Unknown,
}

impl Judgement {
    /// The word a `starvation:` line prints for it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Starved(_) => "undecided",
            Self::Judged => "judged",
            Self::Unknown => "unknown",
        }
    }
}

/// A judgement and its measures. `text` is what the rule measured, without the supply's wording; [`fmt::Display`] renders the human form (`starved cpu: <text>`, `host GPU starved (<text>)`, or `<text>` alone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    pub judgement: Judgement,
    pub text: String,
}

impl Ruling {
    pub fn starved(supply: Supply, text: impl Into<String>) -> Self {
        Self {
            judgement: Judgement::Starved(supply),
            text: text.into(),
        }
    }

    pub fn judged(text: impl Into<String>) -> Self {
        Self {
            judgement: Judgement::Judged,
            text: text.into(),
        }
    }

    pub fn unknown(text: impl Into<String>) -> Self {
        Self {
            judgement: Judgement::Unknown,
            text: text.into(),
        }
    }

    /// The supply that starved it; None unless undecided.
    pub fn supply(&self) -> Option<Supply> {
        match self.judgement {
            Judgement::Starved(supply) => Some(supply),
            Judgement::Judged | Judgement::Unknown => None,
        }
    }
}

impl fmt::Display for Ruling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.judgement {
            Judgement::Starved(Supply::Cpu) => write!(f, "starved cpu: {}", self.text),
            Judgement::Starved(Supply::Gpu) => write!(f, "host GPU starved ({})", self.text),
            Judgement::Judged | Judgement::Unknown => f.write_str(&self.text),
        }
    }
}

/// A gate row that ended undecided: its name and the verdict's ruling. `ruling` None: the run printed no verdict line (a defect; the row still counts as undecided, with no supply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Undecided {
    pub row: String,
    pub ruling: Option<Ruling>,
}

impl Undecided {
    /// The supply that starved the row's verdict.
    pub fn supply(&self) -> Option<Supply> {
        self.ruling.as_ref().and_then(Ruling::supply)
    }
}

/// `<row> <why>`: the note check's verdict line names.
impl fmt::Display for Undecided {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.ruling {
            Some(ruling) => write!(f, "{} {ruling}", self.row),
            None => write!(f, "{} undecided (no verdict line)", self.row),
        }
    }
}

/// The rulings a gate's output printed, in order ([`RULING_PREFIX`] lines). A line that does not parse is an error: the protocol broke, and a guessed judgement would decide a row.
pub fn rulings(log: &str) -> Result<Vec<Ruling>> {
    log.lines()
        .filter_map(|l| l.strip_prefix(RULING_PREFIX))
        .map(|json| {
            serde_json::from_str::<Ruling>(json)
                .with_context(|| format!("unreadable starvation ruling: {json}"))
        })
        .collect()
}

/// The verdict a gate printed (its last [`VERDICT_PREFIX`] line). None when it printed none.
pub fn undecided_verdict(log: &str) -> Result<Option<Ruling>> {
    log.lines()
        .filter_map(|l| l.strip_prefix(VERDICT_PREFIX))
        .next_back()
        .map(|json| {
            serde_json::from_str::<Ruling>(json)
                .with_context(|| format!("unreadable starvation verdict: {json}"))
        })
        .transpose()
}

/// Judged late waits in a gate's log: the rulings the rule refused to excuse (starvation.rs `print_ruling`).
pub fn judged_in(log: &str) -> Result<usize> {
    Ok(rulings(log)?
        .iter()
        .filter(|r| r.judgement == Judgement::Judged)
        .count())
}

/// One gates step's starvation counters (gate-starved-red-design 2.7), over every attempt of its rows. Starved: first attempts that ended undecided.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Starvation {
    pub starved: usize,
    pub cpu: usize,
    pub gpu: usize,
    /// reruns that held the pool's quiet grant / any other grant
    pub quiet: usize,
    pub shared: usize,
    /// reruns' outcomes
    pub ok: usize,
    pub failed: usize,
    pub undecided: usize,
    /// judged late waits over every attempt
    pub judged: usize,
    /// attempts whose host-load record was unknown
    pub unknown: usize,
    /// rows at the ceiling this step
    pub ceiling: usize,
}

impl Starvation {
    /// A first attempt that ended undecided, by its supply.
    pub fn record_starved(&mut self, supply: Option<Supply>) {
        self.starved += 1;
        match supply {
            Some(Supply::Cpu) => self.cpu += 1,
            Some(Supply::Gpu) => self.gpu += 1,
            None => {}
        }
    }

    /// A rerun: the grant class it held (`quiet` = the whole pool) and its outcome.
    pub fn record_rerun(&mut self, grant: &str, outcome: Outcome) {
        if grant == "quiet" {
            self.quiet += 1;
        } else {
            self.shared += 1;
        }
        match outcome {
            Outcome::Pass => self.ok += 1,
            Outcome::Fail => self.failed += 1,
            Outcome::Undecided => self.undecided += 1,
        }
    }

    /// One run's log: its judged late waits, and whether its host-load record was unknown.
    pub fn record_attempt(&mut self, judged: usize, host_unknown: bool) {
        self.judged += judged;
        self.unknown += usize::from(host_unknown);
    }

    /// The counter line of step `who` (`gates`, or a harness step's name).
    pub fn line(&self, who: &str) -> String {
        format!(
            "{who}: starvation starved={} (cpu={} gpu={}) rerun quiet={} shared={} -> ok={} failed={} undecided={}; judged late={}; unknown={}; ceiling={}",
            self.starved,
            self.cpu,
            self.gpu,
            self.quiet,
            self.shared,
            self.ok,
            self.failed,
            self.undecided,
            self.judged,
            self.unknown,
            self.ceiling
        )
    }
}

/// Copies `from` to our stderr line by line as it arrives; returns what it copied.
fn show_and_keep(from: impl Read) -> String {
    let mut kept = String::new();
    let mut reader = BufReader::new(from);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&raw);
                let mut err = std::io::stderr().lock();
                // a closed stderr loses the echo, not the capture
                let _ = err.write_all(line.as_bytes()).and_then(|()| err.flush());
                kept.push_str(&line);
            }
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The human form renders from the typed ruling: the supply's wording is added by `Display`, never matched in text.
    #[test]
    fn a_ruling_renders_its_supply_wording_once() {
        let cpu = Ruling::starved(
            Supply::Cpu,
            "event bound 1000 ms need 100 ms stalled 960 ms",
        );
        assert_eq!(
            cpu.to_string(),
            "starved cpu: event bound 1000 ms need 100 ms stalled 960 ms"
        );
        assert_eq!(cpu.supply(), Some(Supply::Cpu));
        let gpu = Ruling::starved(
            Supply::Gpu,
            "window render median 300 ms, max 300 ms in the phase > bound 100 ms",
        );
        assert_eq!(
            gpu.to_string(),
            "host GPU starved (window render median 300 ms, max 300 ms in the phase > bound 100 ms)"
        );
        assert_eq!(gpu.supply(), Some(Supply::Gpu));
        let judged = Ruling::judged("event bound 10000 ms need 100 ms stalled 5 ms");
        assert_eq!(judged.supply(), None);
        assert_eq!(
            judged.to_string(),
            "event bound 10000 ms need 100 ms stalled 5 ms"
        );
        assert_eq!(
            Ruling::unknown("x: no gate scope samples").to_string(),
            "x: no gate scope samples"
        );
    }

    /// The wire form: one JSON object per line, the judgement tagged by its supply (`{"starved":"cpu"}`), read back as the same value.
    #[test]
    fn a_ruling_round_trips_through_its_line() {
        let ruling = Ruling::starved(
            Supply::Cpu,
            "event bound 1000 ms need 100 ms stalled 960 ms",
        );
        let json = serde_json::to_string(&ruling).expect("json");
        assert_eq!(
            json,
            r#"{"judgement":{"starved":"cpu"},"text":"event bound 1000 ms need 100 ms stalled 960 ms"}"#
        );
        let log = format!("window: undecided ime: {ruling}\n{RULING_PREFIX}{json}\n");
        assert_eq!(rulings(&log).expect("rulings"), [ruling]);
    }

    /// Only `Judged` rulings count as judged late: an undecided or unknown one is not a late wait the rule refused to excuse. An indented line (a case's output echoed by host-load) is not the gate's own.
    #[test]
    fn judged_counts_only_the_judged_rulings() {
        let line = |r: &Ruling| {
            format!(
                "{RULING_PREFIX}{}\n",
                serde_json::to_string(r).expect("json")
            )
        };
        let log = [
            line(&Ruling::judged(
                "event bound 10000 ms need 100 ms stalled 5 ms",
            )),
            line(&Ruling::starved(
                Supply::Cpu,
                "event bound 1000 ms need 100 ms stalled 960 ms",
            )),
            line(&Ruling::unknown("no gate scope samples")),
            format!(
                "  {RULING_PREFIX}{}\n",
                serde_json::to_string(&Ruling::judged("a case's own")).expect("json")
            ),
        ]
        .concat();
        assert_eq!(judged_in(&log).expect("count"), 1);
    }

    /// A ruling line that does not parse is an error: a guessed judgement would decide a row.
    #[test]
    fn an_unreadable_ruling_line_is_an_error() {
        assert!(rulings(&format!("{RULING_PREFIX}{{\"judgement\":\"maybe\"}}\n")).is_err());
    }

    /// The verdict is the gate's last verdict line; a gate that printed none has no verdict.
    #[test]
    fn the_verdict_is_the_last_verdict_line() {
        let first = Ruling::starved(Supply::Gpu, "render max 205 ms > 100 ms");
        let last = Ruling::starved(
            Supply::Cpu,
            "event bound 1000 ms need 100 ms stalled 960 ms",
        );
        let log = format!(
            "{VERDICT_PREFIX}{}\n{VERDICT_PREFIX}{}\n",
            serde_json::to_string(&first).expect("json"),
            serde_json::to_string(&last).expect("json")
        );
        assert_eq!(undecided_verdict(&log).expect("verdict"), Some(last));
        assert_eq!(undecided_verdict("window: pass x\n").expect("none"), None);
    }

    /// An undecided row names itself and its verdict's wording and supply; a row whose run printed no verdict says so, with no supply. Both survive the JSON line check_step reads.
    #[test]
    fn an_undecided_row_names_its_ruling() {
        let row = Undecided {
            row: "window-slow-display".to_owned(),
            ruling: Some(Ruling::starved(Supply::Gpu, "render max 205 ms > 100 ms")),
        };
        assert_eq!(
            row.to_string(),
            "window-slow-display host GPU starved (render max 205 ms > 100 ms)"
        );
        assert_eq!(row.supply(), Some(Supply::Gpu));
        let json = serde_json::to_string(&row).expect("json");
        assert_eq!(serde_json::from_str::<Undecided>(&json).expect("back"), row);
        let bare = Undecided {
            row: "drive-tap".to_owned(),
            ruling: None,
        };
        assert_eq!(bare.to_string(), "drive-tap undecided (no verdict line)");
        assert_eq!(bare.supply(), None);
    }

    /// `out` (the logs) and `task` (kept logs, ledger) under a fresh temp dir named for the test; the test removes `out`'s parent.
    fn settle_dirs(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("xtask-settle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        (base.join("out"), base.join("task"))
    }

    /// A drive-tap first attempt that ended undecided on `supply`.
    fn undecided_step(supply: Supply) -> StepDone {
        StepDone {
            undecided: vec![Undecided {
                row: "drive-tap".to_owned(),
                ruling: Some(Ruling::starved(
                    supply,
                    "drive-tap wait bound 120000 ms need 900 ms stalled 3000 ms",
                )),
            }],
            log: "drive-tap: the first shot was blank\n".to_owned(),
            ..StepDone::default()
        }
    }

    /// The undecided path makes the drive-tap settle fire (the step's rerun is the only part faked): a starved first attempt, a passing rerun, the first log kept beside it, one ledger line with the first attempt's supply.
    #[test]
    fn a_starved_step_is_rerun_once_and_settles_on_the_rerun() {
        let (out, task) = settle_dirs("pass");
        let settled = settle_once(
            &task,
            "run-1",
            "c0ffee",
            "drive-tap",
            &out,
            undecided_step(Supply::Gpu),
            || Ok(("quiet", Ok(StepDone::default()))),
        )
        .expect("a passing rerun settles the step");
        assert!(settled.undecided.is_empty());
        assert_eq!(
            std::fs::read_to_string(task.join(red_logs::LEDGER)).expect("ledger"),
            "run-1\tc0ffee\tdrive-tap\tpass\tundecided\t-\tquiet\tgpu\n"
        );
        assert!(out.join("check-gates").join("drive-tap.log").is_file());
        assert!(
            task.join(red_logs::UNDECIDED)
                .join("run-1")
                .join("drive-tap.log")
                .is_file()
        );
        std::fs::remove_dir_all(out.parent().expect("base")).expect("cleanup");
    }

    /// Undecided in two runs is not a failure; the third consecutive undecided run fails the step, naming the three runs (red_logs::CEILING).
    #[test]
    fn a_step_undecided_in_three_runs_fails_at_the_ceiling() {
        let (out, task) = settle_dirs("ceiling");
        for run in ["r1", "r2"] {
            let settled = settle_once(
                &task,
                run,
                "c0ffee",
                "drive-tap",
                &out,
                undecided_step(Supply::Cpu),
                || Ok(("harness", Ok(undecided_step(Supply::Cpu)))),
            )
            .expect("undecided twice is not a failure of its run");
            assert_eq!(settled.undecided.len(), 1);
        }
        let err = settle_once(
            &task,
            "r3",
            "c0ffee",
            "drive-tap",
            &out,
            undecided_step(Supply::Cpu),
            || Ok(("harness", Ok(undecided_step(Supply::Cpu)))),
        )
        .expect_err("the third run hits the ceiling");
        assert_eq!(
            format!("{err:#}"),
            "drive-tap undecided in 3 consecutive runs of this checkout (r1, r2, r3): the ceiling fails the step"
        );
        std::fs::remove_dir_all(out.parent().expect("base")).expect("cleanup");
    }

    /// A rerun that fails fails the step, and its log is kept in the red bucket.
    #[test]
    fn a_failing_rerun_fails_the_step() {
        let (out, task) = settle_dirs("red");
        let err = settle_once(
            &task,
            "run-1",
            "c0ffee",
            "drive-tap",
            &out,
            undecided_step(Supply::Gpu),
            || Ok(("quiet", Err(anyhow::anyhow!("the tap drive failed")))),
        )
        .expect_err("a red rerun fails the step");
        assert!(
            format!("{err:#}").starts_with("drive-tap: starved, rerun FAILED (log "),
            "{err:#}"
        );
        assert!(
            task.join(red_logs::RED)
                .join("run-1")
                .join("drive-tap.rerun.log")
                .is_file(),
            "the rerun's log is kept in the red bucket"
        );
        std::fs::remove_dir_all(out.parent().expect("base")).expect("cleanup");
    }

    /// Each counter sits in its own field of the line; a rerun on the whole pool is `quiet`, any other grant `shared`.
    #[test]
    fn counter_line_names_each_counter() {
        let mut s = Starvation::default();
        s.record_starved(Some(Supply::Cpu));
        s.record_starved(Some(Supply::Gpu));
        s.record_rerun("quiet", Outcome::Pass);
        s.record_rerun("harness", Outcome::Undecided);
        s.record_attempt(2, true);
        assert_eq!(
            s.line("gates"),
            "gates: starvation starved=2 (cpu=1 gpu=1) rerun quiet=1 shared=1 -> ok=1 failed=0 undecided=1; judged late=2; unknown=1; ceiling=0"
        );
    }
}
