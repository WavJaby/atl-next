// SPDX-License-Identifier: MIT
//! The `processinfo` service (ActivityManagerService's ProcessInfoService): media's resource manager asks it for a client process's state and OOM score (frameworks/av media/utils ProcessInfo.cpp `getPriority` / `isPidTrusted`, both `waitForService("processinfo")` -> blocks forever when absent).
//! Reference: ActivityManagerService.getProcessStatesAndOomScoresForPIDs. ATL keeps no per-process state or OOM adjustment yet: every app process is a desktop window the user started, so each pid answers as AMS answers a pending-top app (PROCESS_STATE_TOP, FOREGROUND_APP_ADJ). No pid answers NONEXISTENT: the service does not track pids (the caller's pid is in its own sandbox namespace). Revisit when the activity service tracks process states.

use atl_aidl::aidl::android::os::IProcessInfoService::{BnProcessInfoService, IProcessInfoService};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status};

/// The interface name counters use.
const INTERFACE: &str = "IProcessInfoService";
/// ActivityManager.PROCESS_STATE_TOP = ProcessStateEnum.TOP (2).
const PROCESS_STATE_TOP: i32 = 2;
/// ProcessList.FOREGROUND_APP_ADJ (0).
const FOREGROUND_APP_ADJ: i32 = 0;

struct ProcessInfoService;

impl Interface for ProcessInfoService {}

/// AMS's argument checks: NullPointerException for a null array, IllegalArgumentException when an out array's length differs from `pids`'.
fn fill(pids: Option<&[i32]>, out: &mut Option<Vec<i32>>, value: i32) -> binder::Result<()> {
    let (Some(pids), Some(out)) = (pids, out.as_mut()) else {
        return Err(Status::new_exception(ExceptionCode::NULL_POINTER, None));
    };
    if out.len() != pids.len() {
        return Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None));
    }
    out.fill(value);
    Ok(())
}

impl IProcessInfoService for ProcessInfoService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        atl_services_wire::unimplemented(INTERFACE, method)
    }

    /// ProcessInfoService.getProcessStatesFromPids -> AMS getProcessStatesAndOomScoresForPIDs(pids, states, null).
    fn r#getProcessStatesFromPids(
        &self,
        pids: Option<&[i32]>,
        states: &mut Option<Vec<i32>>,
    ) -> binder::Result<()> {
        fill(pids, states, PROCESS_STATE_TOP)
    }

    /// ProcessInfoService.getProcessStatesAndOomScoresFromPids. AMS first enforces GET_PROCESS_STATE_AND_OOM_SCORE: held by the media server on Android; here the caller is media code inside the app process itself.
    fn r#getProcessStatesAndOomScoresFromPids(
        &self,
        pids: Option<&[i32]>,
        states: &mut Option<Vec<i32>>,
        scores: &mut Option<Vec<i32>>,
    ) -> binder::Result<()> {
        fill(pids, states, PROCESS_STATE_TOP)?;
        fill(pids, scores, FOREGROUND_APP_ADJ)
    }
}

/// The service's binder, for `addService`.
pub fn new_binder() -> SpIBinder {
    BnProcessInfoService::new_binder(ProcessInfoService, BinderFeatures::default()).as_binder()
}
