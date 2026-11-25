// SPDX-License-Identifier: MIT
//! The processes atld started for the services, by host pid (binderd tags them with it). A kill and a program-pid request are for these only.
//! `killed`: the services' kill went out, the process is dying and not left running at the end of the run. `reaped`: its wait thread ended, the pid is gone.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// The started processes not yet reaped, by host pid: whether the services' kill went out.
#[derive(Default)]
pub struct Processes {
    live: Mutex<BTreeMap<i32, bool>>,
}

impl Processes {
    /// A process `start` made for the services: controlled until its reap.
    pub fn started(&self, pid: i32) {
        self.lock().insert(pid, false);
    }

    /// The services may still query or kill a pid they started: started, and its kill not gone out yet.
    pub fn may_kill(&self, pid: i32) -> bool {
        self.lock().get(&pid) == Some(&false)
    }

    /// The services' kill went out: the process is dying.
    pub fn killed(&self, pid: i32) {
        if let Some(killed) = self.lock().get_mut(&pid) {
            *killed = true;
        }
    }

    /// The wait thread ended (`Ok` or `Err`): the process is gone.
    pub fn reaped(&self, pid: i32) {
        self.lock().remove(&pid);
    }

    /// The started processes not killed: the run's teardown list.
    pub fn running(&self) -> Vec<i32> {
        self.lock()
            .iter()
            .filter(|(_, killed)| !**killed)
            .map(|(pid, _)| *pid)
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<i32, bool>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::Processes;

    #[test]
    fn a_started_process_may_be_killed_once() {
        let processes = Processes::default();
        processes.started(41);
        assert!(processes.may_kill(41), "a started process may be killed");
        assert!(
            !processes.may_kill(42),
            "a pid the services did not start may not be killed"
        );
        processes.killed(41);
        assert!(
            !processes.may_kill(41),
            "a killed process may not be killed again"
        );
    }

    #[test]
    fn running_is_the_started_pids_not_killed_or_reaped() {
        let processes = Processes::default();
        processes.started(41);
        processes.started(42);
        processes.killed(41);
        assert_eq!(
            processes.running(),
            [42],
            "41 is killed: dying, not left running"
        );
        processes.reaped(42);
        assert!(processes.running().is_empty(), "42 is reaped: gone");
    }
}
