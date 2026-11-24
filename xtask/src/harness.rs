// SPDX-License-Identifier: MIT
//! The runtime harness (crate xtask-harness) as a process: `cargo xtask test|run-apk|remote` and check step `gl-proxies` run its binary.
//! It links the product crates, whose build scripts read fetched trees (bionic headers, native_bridge.h), so it builds only after `xtask fetch`; this tool must not link it.

use std::ffi::OsString;
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
/// The line a harness check step prints per gate it ran whose outcome was [`crate::verdict::Outcome::Undecided`]: `<gate> <why>`, named on check's verdict line.
pub const UNDECIDED_PREFIX: &str = "undecided-gate: ";

/// A step that ran without failing: `skipped` = checked only part of its input (why); `undecided` = gates it ran that could not judge on this host (`<gate> <why>`), neither pass nor fail.
/// `log`: the harness run's output (stdout, stderr), kept by [`settle_step`] when the run is undecided.
#[derive(Debug, Default)]
pub struct StepDone {
    pub skipped: Option<String>,
    pub undecided: Vec<String>,
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
            .map(str::to_owned)
            .collect(),
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
    let run = red_logs::run_id();
    let logs = out.join("check-gates");
    std::fs::create_dir_all(&logs).with_context(|| logs.display().to_string())?;
    let mut starvation = Starvation::default();
    starvation.record_starved(first.undecided.iter().find_map(|u| Supply::of_why(u)));
    starvation.record_attempt(judged_late(&first.log), false);
    let first_log = logs.join(format!("{step}.log"));
    std::fs::write(&first_log, &first.log).with_context(|| first_log.display().to_string())?;
    red_logs::keep(&task, red_logs::UNDECIDED, &run, &first_log)?;
    let grant = build_pool::acquire_rerun(ws, root, Class::Harness)?;
    let class = grant.class;
    let second = check_step(root, step, out);
    drop(grant);
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
    starvation.record_attempt(judged_late(&rerun.log), false);
    starvation.record_rerun(class, outcome);
    let rerun_log = logs.join(format!("{step}.rerun.log"));
    std::fs::write(&rerun_log, &rerun.log).with_context(|| rerun_log.display().to_string())?;
    let kept = match red_logs::bucket_of(outcome) {
        Some(bucket) => Some(red_logs::keep(&task, bucket, &run, &rerun_log)?),
        None => None,
    };
    let commit = crate::git::git(Some(root), &["rev-parse", "HEAD"])?;
    red_logs::ledger_append(
        &task,
        &[red_logs::Entry {
            run: run.as_str(),
            commit: commit.trim(),
            row: step,
            outcome,
            first: Some(Outcome::Undecided),
            stall: None,
            grant: class,
        }],
    )?;
    let streak = red_logs::trailing_undecided(&task)?
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

/// Starved gates' supply: the host's CPU (`starved cpu:`, xtask-harness starvation.rs) or the GPU (`host GPU starved`). None for a line naming neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supply {
    Cpu,
    Gpu,
}

impl Supply {
    /// The supply an undecided line, or its `<row> <why>` note, names.
    pub fn of_why(why: &str) -> Option<Self> {
        if why.contains("GPU starved") {
            Some(Self::Gpu)
        } else if why.contains("starved cpu") {
            Some(Self::Cpu)
        } else {
            None
        }
    }
}

/// Judged late waits in a gate's log: the `starvation: ` lines that refused to excuse a miss (`-> judged:`, starvation.rs `print_cpu`).
pub fn judged_late(log: &str) -> usize {
    log.lines()
        .filter(|l| l.starts_with("starvation: ") && l.contains(" -> judged: "))
        .count()
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

    /// Supply reads the wording starvation.rs prints: `starved cpu: ...` (CPU), `host GPU starved (...)` (GPU); a line naming neither is None.
    #[test]
    fn supply_reads_the_undecided_wording() {
        assert_eq!(
            Supply::of_why(
                "window: undecided slow-display: host GPU starved (window render median 300 ms, max 300 ms in the phase > bound 100 ms)"
            ),
            Some(Supply::Gpu)
        );
        assert_eq!(
            Supply::of_why(
                "window: undecided ime: starved cpu: event bound 1000 ms need 100 ms stalled 960 ms"
            ),
            Some(Supply::Cpu)
        );
        assert_eq!(Supply::of_why("window: FAILED ime: copy lost"), None);
    }

    /// Only `-> judged:` lines count: an `undecided` or `unknown` refusal is not a late wait the rule refused to excuse.
    #[test]
    fn judged_late_counts_the_refusals() {
        let log = "starvation: ime IME_WAIT -> judged: event bound 10000 ms need 100 ms stalled 5 ms\nstarvation: ime x -> undecided: starved cpu: event bound 1000 ms need 100 ms stalled 960 ms\nstarvation: ime y -> unknown: no gate scope samples\nwindow: FAILED ime\n";
        assert_eq!(judged_late(log), 1);
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
