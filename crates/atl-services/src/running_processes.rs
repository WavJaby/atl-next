// SPDX-License-Identifier: MIT
//! IActivityManager.getRunningAppProcesses' policy (ActivityManagerService.getRunningAppProcesses -> ProcessList.getRunningAppProcessesLOSP / fillInProcMemInfoLOSP), over the activity service's process table.
//! Visibility as AOSP: a caller sees only its own uid's processes unless it holds REAL_GET_TASKS (ATM isGetTasksAllowed; no recents component and no privileged GET_TASKS holders here), and only its own
//! user's unless it holds INTERACT_ACROSS_USERS_FULL. Only attached processes are listed (`app.getThread() != null`, the caller's filter); none visible -> null, as AOSP's lazily made list.
//! Process state: each listed process answers `uid_state::process_state` of its holdings (Task, FGS, started or auto-create service, receiver, else CACHED_EMPTY) -> `importance` (AOSP procStateToImportance) and processState; lru stays 0 (AOSP sets curAdj for IMPORTANCE_CACHED; no OomAdjuster numbers, plan P1).
//! Left at Java's defaults (no ATL source to fill them truthfully): flags, lastTrimLevel, importanceReason*, isFocused, lastActivityTime, pkgDeps.
//! processinfo's pid answers (process_info.rs) read the same table: `state_of_pid`, `process_info_answer`.

use atl_services_types::permission;

use crate::uid_state::ProcessState;

/// RunningAppProcessInfo.IMPORTANCE_FOREGROUND (ActivityManager.java:3820) = procStateToImportance below FOREGROUND_SERVICE (:3962-3966).
pub const IMPORTANCE_FOREGROUND: i32 = 100;
/// IMPORTANCE_FOREGROUND_SERVICE (ActivityManager.java:3828): procStateToImportance for FOREGROUND_SERVICE (4) below IMPORTANT_FOREGROUND (6) (:3962-3963).
const IMPORTANCE_FOREGROUND_SERVICE: i32 = 125;
/// IMPORTANCE_SERVICE (ActivityManager.java:3897): procStateToImportance for SERVICE (10) and RECEIVER (11), below TOP_SLEEPING (12) (:3956-3957).
const IMPORTANCE_SERVICE: i32 = 300;
/// IMPORTANCE_CACHED (ActivityManager.java:3921): procStateToImportance from HOME (14) up (:3950-3951); CACHED_EMPTY (19) is in that band.
const IMPORTANCE_CACHED: i32 = 400;

/// AOSP procStateToImportance (ActivityManager.java:3947-3967) over the states uid_state derives. Its bands between FOREGROUND_SERVICE and HOME are not reached: no visible-but-not-top, perceptible or heavy-weight state exists here.
/// The pre-O rewrite (procStateToImportanceForTargetSdk :3991-3999: PERCEPTIBLE, TOP_SLEEPING, CANT_SAVE_STATE only) changes none of these five values, so the caller's targetSdk does not change the answer.
pub fn importance(state: ProcessState) -> i32 {
    match state {
        ProcessState::Top => IMPORTANCE_FOREGROUND,
        ProcessState::ForegroundService => IMPORTANCE_FOREGROUND_SERVICE,
        ProcessState::Service | ProcessState::Receiver => IMPORTANCE_SERVICE,
        ProcessState::CachedEmpty => IMPORTANCE_CACHED,
    }
}

/// ProcessStateEnum.NONEXISTENT (ProcessStateEnum.aidl:98): processinfo's state for a pid no attached process is (ActivityManagerService.java :6024-6026).
pub const PROCESS_STATE_NONEXISTENT: i32 = 20;
/// ProcessList.INVALID_ADJ (ProcessList.java:199): processinfo's score for such a pid (ActivityManagerService.java :6026-6028).
pub const INVALID_ADJ: i32 = -10_000;
/// ProcessList.FOREGROUND_APP_ADJ (ProcessList.java:277): the score of every found process. No OomAdjuster numbers yet (plan P1), so a Service-state process also answers 0 where AOSP answers its own adj.
pub const FOREGROUND_APP_ADJ: i32 = 0;

/// processinfo's (state, score) for one pid: the process's ProcessStateEnum value and FOREGROUND_APP_ADJ, or NONEXISTENT and INVALID_ADJ when `found` is None.
/// Counted: a pid with no process, and the constant score given to a non-top state (plan P1: no adj is derived yet).
pub fn process_info_answer(found: Option<ProcessState>) -> (i32, i32) {
    match found {
        Some(state) => {
            if state != ProcessState::Top {
                atl_diag::answered(
                    "IProcessInfoService",
                    "score not modelled: FOREGROUND_APP_ADJ for a non-top state",
                );
            }
            (state as i32, FOREGROUND_APP_ADJ)
        }
        None => {
            atl_diag::answered(
                "IProcessInfoService",
                "pid with no attached process: NONEXISTENT",
            );
            (PROCESS_STATE_NONEXISTENT, INVALID_ADJ)
        }
    }
}

/// processinfo's lookup for `pid` as the caller (binderd tag `calling_tag`) numbers it: the state of the process that `pid_seen_by` maps to it. `procs` holds (tag, program pid, state) of the attached processes.
/// None when no process is: pid 0 names none (pid_seen_by answers 0 for an invisible process).
pub fn state_of_pid(
    mut procs: impl Iterator<Item = (i32, Option<i32>, ProcessState)>,
    calling_tag: i32,
    pid: i32,
) -> Option<ProcessState> {
    if pid == 0 {
        return None;
    }
    procs
        .find(|&(tag, program_pid, _)| pid_seen_by(tag, program_pid, calling_tag) == pid)
        .map(|(_, _, state)| state)
}

/// What the answer reads of one attached process record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningProcess<'a> {
    pub process_name: &'a str,
    pub package: &'a str,
    pub uid: u32,
    /// the pid as the caller numbers it
    pub pid: i32,
    /// the process's state (uid_state::process_state of its holdings): importance and processState follow it
    pub state: ProcessState,
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
            state: ProcessState::Top,
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

    /// Expected: ActivityManager.RunningAppProcessInfo.IMPORTANCE_* (ActivityManager.java :3820, :3828, :3897, :3921) for each state's ProcessStateEnum value (ProcessStateEnum.aidl :36, :42, :62, :68, :95),
    /// by procStateToImportance (:3947-3967). RECEIVER 11 is below TOP_SLEEPING 12 and at or above SERVICE 10: the SERVICE band.
    #[test]
    fn importance_is_procstatetoimportance_per_state() {
        assert_eq!(importance(ProcessState::Top), 100);
        assert_eq!(importance(ProcessState::ForegroundService), 125);
        assert_eq!(importance(ProcessState::Service), 300);
        assert_eq!(importance(ProcessState::Receiver), 300);
        assert_eq!(importance(ProcessState::CachedEmpty), 400);
    }

    #[test]
    fn state_of_pid_is_the_callers_own_process() {
        // own sandbox: program pid 2 is the caller's process; the other sandbox's program pid 2 is not in the caller's namespace
        let procs = [
            (1_113_415, Some(2), ProcessState::Service),
            (1_200_000, Some(2), ProcessState::Top),
        ];
        assert_eq!(
            state_of_pid(procs.into_iter(), 1_113_415, 2),
            Some(ProcessState::Service)
        );
        assert_eq!(state_of_pid(procs.into_iter(), 1_113_415, 3), None);
        // a caller with no process in the table: none of the pids is its own
        assert_eq!(state_of_pid(procs.into_iter(), 1_300_000, 2), None);
    }

    #[test]
    fn state_of_pid_zero_names_no_process() {
        // a record whose program pid did not read is invisible (pid_seen_by 0): pid 0 must not find it
        let procs = [(1_113_415, None, ProcessState::Top)];
        assert_eq!(state_of_pid(procs.into_iter(), 1_113_415, 0), None);
    }

    #[test]
    fn processinfo_nonexistent_for_no_process() {
        // ProcessStateEnum.NONEXISTENT (ProcessStateEnum.aidl:98) and ProcessList.INVALID_ADJ (ProcessList.java:199)
        assert_eq!(process_info_answer(None), (20, -10_000));
        // a found process answers its ProcessStateEnum value (RECEIVER = 11, ProcessStateEnum.aidl:68); the score stays FOREGROUND_APP_ADJ (ProcessList.java:277) until OomAdjuster numbers exist (plan P1)
        assert_eq!(process_info_answer(Some(ProcessState::Receiver)), (11, 0));
    }
}
