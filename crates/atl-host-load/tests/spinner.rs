// SPDX-License-Identifier: MIT
//! The sampler responds. A spinner pinned to one CPU in its own scope, with three burners pinned to that CPU, each in a scope of its own: four equal groups, so the spinner gets a quarter of the CPU and waits three quarters (stall share near 0.75). Alone it waits for nothing.
//! The burners need their own scopes: the kernel schedules cgroups, and three burners in one group leave the spinner most of the CPU.
//! Needs the user manager (`systemd-run --user --scope`) and `taskset`. Without them the test prints that it skipped and returns: the record is unknown on such a host, never green.
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

/// A busy loop with no syscalls: the shell's `:` builtin.
const SPIN: &str = "while :; do :; done";
/// How long one measurement samples: 40 readings at 50 ms.
const RUN: Duration = Duration::from_secs(2);
/// How long a spinner may take to enter its scope before the test fails.
const ENTER: Duration = Duration::from_secs(5);
/// Poll step while waiting for the scope.
const POLL: Duration = Duration::from_millis(10);
/// Unique unit names across the measurements of one run.
static UNITS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn spinner_responds_to_foreign_load() {
    let probe = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("skipped: no user manager scope here (systemd-run --user --scope)");
        return;
    }
    let cpu = *allowed_cpus()
        .expect("Cpus_allowed_list")
        .last()
        .expect("at least one CPU");
    let alone = stall_share(cpu, 0);
    let burdened = stall_share(cpu, 3);
    eprintln!("spinner on cpu {cpu}: alone stall {alone:.3}, with 3 burners {burdened:.3}");
    assert!(
        alone < 0.1,
        "alone the spinner stalls {alone:.3}, expected near 0"
    );
    assert!(
        burdened >= 0.5,
        "with 3 burners the stall share is {burdened:.3}, expected near 0.75"
    );
}

/// The stall share of a spinner in its own scope over [`RUN`], with `burners` burners, each in its own scope, on the same CPU.
fn stall_share(cpu: u32, burners: usize) -> f64 {
    let (mut spinner, unit) = spawn_scoped(cpu, "spin");
    let scope: PathBuf = scope_of(spinner.id(), &unit);
    let sampler = Sampler::start_for(Some(scope.as_path()), Some(&[cpu]));
    let mut others: Vec<Child> = (0..burners).map(|_| spawn_scoped(cpu, "burn").0).collect();
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
    let wall_us = (span.to_ns - span.from_ns) as f64 / 1e3;
    stalled_us as f64 / wall_us
}

/// A spinner pinned to `cpu` in a new scope `atl-host-load-<role>-<pid>-<n>`, and that unit's name.
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
        .args(["--", "taskset", "-c", cpu.as_str(), "sh", "-c", SPIN])
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
