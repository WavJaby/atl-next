// SPDX-License-Identifier: MIT
//! Stress for the race `write_exe` avoids: a fork of another thread holds a write fd of a just-written file, and exec of the file fails with ETXTBSY until that child execs.
//! Run by hand: `cargo test -p atl-exec-fixture -- --ignored`.
#![expect(
    clippy::disallowed_types,
    reason = "test: forks processes to keep the race open"
)]

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

const RUNS: usize = 2000;
const FORKERS: usize = 8;

/// Stops the forkers when the scope ends, also on a panic (a panic would otherwise wait for them forever).
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
#[ignore = "stress: a few seconds; run by hand"]
fn write_exe_survives_forking_threads() {
    let dir = std::env::temp_dir().join(format!("atl-exec-fixture-stress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let stop = AtomicBool::new(false);
    let failed = std::thread::scope(|s| {
        for _ in 0..FORKERS {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("/bin/true").status();
                }
            });
        }
        let _stop = StopOnDrop(&stop);
        let mut failed = Vec::new();
        for i in 0..RUNS {
            let exe = dir.join(format!("exe-{i}"));
            atl_exec_fixture::write_exe(&exe, "#!/bin/sh\nexit 0\n");
            if let Err(e) = Command::new(&exe).status() {
                failed.push(format!("run {i}: {e}"));
            }
        }
        failed
    });
    std::fs::remove_dir_all(&dir).expect("cleanup");
    assert!(
        failed.is_empty(),
        "{} of {RUNS} runs failed to exec, first: {:?}",
        failed.len(),
        failed.first()
    );
}
