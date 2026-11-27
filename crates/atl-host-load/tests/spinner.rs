// SPDX-License-Identifier: MIT
//! The sampler responds. A spinner pinned to one CPU in its own scope, with three burners pinned to that CPU, each in a scope of its own: four equal groups, so the spinner gets a quarter of the CPU and waits three quarters. Alone it waits for nothing, unless the host runs its own work on that CPU: then the alone check is undecided (it prints the measured foreign share) and the burner check still judges.
//! The burner check judges the stall against the foreign CPU of the same intervals (an always-runnable spinner waits exactly while others run), not a fixed 0.75: host load can raise the foreign share (work on that CPU) or lower the burners' (a loaded host runs them late).
//! Its premise is the burners' own CPU (their scopes' `cpu.stat`, read apart from the sampler): none at all is a broken test (red); some but under [`ONE_BURNER`] is a host that kept them from their share (undecided, printed).
//! The burners need their own scopes: the kernel schedules cgroups, and three burners in one group leave the spinner most of the CPU.
//! Needs the user manager (`systemd-run --user --scope`) and `taskset`. Without them the test fails: a return would be a green for what it never measured.
#![expect(
    clippy::disallowed_types,
    clippy::print_stderr,
    reason = "a host test: its spinners are scopes pinned to one CPU (they run no app, so no broker); it prints the measured shares as its evidence"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use atl_host_load::{Sampler, Span, allowed_cpus, cgroup_dir};

/// A busy loop with no syscalls: the shell's `:` builtin. `ulimit -t` ends it after 60 s of CPU (the kernel's SIGKILL at the hard limit), so a leaked spinner stops on its own.
const SPIN: &str = "ulimit -t 60; while :; do :; done";
/// How long one measurement samples: 40 readings at 50 ms.
const RUN: Duration = Duration::from_secs(2);
/// How long a spinner may take to enter its scope before the test fails.
const ENTER: Duration = Duration::from_secs(5);
/// Poll step while waiting for the scope.
const POLL: Duration = Duration::from_millis(10);
/// Stall share of a spinner against one equal burner (1 of 2 groups). The burners' own share must reach it for the burner check to judge: three burners at their share get 0.75.
const ONE_BURNER: f64 = 0.5;
/// Burners in the burner check: four equal groups with the spinner.
const BURNERS: usize = 3;
/// How far the stall may sit from the foreign share: half of one group's share among four (1/8). A sampler that missed one burner's intervals would be a whole group (1/4) off.
const TRACK: f64 = 1.0 / (2.0 * (BURNERS as f64 + 1.0));
/// Alone bound: the host's other work on the CPU is the only stall, so it varies with the host. Below half the one-burner share, a lone spinner is not read as a loaded one.
/// Also the premise bound: a spinner waits only while foreign work runs, so foreign CPU under it keeps a working sampler's stall under it; foreign CPU at or past it decides nothing.
const ALONE_MAX: f64 = ONE_BURNER / 2.0;
/// Unique unit names across the measurements of one run.
static UNITS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn spinner_responds_to_foreign_load() {
    let probe = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .status();
    assert!(
        probe.is_ok_and(|s| s.success()),
        "no user manager scope here (systemd-run --user --scope): nothing can be measured"
    );
    let cpu = *allowed_cpus()
        .expect("Cpus_allowed_list")
        .last()
        .expect("at least one CPU");
    let alone = stall_share(cpu, 0);
    let burdened = stall_share(cpu, BURNERS);
    eprintln!(
        "spinner on cpu {cpu}: alone stall {:.3} (foreign {:.3}), with {BURNERS} burners {:.3} (foreign {:.3}, burners {:.3})",
        alone.stall, alone.foreign, burdened.stall, burdened.foreign, burdened.burners
    );
    // the alone premise is no foreign load; a host that ran its own work on the CPU (a parallel build) breaks it, and the spinner then rightly stalls: undecided, not red
    if alone.foreign < ALONE_MAX {
        assert!(
            alone.stall < ALONE_MAX,
            "alone (foreign {:.3}) the spinner stalls {:.3}, expected near 0",
            alone.foreign,
            alone.stall
        );
    } else {
        eprintln!(
            "spinner: alone undecided: foreign {:.3} >= {ALONE_MAX} on cpu {cpu} (stall {:.3}); the 3-burner check still judges",
            alone.foreign, alone.stall
        );
    }
    // the premise is the burners' own CPU: none ran -> the test measured no load (red); some, under their share -> the host kept it from them (undecided)
    assert!(
        burdened.burners > 0.0,
        "with {BURNERS} burners none ran on cpu {cpu} (foreign {:.3}, stall {:.3}): no load was measured",
        burdened.foreign,
        burdened.stall
    );
    if burdened.burners < ONE_BURNER {
        eprintln!(
            "spinner: burner check undecided: the burners got {:.3} < {ONE_BURNER} of cpu {cpu} (foreign {:.3}, stall {:.3})",
            burdened.burners, burdened.foreign, burdened.stall
        );
        return;
    }
    assert!(
        (burdened.stall - burdened.foreign).abs() < TRACK,
        "with {BURNERS} burners (their share {:.3}, foreign {:.3}) the stall share is {:.3}: {TRACK} or more off the foreign share",
        burdened.burners,
        burdened.foreign,
        burdened.stall
    );
}

/// One measurement over [`RUN`], each a share of its wall time: the spinner scope's counted stall, the foreign CPU on its CPU (burners and anything else the host ran there), the burners' own CPU.
struct Shares {
    stall: f64,
    foreign: f64,
    burners: f64,
}

/// A spinner in its own scope over [`RUN`], with `burners` burners, each in its own scope, on the same CPU.
/// Sampling starts once every process spins on `cpu` ([`spinning`]): under host load `systemd-run` places a process in its scope well before `setpriv` -> `taskset` -> `sh` have run, and a burner
/// not yet burning leaves the spinner the CPU.
fn stall_share(cpu: u32, burners: usize) -> Shares {
    let (mut spinner, unit) = spawn_scoped(cpu, "spin");
    let scope: PathBuf = scope_of(spinner.id(), &unit);
    spinning(spinner.id(), cpu);
    let (mut others, scopes): (Vec<Child>, Vec<PathBuf>) = (0..burners)
        .map(|_| {
            let (burner, unit) = spawn_scoped(cpu, "burn");
            let scope = scope_of(burner.id(), &unit);
            spinning(burner.id(), cpu);
            (burner, scope)
        })
        .unzip();
    let burned = || scopes.iter().map(|s| usage_us(s)).sum::<u64>();
    let sampler = Sampler::start_for(Some(scope.as_path()), Some(&[cpu]));
    let burned_from = burned();
    thread::sleep(RUN);
    let burned_us = burned().saturating_sub(burned_from);
    let samples = sampler.stop();
    for child in others.iter_mut().chain(std::iter::once(&mut spinner)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let span: Span = samples.span().expect("two or more samples");
    let stalled_us = samples
        .stalled(span)
        .expect("every interval has the scope and the CPU's busy time");
    let foreign_us = samples
        .foreign(span)
        .expect("the same intervals as the stall");
    let wall_us = (span.to_ns - span.from_ns) as f64 / 1e3;
    Shares {
        stall: stalled_us as f64 / wall_us,
        foreign: foreign_us as f64 / wall_us,
        burners: burned_us as f64 / RUN.as_micros() as f64,
    }
}

/// A scope's `usage_usec`: its CPU since it began.
fn usage_us(scope: &Path) -> u64 {
    std::fs::read_to_string(scope.join("cpu.stat"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("{}: no usage_usec", scope.display()))
}

/// Waits until `pid` spins on `cpu`: the shell past the exec chain (`systemd-run` -> `setpriv` -> `taskset` -> `sh`, one pid throughout), pinned, and running: its CPU ticks grew since it was
/// first seen so (a shell that is there but not yet in its loop has none to show).
fn spinning(pid: u32, cpu: u32) {
    let deadline = Instant::now() + ENTER;
    let want = cpu.to_string();
    let mut first: Option<u64> = None;
    loop {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let pinned = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
            .is_some_and(|l| l.trim() == want);
        if comm.trim() == "sh" && pinned {
            let now = ticks(pid);
            match first {
                Some(t) if now > t => return,
                Some(_) => {}
                None => first = Some(now),
            }
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} never spun on cpu {cpu}"
        );
        thread::sleep(POLL);
    }
}

/// `pid`'s utime + stime in clock ticks (`/proc/<pid>/stat` fields 14 and 15); 0 when unreadable.
fn ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // fields after `(comm)` start at field 3 (state): utime is the 12th of them
    stat.rsplit_once(')')
        .map(|(_, rest)| {
            rest.split_whitespace()
                .skip(11)
                .take(2)
                .filter_map(|v| v.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0)
}

/// A spinner pinned to `cpu` in a new scope `atl-host-load-<role>-<pid>-<n>`, and that unit's name. `setpriv --pdeathsig KILL` ends it with the test thread that spawned it, so a failed assertion leaves none.
fn spawn_scoped(cpu: u32, role: &str) -> (Child, String) {
    let unit = format!(
        "atl-host-load-{role}-{}-{}",
        std::process::id(),
        UNITS.fetch_add(1, Ordering::Relaxed)
    );
    let cpu = cpu.to_string();
    let child = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        .args([
            "--",
            "setpriv",
            "--pdeathsig",
            "KILL",
            "--",
            "taskset",
            "-c",
            cpu.as_str(),
            "sh",
            "-c",
            SPIN,
        ])
        .stdin(Stdio::null())
        .spawn()
        .expect("systemd-run spawns");
    (child, unit)
}

/// The scope named `unit` that `pid` entered. Until systemd-run moves it there, the pid sits in the caller's own scope, so the name is what must match.
fn scope_of(pid: u32, unit: &str) -> PathBuf {
    let want = format!("{unit}.scope");
    let deadline = Instant::now() + ENTER;
    loop {
        let scope = cgroup_dir(pid)
            .ok()
            .filter(|dir| dir.file_name().and_then(|n| n.to_str()) == Some(want.as_str()));
        if let Some(dir) = scope {
            return dir;
        }
        assert!(Instant::now() < deadline, "pid {pid} never entered {want}");
        thread::sleep(POLL);
    }
}
