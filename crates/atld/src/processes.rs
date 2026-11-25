// SPDX-License-Identifier: MIT
//! The processes atld started for the services, by host pid (binderd tags them with it). A kill and a program-pid request are for these only.
//! `killed`: the services' kill went out, the process is dying and not left running at the end of the run. `reaped`: its wait thread ended, the pid is gone.
//! `wait_gone`: an update of a package waits until every one of its processes is reaped. A kill only signals, so a killed process still counts as alive.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A started process not yet reaped: its package, and whether the services' kill went out.
struct Live {
    package: String,
    killed: bool,
}

/// The started processes not yet reaped, by host pid.
#[derive(Default)]
pub struct Processes {
    live: Mutex<BTreeMap<i32, Live>>,
    /// notified at each reap, for `wait_gone`
    ended: Condvar,
}

impl Processes {
    /// A process `start` made for the services of `package`: controlled until its reap.
    pub fn started(&self, pid: i32, package: &str) {
        self.lock().insert(
            pid,
            Live {
                package: package.to_owned(),
                killed: false,
            },
        );
    }

    /// The services may still query or kill a pid they started: started, and its kill not gone out yet.
    pub fn may_kill(&self, pid: i32) -> bool {
        self.lock().get(&pid).is_some_and(|l| !l.killed)
    }

    /// The services' kill went out: the process is dying.
    pub fn killed(&self, pid: i32) {
        if let Some(l) = self.lock().get_mut(&pid) {
            l.killed = true;
        }
    }

    /// The wait thread ended (`Ok` or `Err`): the process is gone.
    pub fn reaped(&self, pid: i32) {
        self.lock().remove(&pid);
        self.ended.notify_all();
    }

    /// The started processes not killed: the run's teardown list.
    pub fn running(&self) -> Vec<i32> {
        self.lock()
            .iter()
            .filter(|(_, l)| !l.killed)
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Returns once every process of `package` is reaped. Killed counts as alive. After `limit`: error naming the pids still there.
    pub fn wait_gone(&self, package: &str, limit: Duration) -> Result<()> {
        let deadline = Instant::now() + limit;
        let mut live = self.lock();
        loop {
            let pids: Vec<i32> = live
                .iter()
                .filter(|(_, l)| l.package == package)
                .map(|(pid, _)| *pid)
                .collect();
            if pids.is_empty() {
                return Ok(());
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                bail!("{package}: process {pids:?} not reaped within {limit:?}");
            };
            live = self
                .ended
                .wait_timeout(live, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<i32, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::Processes;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn a_started_process_may_be_killed_once() {
        let processes = Processes::default();
        processes.started(41, "com.a");
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
        processes.started(41, "com.a");
        processes.started(42, "com.a");
        processes.killed(41);
        assert_eq!(
            processes.running(),
            [42],
            "41 is killed: dying, not left running"
        );
        processes.reaped(42);
        assert!(processes.running().is_empty(), "42 is reaped: gone");
    }

    /// An update waits for the reap, not for the kill. The reaper sets its flag before it reaps, so a return before the reap shows as the flag not set.
    #[test]
    fn wait_gone_returns_after_the_reap_not_after_the_kill() {
        let processes = Arc::new(Processes::default());
        processes.started(41, "com.a");
        processes.killed(41);
        let reaped = Arc::new(AtomicBool::new(false));
        let reaper = {
            let (processes, reaped) = (Arc::clone(&processes), Arc::clone(&reaped));
            std::thread::spawn(move || {
                // killed, not gone: the waiter has to stay until here
                std::thread::sleep(Duration::from_millis(100));
                reaped.store(true, Ordering::SeqCst);
                processes.reaped(41);
            })
        };
        processes
            .wait_gone("com.a", Duration::from_secs(10))
            .expect("the reap comes within the limit");
        assert!(
            reaped.load(Ordering::SeqCst),
            "wait_gone returned before the reap: a killed process counted as gone"
        );
        reaper.join().expect("the reaper");
    }

    #[test]
    fn wait_gone_times_out_naming_the_pid() {
        let processes = Processes::default();
        processes.started(42, "com.a");
        let err = processes
            .wait_gone("com.a", Duration::from_millis(20))
            .expect_err("pid 42 is not reaped");
        assert!(
            err.to_string().contains("42"),
            "the error names the pid: {err}"
        );
    }

    #[test]
    fn wait_gone_ignores_other_packages() {
        let processes = Processes::default();
        processes.started(41, "com.b");
        processes
            .wait_gone("com.a", Duration::from_millis(20))
            .expect("no process of com.a is alive");
    }
}
