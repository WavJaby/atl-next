// SPDX-License-Identifier: MIT
//! A managed row measured under systemd, as `RowRun::row` runs it (atl-host-load `scoped_command` + `supervise`): a normal exit, a plain SIGKILL, a kill at the leaf's own limit, a process that leaves the leaf, a gate that measures its leaf without relaunching, and the cleanup after the supervisor died.
//! Needs the user manager that makes scopes with `Delegate=yes`; without it the test fails (nothing can be measured).
#![expect(
    clippy::disallowed_types,
    reason = "a host test: it runs the harness's own supervisor under systemd-run"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use atl_host_load::{Events, Exit, Record};

const HARNESS: &str = env!("CARGO_BIN_EXE_xtask-harness");
const MIB: u64 = 1024 * 1024;

/// Runs `workload` as a managed row runs it: its scope, its supervisor, the placer. Returns the log's text and the record the reader takes from it.
fn managed(
    name: &str,
    memory_max: Option<u64>,
    workload: &[&str],
) -> (String, Result<Record, String>) {
    let log = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("managed-{name}.log"));
    let file = std::fs::File::create(&log).expect("the log is created");
    let unit = atl_host_load::new_unit();
    let mut cmd = atl_host_load::scoped_command(&unit);
    cmd.arg(HARNESS)
        .args(["scope-supervise", "--record"])
        .arg(&log);
    if let Some(max) = memory_max {
        cmd.arg("--memory-max").arg(max.to_string());
    }
    // the row's stdio is its log, as RowRun::row sets it: the workload's output and the record share it
    cmd.arg("--").args(workload);
    cmd.stdout(file.try_clone().expect("the log's stdout"))
        .stderr(file);
    cmd.status().expect("systemd-run runs the supervisor");
    atl_host_load::stop(&unit);
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let record = Record::read_last(&text);
    (text, record)
}

/// A workload that exits 3 is measured complete: its exit, its leaf's memory, and no OOM counter moved.
#[test]
fn a_normal_exit_is_measured_complete() {
    let (log, record) = managed("exit", None, &["sh", "-c", "exit 3"]);
    let record = record.unwrap_or_else(|why| panic!("no complete record ({why}) in:\n{log}"));
    assert_eq!(record.exit, Exit::Code(3));
    assert!(
        record.unit.starts_with("atl-row-") && record.unit.ends_with(".scope"),
        "{}",
        record.unit
    );
    assert!(
        record.mem_peak > 0,
        "the workload's memory is in its leaf: {record:?}"
    );
    assert_eq!(record.events, Events::default());
    assert!(!record.certified());
    assert_eq!(record.limit, None);
}

/// A SIGKILL the workload sends itself is not an OOM: the leaf's counters stay at 0, so nothing certifies it.
#[test]
fn a_plain_sigkill_is_not_an_oom() {
    let (log, record) = managed("sigkill", None, &["sh", "-c", "kill -9 $$"]);
    let record = record.unwrap_or_else(|why| panic!("no complete record ({why}) in:\n{log}"));
    assert_eq!(record.exit, Exit::Signal(9));
    assert_eq!(
        record.events,
        Events::default(),
        "a plain SIGKILL moves no counter"
    );
    assert!(!record.certified());
}

/// The leaf's own 64 MiB limit kills a 200 MiB allocation: the leaf's `oom` and its `oom_kill` both move, so the kill certifies a local OOM.
#[test]
fn a_kill_at_the_leafs_own_limit_certifies_a_local_oom() {
    let workload = ["dd", "if=/dev/zero", "of=/dev/null", "bs=200M", "count=1"];
    let (log, record) = managed("oom", Some(64 * MIB), &workload);
    let record = record.unwrap_or_else(|why| panic!("no complete record ({why}) in:\n{log}"));
    assert_eq!(record.exit, Exit::Signal(9), "{record:?}");
    assert!(
        record.events.oom >= 1 && record.events.oom_kill >= 1,
        "{:?}",
        record.events
    );
    assert!(record.certified());
    assert_eq!(record.limit, Some(64 * MIB));
}

/// A process that moves itself out of the leaf is outside the measured tree: the record is incomplete, and says membership.
#[test]
fn a_process_that_leaves_the_leaf_makes_the_measure_incomplete() {
    let escape = "d=$(sed -n 's#^0::##p' /proc/self/cgroup); echo $$ > /sys/fs/cgroup$(dirname $d)/supervisor/cgroup.procs; sleep 60 &";
    let (log, record) = managed("escape", None, &["sh", "-c", escape]);
    let why = record.expect_err(&format!(
        "a process outside the leaf measured complete:\n{log}"
    ));
    assert!(why.starts_with("membership:"), "{why}");
}

/// A managed gate measures its own leaf and never relaunches: a relaunch would leave the leaf for `atl-gate-<pid>.scope`.
#[test]
fn a_managed_gate_measures_its_leaf_and_never_relaunches() {
    let gate = [HARNESS, "test", "host-load", "--case", "pass-under-load"];
    let (log, _) = managed("gate", None, &gate);
    let line = log
        .lines()
        .find(|l| l.starts_with("host load:"))
        .unwrap_or_else(|| panic!("no host load line in:\n{log}"));
    assert!(
        line.contains(" unit=atl-row-") && line.contains("/workload "),
        "the gate did not measure its leaf: {line}"
    );
    assert!(
        !log.contains("atl-gate-"),
        "a managed row relaunched into its own scope:\n{log}"
    );
}

/// A gate nested in a managed row shares the row's leaf, and its parent is alive there, waiting: the parent is not a process the nested gate left running.
#[test]
fn a_gate_nested_under_a_live_parent_in_the_leaf_leaves_nothing_behind() {
    let gate = format!("{HARNESS} test host-load --case pass-under-load");
    let (log, _) = managed("nested", None, &["sh", "-c", &gate]);
    assert!(
        !log.contains("left running"),
        "the nested gate counted its live parent as left running:\n{log}"
    );
    assert!(
        log.contains("host load: tier=scope unit=atl-row-"),
        "the nested gate did not measure the leaf:\n{log}"
    );
}

/// The supervisor killed while its workload runs: `stop` takes the scope and its leaf's process with it.
#[test]
fn stopping_a_scope_whose_supervisor_died_leaves_nothing_behind() {
    let unit = atl_host_load::new_unit();
    let _stop = StopOnDrop(unit.clone());
    let log = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("managed-killed.log");
    let mut cmd = atl_host_load::scoped_command(&unit);
    cmd.arg(HARNESS)
        .args(["scope-supervise", "--record"])
        .arg(&log)
        .args(["--", "sleep", "300"]);
    let mut supervisor = cmd.spawn().expect("systemd-run spawns");
    let scope = wait_for("the scope", || scope_dir(&unit).filter(|d| d.exists()));
    wait_for("the workload in its leaf", || {
        members(&scope.join("workload")).first().copied()
    });
    supervisor.kill().expect("the supervisor is killed");
    let _ = supervisor.wait();
    atl_host_load::stop(&unit);
    wait_for("the scope gone after stop", || {
        (!scope.exists()).then_some(())
    });
}

/// Stops a test's scope when the test ends, pass or fail, so a failed assertion leaves no `sleep` behind.
struct StopOnDrop(String);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        atl_host_load::stop(&self.0);
    }
}

/// The scope's cgroup dir, as systemd reports it (`ControlGroup`); None until systemd has registered the scope.
fn scope_dir(unit: &str) -> Option<PathBuf> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "-p", "ControlGroup", "--value"])
        .arg(format!("{unit}.scope"))
        .output()
        .expect("systemctl runs");
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!path.is_empty()).then(|| PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// The pids in a cgroup dir.
fn members(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.parse().ok())
        .collect()
}

/// Polls `probe` until it answers, for up to 10 s; the panic names `what` when it never does.
fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
