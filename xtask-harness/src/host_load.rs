// SPDX-License-Identifier: MIT
//! `xtask test host-load`: the starvation rule (starvation.rs) on a process tree the gate controls, no atld (gate-starved-red-design H4). Each case is its own gate process
//! (`test host-load --case <c>`, so its own `atl-gate-<pid>.scope` and sampler), pinned to one CPU with `taskset`: its sampler's CPU set is that CPU, so foreign load is what shares it.
//! The case starts a worker (`host-load-child`, in its scope) and waits `host-load-event` for the worker's event line: bound [`BOUND`], need [`NEED`].
//! Burners are `host-load-child --role burn` loops pinned to the same CPU, each in its own sibling scope (`systemd-run --user --scope -p CPUWeight=`): at [`STARVE_WEIGHT`] against the
//! case scope's default 100 the worker gets about 1 % of the CPU, so its 50 ms of work takes about 5 s; at the default weight ("under load") it gets about half.
//!
//! Cases and their expected outcomes ([`Case::expect`]): starved -> undecided (exit 4); no event, no load -> FAIL; worker asleep under load -> FAIL (a sleeper adds no stall);
//! crash under load -> FAIL (a death); wrong value under load -> FAIL; self-inflicted (64 spinners in the case's own scope, no burners) -> FAIL (own CPU, not foreign); late on a quiet host
//! (sleeps past the bound, then works) -> FAIL; pass under equal-weight load -> pass. Without `--case` the gate runs every case and passes when each ends as expected.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::time::{ClockId, clock_gettime};

use crate::starvation;
use crate::wait::{Miss, Wait, until_child};

/// The case's wait: 1 s, against a quiet need of 100 ms (the worker's 50 ms of CPU work, doubled for spawn and the log write). Design H4's values.
const BOUND: Duration = Duration::from_millis(1000);
const NEED: Duration = Duration::from_millis(100);
/// The worker's CPU work before its event: half the need.
const WORK: Duration = Duration::from_millis(50);
/// A burner scope's CPUWeight when it starves the case: cgroup v2's maximum, against the case scope's default 100, so the case gets 100 / 10100 = 1 % of the shared CPU.
pub(crate) const STARVE_WEIGHT: u32 = 10_000;
/// The default CPUWeight (systemd): an equal share.
const EQUAL_WEIGHT: u32 = 100;
/// Time for a burner to start spinning (a systemd-run scope start and an exec) before the worker starts: an unstarted burner leaves the worker its CPU.
const BURN_SETTLE: Duration = Duration::from_millis(300);
/// The late case's worker sleeps past the bound by half of it before it works.
const LATE_SLEEP: Duration = Duration::from_millis(1500);
/// Spinner threads in the self-inflicted case (design H4).
const SPINNERS: usize = 64;
/// The worker's event and the value it carries; the wrong case logs another value.
const EVENT: &str = "host-load: event value=";
const VALUE: u32 = 42;

#[derive(clap::Args)]
pub struct Args {
    /// Run one case in this process (the gate runs each case so, pinned to one CPU); without it every case runs and is compared with its expected outcome.
    #[arg(long, value_enum, hide = true)]
    case: Option<Case>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Starved,
    NoEvent,
    AsleepUnderLoad,
    CrashUnderLoad,
    WrongUnderLoad,
    SelfInflicted,
    LateQuiet,
    PassUnderLoad,
}

/// A case's outcome, as its exit code says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Undecided,
}

impl Case {
    const ALL: [Self; 8] = [
        Self::Starved,
        Self::NoEvent,
        Self::AsleepUnderLoad,
        Self::CrashUnderLoad,
        Self::WrongUnderLoad,
        Self::SelfInflicted,
        Self::LateQuiet,
        Self::PassUnderLoad,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Starved => "starved",
            Self::NoEvent => "no-event",
            Self::AsleepUnderLoad => "asleep-under-load",
            Self::CrashUnderLoad => "crash-under-load",
            Self::WrongUnderLoad => "wrong-under-load",
            Self::SelfInflicted => "self-inflicted",
            Self::LateQuiet => "late-quiet",
            Self::PassUnderLoad => "pass-under-load",
        }
    }

    fn expect(self) -> Outcome {
        match self {
            Self::Starved => Outcome::Undecided,
            Self::PassUnderLoad => Outcome::Pass,
            _ => Outcome::Fail,
        }
    }

    /// The burners' weight, None without burners.
    fn burners(self) -> Option<u32> {
        match self {
            Self::Starved => Some(STARVE_WEIGHT),
            // equal weight, not starving: at 1 % the worker's own start (exec, abort, a write) and the case's 50 ms log poll wait for the CPU long enough to run out the 1 s bound,
            // so the run is starved by the rule's own measure (asleep: 805 ms stalled of 1000, 2026-11-24) and no longer tests a death, a wrong value or a sleeper
            Self::AsleepUnderLoad
            | Self::CrashUnderLoad
            | Self::WrongUnderLoad
            | Self::PassUnderLoad => Some(EQUAL_WEIGHT),
            Self::NoEvent | Self::SelfInflicted | Self::LateQuiet => None,
        }
    }

    fn role(self) -> Role {
        match self {
            Self::Starved | Self::PassUnderLoad => Role::Work,
            Self::NoEvent | Self::AsleepUnderLoad => Role::Silent,
            Self::CrashUnderLoad => Role::Abort,
            Self::WrongUnderLoad => Role::Wrong,
            Self::SelfInflicted => Role::Spin,
            Self::LateQuiet => Role::Late,
        }
    }
}

/// What a `host-load-child` process does.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// [`WORK`] of CPU, then the event.
    Work,
    /// Never logs: sleeps.
    Silent,
    /// Aborts at once.
    Abort,
    /// Logs the event with a wrong value at once.
    Wrong,
    /// Sleeps [`LATE_SLEEP`], then works.
    Late,
    /// Starts [`SPINNERS`] spinning threads, then works.
    Spin,
    /// Spins forever (a burner).
    Burn,
}

#[derive(clap::Args)]
pub struct ChildArgs {
    #[arg(long, value_enum)]
    role: Role,
    /// Where the event line goes.
    #[arg(long)]
    log: Option<PathBuf>,
}

/// The worker and burner process (`xtask-harness host-load-child`): runs its role; never returns for `silent`, `burn`.
pub fn child(args: &ChildArgs) -> Result<ExitCode> {
    let event = |value: u32| -> Result<ExitCode> {
        let log = args.log.as_ref().context("--log")?;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(log)?;
        writeln!(f, "{EVENT}{value}")?;
        Ok(ExitCode::SUCCESS)
    };
    match args.role {
        Role::Work => {
            spend(WORK);
            event(VALUE)
        }
        Role::Late => {
            std::thread::sleep(LATE_SLEEP);
            spend(WORK);
            event(VALUE)
        }
        Role::Spin => {
            for _ in 0..SPINNERS {
                std::thread::spawn(|| spend(Duration::MAX));
            }
            spend(WORK);
            event(VALUE)
        }
        Role::Wrong => event(VALUE - 1),
        Role::Abort => std::process::abort(),
        Role::Silent => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        Role::Burn => {
            spend(Duration::MAX);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Busy until this thread has run `cpu` (its own CPU clock, so time it waited for the CPU does not count).
fn spend(cpu: Duration) {
    let ran = || {
        let t = clock_gettime(ClockId::ThreadCPUTime);
        Duration::new(t.tv_sec.unsigned_abs(), t.tv_nsec as u32)
    };
    let start = ran();
    while ran().saturating_sub(start) < cpu {
        std::hint::spin_loop();
    }
}

/// Burner loops, each pinned to one CPU in its own sibling scope at one weight; killed on drop. A shared scope would split its weight over its CPUs, so each burner has its own.
pub(crate) struct Burners(Vec<Child>);

impl Burners {
    pub(crate) fn start(cpus: &[u32], weight: u32) -> Result<Self> {
        let exe = std::env::current_exe().context("the harness's own path")?;
        let mut burners = Self(Vec::new());
        for cpu in cpus {
            let child = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "--collect"])
                .arg(format!("--property=CPUWeight={weight}"))
                .args(["--", "taskset", "-c", &cpu.to_string()])
                .arg(&exe)
                .args(["host-load-child", "--role", "burn"])
                .stdout(Stdio::null())
                .spawn()
                .context("systemd-run burner")?;
            burners.0.push(child);
        }
        std::thread::sleep(BURN_SETTLE);
        for b in &mut burners.0 {
            if let Some(status) = b.try_wait()? {
                anyhow::bail!("a burner ended at once ({status}): no scope with CPUWeight here");
            }
        }
        println!(
            "host-load: {} burner(s) at CPUWeight={weight} on cpu {cpus:?}",
            cpus.len()
        );
        Ok(burners)
    }
}

impl Drop for Burners {
    fn drop(&mut self) {
        for b in &mut self.0 {
            let _ = b.kill();
            let _ = b.wait();
        }
    }
}

pub fn run(args: &Args) -> Result<ExitCode> {
    match args.case {
        Some(case) => one(case),
        None => every(),
    }
}

/// Every case in its own pinned gate process, each compared with its expected outcome.
fn every() -> Result<ExitCode> {
    let cpu = *atl_host_load::allowed_cpus()
        .context("this process's CPU set")?
        .last()
        .context("an empty CPU set")?;
    let exe = std::env::current_exe().context("the harness's own path")?;
    let mut fails = Vec::new();
    for case in Case::ALL {
        let out = Command::new("taskset")
            .args(["-c", &cpu.to_string()])
            .arg(&exe)
            .args(["test", "host-load", "--case", case.name()])
            .output()
            .with_context(|| format!("taskset {}", case.name()))?;
        // indented: the case's own `host-load:` verdict is not this gate's (check_gates reads the gate's lines)
        for line in String::from_utf8_lossy(&out.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&out.stderr).lines())
        {
            println!("  {line}");
        }
        let got = match out.status.code() {
            Some(0) => Some(Outcome::Pass),
            Some(1) => Some(Outcome::Fail),
            Some(c) if c == i32::from(crate::verdict::UNDECIDED_EXIT) => Some(Outcome::Undecided),
            _ => None,
        };
        let want = case.expect();
        if got == Some(want) {
            println!("host-load: case {} {want:?} as expected", case.name());
        } else {
            fails.push(format!(
                "case {}: want {want:?}, got {:?} ({})",
                case.name(),
                got,
                out.status
            ));
        }
    }
    if fails.is_empty() {
        println!(
            "host-load: pass ({} cases on cpu {cpu}: a starved wait is undecided, every real red stays red)",
            Case::ALL.len()
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", crate::verdict::gate("host-load", &fails));
    Ok(ExitCode::FAILURE)
}

/// One case in this (pinned) gate process.
fn one(case: Case) -> Result<ExitCode> {
    let cpus = atl_host_load::allowed_cpus().context("this process's CPU set")?;
    let dir = std::env::temp_dir().join(format!("atl-host-load-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let log = dir.join("worker.log");
    let result = judge(case, &cpus, &log);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn judge(case: Case, cpus: &[u32], log: &Path) -> Result<ExitCode> {
    let burners = case
        .burners()
        .map(|weight| Burners::start(cpus, weight))
        .transpose()?;
    let exe = std::env::current_exe().context("the harness's own path")?;
    let mut worker = Command::new(exe)
        .arg("host-load-child")
        .args(["--role", role_name(case.role())])
        .arg("--log")
        .arg(log)
        .spawn()
        .context("host-load-child")?;
    let want = |t: &str| t.contains(EVENT);
    let mut wait = Wait::log(
        "host-load-event",
        format!("no event within {BOUND:?}"),
        BOUND,
        log,
        &want,
    );
    wait.need = Some(NEED);
    let outcome = until_child(&wait, &mut worker);
    let _ = worker.kill();
    let _ = worker.wait();
    // the burners starve this process too (its sampler, its judge): the samples past the wait come once they end
    drop(burners);
    let fault = match outcome {
        Ok(_) => {
            let text = atl_diag::logfiles::read(log);
            let want = format!("{EVENT}{VALUE}");
            if text.contains(&want) {
                println!("host-load: pass {} ({})", case.name(), text.trim());
                return Ok(ExitCode::SUCCESS);
            }
            Miss::Wrong(format!("want `{want}`, got `{}`", text.trim()))
        }
        Err(miss) => miss,
    };
    let samples = starvation::live();
    let run = starvation::Run::cpu(case.name(), samples.as_ref());
    if let Some(why) = starvation::red(&run, &[&fault], "") {
        return Ok(starvation::undecided("host-load", case.name(), &why));
    }
    println!(
        "{}",
        crate::verdict::gate("host-load", &[format!("{}: {}", case.name(), fault.text())])
    );
    Ok(ExitCode::FAILURE)
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Work => "work",
        Role::Silent => "silent",
        Role::Abort => "abort",
        Role::Wrong => "wrong",
        Role::Late => "late",
        Role::Spin => "spin",
        Role::Burn => "burn",
    }
}
