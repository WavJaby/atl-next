// SPDX-License-Identifier: MIT
//! A gate that leaves a process running ends in the leftover verdict (main.rs, atl-host-load `Gate::leftovers`); one that leaves none does not. Needs a user systemd that makes scopes.
#![expect(
    clippy::disallowed_types,
    reason = "a host test: it runs the harness's own gate"
)]

use std::process::{Command, Output};

/// main.rs's verdict text for a leftover process.
const LEFT: &str = "left running: ";

/// `pass-under-load`: the case whose own verdict is a pass, so the break's red can only be the leftover check's. One CPU, as the gate runs it: one burner.
fn gate(extra: &[&str]) -> Output {
    let cpu = atl_host_load::allowed_cpus().expect("this process's CPU set")[0];
    Command::new("taskset")
        .args(["-c", &cpu.to_string()])
        .arg(env!("CARGO_BIN_EXE_xtask-harness"))
        .args(["test", "host-load", "--case", "pass-under-load"])
        .args(extra)
        .output()
        .expect("the harness runs")
}

#[test]
fn a_leaked_process_is_named_in_a_red_verdict() {
    let out = gate(&["--break", "leak"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.starts_with("host-load: FAILED") && l.contains(LEFT))
        .unwrap_or_else(|| panic!("no leftover verdict in:\n{text}"));
    assert!(line.contains(" sleep 60"), "{line}");
    assert_eq!(out.status.code(), Some(1));
    let pid = line
        .split(LEFT)
        .nth(1)
        .and_then(|l| l.split(' ').next())
        .expect("a pid");
    let killed = Command::new("kill").arg(pid).status().expect("kill");
    assert!(killed.success(), "the leaked process {pid} was running");
}

/// The exit code is not judged: a loaded host may starve the case (undecided).
#[test]
fn a_gate_that_leaves_nothing_has_no_leftover_verdict() {
    let text = String::from_utf8_lossy(&gate(&[]).stdout).into_owned();
    assert!(text.contains("host load: tier=scope"), "{text}");
    assert!(!text.contains(LEFT), "{text}");
}
