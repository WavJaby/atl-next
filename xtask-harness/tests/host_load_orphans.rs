// SPDX-License-Identifier: MIT
//! A killed host-load gate leaves no burner: each burner runs under `setpriv --pdeathsig KILL` (host_load.rs `Burners::start`), so the kernel ends it with its gate.
#![expect(
    clippy::disallowed_types,
    reason = "a host test: it runs the harness's own gate, which starts burners in scopes"
)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The gate's stdout line per burner: `host-load: burner pid <pid>` (host_load.rs `BURNER_PREFIX`).
const BURNER: &str = "host-load: burner pid ";
/// The gate's line once every burner is up (host_load.rs `Burners::start`).
const UP: &str = "burner(s) at CPUWeight=";
/// The gate prints its burners within this: a scope per CPU, each burner's start line.
const START: Duration = Duration::from_secs(30);
/// A burner is gone this long after its gate is killed: PDEATHSIG is delivered at once.
const DIE: Duration = Duration::from_secs(5);

#[test]
fn a_killed_gate_leaves_no_burner() {
    let mut gate = Command::new(env!("CARGO_BIN_EXE_xtask-harness"))
        .args(["test", "host-load", "--case", "starved"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the harness starts");
    let stdout = gate.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut burners = Vec::new();
    let deadline = Instant::now() + START;
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the gate prints its burners in time");
        if let Some(pid) = line.strip_prefix(BURNER) {
            burners.push(pid.trim().parse::<u32>().expect("a burner pid"));
        }
        if line.contains(UP) {
            break;
        }
    }
    assert!(!burners.is_empty(), "no burner was started");
    gate.kill().expect("kill the gate");
    gate.wait().expect("reap the gate");
    let _ =
        std::fs::remove_dir_all(std::env::temp_dir().join(format!("atl-host-load-{}", gate.id())));
    let until = Instant::now() + DIE;
    for pid in burners {
        while alive(pid) {
            assert!(
                Instant::now() < until,
                "burner {pid} outlived its killed gate"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Whether `pid` still runs: a zombie (exited, not yet reaped by its new parent) does not count.
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // `pid (comm) state ...`: comm may hold spaces and parentheses, so the state follows the last `)`
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}
