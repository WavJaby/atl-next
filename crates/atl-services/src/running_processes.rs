// SPDX-License-Identifier: MIT
//! IActivityManager.getRunningAppProcesses' policy (ActivityManagerService.getRunningAppProcesses -> ProcessList.getRunningAppProcessesLOSP / fillInProcMemInfoLOSP), over the activity service's process table.
//! Visibility as AOSP: a caller sees only its own uid's processes unless it holds REAL_GET_TASKS (ATM isGetTasksAllowed; no recents component and no privileged GET_TASKS holders here), and only its own
//! user's unless it holds INTERACT_ACROSS_USERS_FULL. Only attached processes are listed (`app.getThread() != null`, the caller's filter); none visible -> null, as AOSP's lazily made list.
//! Process state: ATL keeps none per process; every app process is a desktop window the user started, so each answers PROCESS_STATE_TOP (as `processinfo` does) -> IMPORTANCE_FOREGROUND, lru 0.
//! Left at Java's defaults (no ATL source to fill them truthfully): flags, lastTrimLevel, importanceReason*, isFocused, lastActivityTime, pkgDeps.

use atl_services_types::permission;

/// ActivityManager.PROCESS_STATE_TOP.
pub const PROCESS_STATE_TOP: i32 = 2;
/// RunningAppProcessInfo.IMPORTANCE_FOREGROUND = procStateToImportance(PROCESS_STATE_TOP) (every state below FOREGROUND_SERVICE).
pub const IMPORTANCE_FOREGROUND: i32 = 100;

/// What the answer reads of one attached process record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningProcess<'a> {
    pub process_name: &'a str,
    pub package: &'a str,
    pub uid: u32,
    /// the pid as the caller numbers it
    pub pid: i32,
}

/// Who asks: its uid and the two permissions that widen the view.
pub struct Caller {
    pub uid: u32,
    /// INTERACT_ACROSS_USERS_FULL
    pub all_users: bool,
    /// isGetTasksAllowed: REAL_GET_TASKS
    pub all_uids: bool,
}

/// A record's pid as the caller numbers it (the kernel's task_tgid_nr_ns): every process is its own sandbox's program in its own pid namespace, so the caller's own process (same binderd tag) is its
/// `program_pid`; another sandbox's is invisible there, 0 (also RunningAppProcessInfo's default, for an own pid the lookup did not find).
pub fn pid_seen_by(tag: i32, program_pid: Option<i32>, calling_tag: i32) -> i32 {
    if tag == calling_tag {
        program_pid.unwrap_or(0)
    } else {
        0
    }
}

/// getRunningAppProcessesLOSP's selection over `procs` (oldest start first), most recently started first (mLruProcesses is walked from its end); None when nothing is visible.
pub fn visible<'a>(
    procs: impl DoubleEndedIterator<Item = RunningProcess<'a>>,
    caller: &Caller,
) -> Option<Vec<RunningProcess<'a>>> {
    let list: Vec<_> = procs
        .rev()
        .filter(|p| {
            caller.all_users || permission::user_id(p.uid) == permission::user_id(caller.uid)
        })
        .filter(|p| caller.all_uids || p.uid == caller.uid)
        .collect();
    (!list.is_empty()).then_some(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: u32 = 10_123;
    const OTHER: u32 = 10_124;
    /// APP's app id in user 10 (UserHandle.getUid(10, 10123)).
    const APP_USER10: u32 = 10 * permission::PER_USER_RANGE + 10_123;

    fn procs() -> Vec<RunningProcess<'static>> {
        let p = |process_name, package, uid, pid| RunningProcess {
            process_name,
            package,
            uid,
            pid,
        };
        vec![
            p("com.a", "com.a", APP, 2),
            p("com.b", "com.b", OTHER, 7),
            p("com.a:remote", "com.a", APP, 9),
            p("com.a", "com.a", APP_USER10, 11),
        ]
    }

    fn pids(all_users: bool, all_uids: bool) -> Vec<i32> {
        let caller = Caller {
            uid: APP,
            all_users,
            all_uids,
        };
        visible(procs().into_iter(), &caller)
            .unwrap_or_default()
            .iter()
            .map(|p| p.pid)
            .collect()
    }

    #[test]
    fn own_uid_only_newest_first() {
        assert_eq!(pids(false, false), [9, 2]);
    }

    #[test]
    fn permissions_widen_the_view() {
        assert_eq!(pids(false, true), [9, 7, 2]);
        // INTERACT_ACROSS_USERS_FULL alone: still the caller's uid only (user 10's process has another uid)
        assert_eq!(pids(true, false), [9, 2]);
        assert_eq!(pids(true, true), [11, 9, 7, 2]);
    }

    #[test]
    fn pid_in_the_callers_namespace() {
        // own sandbox: the program's pid there (bwrap's program is pid 2 under its init); another sandbox: not in the caller's namespace
        assert_eq!(pid_seen_by(1_113_415, Some(2), 1_113_415), 2);
        assert_eq!(pid_seen_by(1_113_415, Some(2), 1_200_000), 0);
        assert_eq!(pid_seen_by(1_113_415, None, 1_113_415), 0);
    }

    #[test]
    fn none_visible_is_null() {
        let caller = Caller {
            uid: 10_999,
            all_users: false,
            all_uids: false,
        };
        assert!(visible(procs().into_iter(), &caller).is_none());
    }
}
