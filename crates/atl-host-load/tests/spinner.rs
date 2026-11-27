// SPDX-License-Identifier: MIT
//! The sampler responds. A spinner pinned to one CPU in its own scope, with three burners pinned to that CPU, each in a scope of its own: four equal groups, so the spinner gets a quarter of the CPU and waits three quarters (stall share near 0.75). Alone it waits for nothing, unless the host runs its own work on that CPU: then the alone check is undecided (it prints the measured foreign share) and the burner check still judges.
//! The burners need their own scopes: the kernel schedules cgroups, and three burners in one group leave the spinner most of the CPU.
//! Needs the user manager (`systemd-run --user --scope`) and `taskset`. Without them the test fails: a return would be a green for what it never measured.
#![expect(
    clippy::disallowed_types,
    clippy::print_stderr,
    reason = "a host test: its spinners are scopes pinned to one CPU (they run no app, so no broker); it prints the measured shares as its evidence"
)]

use std::path::PathBuf;
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
/// Stall share of a spinner against one equal burner (1 of 2 groups). Three burners (expected 0.75) must exceed it: a sampler that misses two of them does not.
const ONE_BURNER: f64 = 0.5;
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
    let burdened = stall_share(cpu, 3);
    eprintln!(
        "spinner on cpu {cpu}: alone stall {:.3} (foreign {:.3}), with 3 burners {:.3} (foreign {:.3})",
        alone.stall, alone.foreign, burdened.stall, burdened.foreign
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
    // foreign load beyond the burners only lowers the spinner's share, so this check needs no premise
    assert!(
        burdened.stall >= ONE_BURNER,
        "with 3 burners (foreign {:.3}) the stall share is {:.3}, expected near 0.75",
        burdened.foreign,
        burdened.stall
    );
}

/// One measurement over [`RUN`], each a share of its wall time: the spinner scope's counted stall and the foreign CPU on its CPU (burners and anything else the host ran there).
struct Shares {
    stall: f64,
    foreign: f64,
}

/// A spinner in its own scope over [`RUN`], with `burners` burners, each in its own scope, on the same CPU.
/// Sampling starts once every burner is in its scope: under host load `systemd-run` takes long to place one, and intervals before it burns have no foreign CPU, so they would not count.
fn stall_share(cpu: u32, burners: usize) -> Shares {
    let (mut spinner, unit) = spawn_scoped(cpu, "spin");
    let scope: PathBuf = scope_of(spinner.id(), &unit);
    let mut others: Vec<Child> = (0..burners)
        .map(|_| {
            let (burner, unit) = spawn_scoped(cpu, "burn");
            scope_of(burner.id(), &unit);
            burner
        })
        .collect();
    let sampler = Sampler::start_for(Some(scope.as_path()), Some(&[cpu]));
    thread::sleep(RUN);
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
    }
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
