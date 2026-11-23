// SPDX-License-Identifier: MIT
//! The `processinfo` service (ActivityManagerService's ProcessInfoService): media's resource manager asks it for a client process's state and OOM score (frameworks/av media/utils ProcessInfo.cpp `getPriority` / `isPidTrusted`, both `waitForService("processinfo")` -> blocks forever when absent).
//! Reference: ActivityManagerService.getProcessStatesAndOomScoresForPIDs (AMS :5979-6039). Each pid is the caller's own process as running_processes::state_of_pid numbers it; its state is the process's uid_state::process_state.
//! A pid that is no attached process of the caller's sandbox answers NONEXISTENT with INVALID_ADJ (AMS :6024-6028; libmedia then consults its override map). Scores: no OomAdjuster numbers yet (plan P1), so every found process answers FOREGROUND_APP_ADJ.
//! Not modelled: a pid starting an activity answers its holdings' state here; AMS answers TOP for it while the uid is pending top (mPendingStartActivityUids, AMS :6005-6023).

use atl_aidl::aidl::android::os::IProcessInfoService::{BnProcessInfoService, IProcessInfoService};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::activity::ActivityManager;
use crate::running_processes::process_info_answer;

struct ProcessInfoService {
    activity: ActivityManager,
}

impl Interface for ProcessInfoService {}

/// AMS's argument checks: NullPointerException for a null array, IllegalArgumentException when an out array's length differs from `pids`'. Then each slot takes the next of `values`, in pid order.
fn fill(
    pids: Option<&[i32]>,
    out: &mut Option<Vec<i32>>,
    values: impl Iterator<Item = i32>,
) -> binder::Result<()> {
    let (Some(pids), Some(out)) = (pids, out.as_mut()) else {
        return Err(Status::new_exception(ExceptionCode::NULL_POINTER, None));
    };
    if out.len() != pids.len() {
        return Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None));
    }
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = value;
    }
    Ok(())
}

impl ProcessInfoService {
    /// (state, score) per pid, each answered once (process_info_answer counts). Empty when `pids` is null: fill refuses that.
    fn answers(&self, pids: Option<&[i32]>) -> Vec<(i32, i32)> {
        pids.map_or_else(Vec::new, |pids| {
            self.activity
                .process_states(ThreadState::get_calling_pid(), pids)
                .into_iter()
                .map(process_info_answer)
                .collect()
        })
    }
}

impl IProcessInfoService for ProcessInfoService {
    /// ProcessInfoService.getProcessStatesFromPids -> AMS getProcessStatesAndOomScoresForPIDs(pids, states, null).
    fn r#getProcessStatesFromPids(
        &self,
        pids: Option<&[i32]>,
        states: &mut Option<Vec<i32>>,
    ) -> binder::Result<()> {
        let answers = self.answers(pids);
        fill(pids, states, answers.iter().map(|a| a.0))
    }

    /// ProcessInfoService.getProcessStatesAndOomScoresFromPids. AMS first enforces GET_PROCESS_STATE_AND_OOM_SCORE: held by the media server on Android; here the caller is media code inside the app process itself.
    fn r#getProcessStatesAndOomScoresFromPids(
        &self,
        pids: Option<&[i32]>,
        states: &mut Option<Vec<i32>>,
        scores: &mut Option<Vec<i32>>,
    ) -> binder::Result<()> {
        let answers = self.answers(pids);
        fill(pids, states, answers.iter().map(|a| a.0))?;
        fill(pids, scores, answers.iter().map(|a| a.1))
    }
}

/// The service's binder, for `addService`; `activity` is the process table it answers from.
pub fn new_binder(activity: ActivityManager) -> SpIBinder {
    BnProcessInfoService::new_binder(ProcessInfoService { activity }, BinderFeatures::default())
        .as_binder()
}
