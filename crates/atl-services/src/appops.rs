// SPDX-License-Identifier: MIT
//! The `appops` service (Context.APP_OPS_SERVICE): IAppOpsService-lite. Without it SystemServiceRegistry gives apps a null AppOpsManager (Battle Cats TW asks 8 times at startup).
//! Behaviour = AppOpsService for a device where nobody changed a mode (app_ops.rs): checkOperation and its Raw / ForDevice forms (checkOperationImpl:2992 -> checkOperationUnchecked), noteOperation (noteOperationImpl:3356 ->
//! noteOperationUnchecked: the mode, nothing recorded), checkPackage (:3186), shouldCollectNotes and the not-sampled stack-trace config (AppOpsManager's first noted op needs both). Package validity = isIncomingPackageValid (installed + visible to the caller, Packages::visible); the
//! package/uid match = verifyAndGetBypass. Not modelled: op history, watchers and callbacks, started ops, proxies, restrictions, async-noted ops; those methods answer UNKNOWN_TRANSACTION (counted).
//! MODE_FOREGROUND (ops 32 TAKE_AUDIO_FOCUS, 154 CONTROL_AUDIO, 155 CONTROL_AUDIO_PARTIAL by default) is evaluated against the uid's state (uid_state::UidView, the activity manager's) as
//! AppOpsUidStateTrackerImpl.evalMode (:132-161) does, in checkOperation and noteOperation alike; see [`eval_mode`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use atl_aidl::aidl::com::android::internal::app::IAppOpsService::{
    BnAppOpsService, EnforcePermission, IAppOpsService,
};
use atl_parcelables::android::app::SyncNotedAppOp;
use atl_parcelables::com::android::internal::app::MessageSamplingConfig;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::{Packages, Permissions};
use crate::uid_state::{ProcessState, UidView, UidViews};
use atl_services_types::{app_ops, permission};

/// The interface name counters use.
const INTERFACE: &str = "IAppOpsService";
/// Manifest.permission.UPDATE_APP_OPS_STATS: verifyIncomingUid's permission for noting another uid's op.
const UPDATE_APP_OPS_STATS: &str = "android.permission.UPDATE_APP_OPS_STATS";
/// Context.DEVICE_ID_DEFAULT: the only device (no virtual devices exist).
const DEVICE_ID_DEFAULT: i32 = 0;
/// AppOpsManager.OP_NONE: no op sampled.
const OP_NONE: i32 = -1;
/// reportRuntimeAppOpAccessMessageAndGetConfig's config lifetime: Instant.now().plus(1, ChronoUnit.HOURS).
const SAMPLING_CONFIG_LIFETIME_MS: i64 = 3_600_000;
/// AppOpsManager UID_STATE_* (AppOpsManager.java:523-600) that processStateToUidState gives the derived process states.
const UID_STATE_TOP: i32 = 200;
const UID_STATE_FOREGROUND_SERVICE: i32 = 400;
/// UID_STATE_FOREGROUND = UID_STATE_MAX_LAST_NON_RESTRICTED (:559, :566), resolveFirstUnrestrictedUidState for every op (:609-611): a uid state above it evaluates MODE_FOREGROUND to IGNORED.
const UID_STATE_MAX_LAST_NON_RESTRICTED: i32 = 500;
const UID_STATE_BACKGROUND: i32 = 600;
const UID_STATE_CACHED: i32 = 700;
const UID_STATE_NONEXISTENT: i32 = i32::MAX;
/// AppOpsService.Constants TOP_STATE_SETTLE_TIME and FG_SERVICE_STATE_SETTLE_TIME defaults (AppOpsService.java:544-547, both 5 s): a move from TOP or FOREGROUND_SERVICE past the restriction
/// threshold commits this long after it happens (AppOpsUidStateTrackerImpl.updateUidProcState :262-277). BG_STATE_SETTLE_TIME (1 s) only delays BACKGROUND -> CACHED, which evaluates alike.
const FOREGROUND_SETTLE_TIME_MS: u64 = 5_000;

struct AppOps {
    permissions: Arc<Permissions>,
    packages: Arc<Packages>,
    /// the op uid's state (the activity manager's)
    uids: UidViews,
}

/// AppOpsUidStateTracker.processStateToUidState (:45-84) over the derived states; no process = NONEXISTENT (finishRunningOpsForKilledPackages; CACHED otherwise, which evaluates alike).
fn uid_state_of(state: Option<ProcessState>) -> i32 {
    match state {
        Some(ProcessState::Top) => UID_STATE_TOP,
        Some(ProcessState::ForegroundService) => UID_STATE_FOREGROUND_SERVICE,
        Some(ProcessState::Service | ProcessState::Receiver) => UID_STATE_BACKGROUND,
        Some(ProcessState::CachedEmpty) => UID_STATE_CACHED,
        None => UID_STATE_NONEXISTENT,
    }
}

/// AppOpsUidStateTrackerImpl.evalMode (:132-161): a mode other than MODE_FOREGROUND stands; MODE_FOREGROUND -> ALLOWED when the committed uid state is at or below FOREGROUND, else IGNORED.
/// Committed (updateUidProcState :219-280): a better state at once, NONEXISTENT at once, a worse one past the threshold FOREGROUND_SETTLE_TIME_MS after the uid's last Task and FGS ended.
/// CONTROL_AUDIO is a capability op (getOpCapability :163-183, FOREGROUND_AUDIO_CONTROL): TOP holds PROCESS_CAPABILITY_ALL; an FGS adds it when while-in-use allowed and ro_foreground_audio_control
/// (ENABLED in the pin's release configs; OomAdjuster :2435-2446). Every FGS is taken as while-in-use here (counted), so the capability rule answers as the state rule.
/// Not modelled: app widget visibility, isPendingTopUid, isTempAllowlistedForFgsWhileInUse (all would answer ALLOWED; none exist here).
pub fn eval_mode(code: i32, mode: i32, view: &UidView) -> i32 {
    if mode != app_ops::FOREGROUND {
        return mode;
    }
    let state = uid_state_of(view.state);
    let settling = view.state.is_some()
        && view
            .restricted_since
            .is_some_and(|t| view.now < t.saturating_add(FOREGROUND_SETTLE_TIME_MS));
    if code == OP_CONTROL_AUDIO && view.state == Some(ProcessState::ForegroundService) {
        atl_diag::answered(
            INTERFACE,
            "CONTROL_AUDIO for an FGS uid: taken as while-in-use",
        );
    }
    if state <= UID_STATE_MAX_LAST_NON_RESTRICTED || settling {
        app_ops::ALLOWED
    } else {
        app_ops::IGNORED
    }
}

/// AppOpsManager.OP_CONTROL_AUDIO: the one MODE_FOREGROUND op with a capability (FOREGROUND_AUDIO_CONTROL).
const OP_CONTROL_AUDIO: i32 = 154;

/// How a (uid, package) pair passed AppOpsService's checks.
enum Verified {
    /// isIncomingPackageValid false: not installed, or filtered from the caller
    Invalid,
    /// verifyAndGetBypass threw: the package is not `uid`'s
    WrongUid,
    Ok,
}

impl AppOps {
    fn verify(&self, uid: i32, package: &str) -> Verified {
        let caller = ThreadState::get_calling_uid();
        if self.packages.visible(caller, package).is_none() {
            return Verified::Invalid;
        }
        match u32::try_from(uid) {
            Ok(uid) if self.packages.is_same_app(package, uid) => Verified::Ok,
            _ => Verified::WrongUid,
        }
    }

    /// The op's mode for `uid` (app_ops::mode with this uid's runtime grants); an unknown op throws as verifyIncomingOp does.
    fn mode(&self, code: i32, uid: i32) -> binder::Result<i32> {
        let uid = u32::try_from(uid).unwrap_or(u32::MAX);
        app_ops::mode(code, |p| self.permissions.runtime_grant(uid, p)).ok_or_else(|| bad_op(code))
    }

    /// checkOperationImpl for every check variant: an unknown op throws (verifyIncomingOp); a virtual device -> MODE_IGNORED (none exist); a null package -> MODE_IGNORED (resolvePackageName
    /// gives an app uid no name); an invalid or foreign package -> the op's default mode (checkOperationImpl / checkOperationUnchecked's catch); else the uid's mode. Not raw: MODE_FOREGROUND is
    /// evaluated against the uid's state ([`eval_mode`]; checkOperationUnchecked :3060-3090 evaluates the verified mode only, the default of an unverified package is returned as is).
    fn check(
        &self,
        method: &str,
        code: i32,
        uid: i32,
        package: Option<&str>,
        device: i32,
        raw: bool,
    ) -> binder::Result<i32> {
        let default = app_ops::default_mode(code).ok_or_else(|| bad_op(code))?;
        let mode = match package {
            _ if device != DEVICE_ID_DEFAULT => app_ops::IGNORED,
            None => app_ops::IGNORED,
            Some(package) => match self.verify(uid, package) {
                Verified::Invalid | Verified::WrongUid => default,
                Verified::Ok if raw => self.mode(code, uid)?,
                Verified::Ok => self.evaluated(code, uid)?,
            },
        };
        atl_diag::answered(INTERFACE, method);
        Ok(mode)
    }

    /// The uid's mode with MODE_FOREGROUND evaluated against its state now (evaluateForegroundMode).
    fn evaluated(&self, code: i32, uid: i32) -> binder::Result<i32> {
        let mode = self.mode(code, uid)?;
        if mode != app_ops::FOREGROUND {
            return Ok(mode);
        }
        let view = (self.uids)(u32::try_from(uid).unwrap_or(u32::MAX));
        Ok(eval_mode(code, mode, &view))
    }
}

/// verifyIncomingOp's IllegalArgumentException.
fn bad_op(code: i32) -> Status {
    Status::new_exception_str(
        ExceptionCode::ILLEGAL_ARGUMENT,
        Some(format!("Bad operation #{code}")),
    )
}

fn non_null(package: Option<&str>) -> binder::Result<&str> {
    package.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))
}

impl Interface for AppOps {}

impl IAppOpsService for AppOps {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#checkOperation(&self, code: i32, uid: i32, package: Option<&str>) -> binder::Result<i32> {
        self.check(
            "checkOperation",
            code,
            uid,
            package,
            DEVICE_ID_DEFAULT,
            false,
        )
    }

    fn r#checkOperationRaw(
        &self,
        code: i32,
        uid: i32,
        package: Option<&str>,
        _attribution_tag: Option<&str>,
    ) -> binder::Result<i32> {
        self.check(
            "checkOperationRaw",
            code,
            uid,
            package,
            DEVICE_ID_DEFAULT,
            true,
        )
    }

    /// AppOpsManager's mode cache asks this one (checkOpNoThrow with app-op mode caching on).
    fn r#checkOperationForDevice(
        &self,
        code: i32,
        uid: i32,
        package: Option<&str>,
        _attribution_tag: Option<&str>,
        device: i32,
    ) -> binder::Result<i32> {
        self.check("checkOperationForDevice", code, uid, package, device, false)
    }

    fn r#checkOperationRawForDevice(
        &self,
        code: i32,
        uid: i32,
        package: Option<&str>,
        _attribution_tag: Option<&str>,
        device: i32,
    ) -> binder::Result<i32> {
        self.check(
            "checkOperationRawForDevice",
            code,
            uid,
            package,
            device,
            true,
        )
    }

    /// noteOperation: another uid's op needs UPDATE_APP_OPS_STATS (verifyIncomingUid); an invalid or foreign package is MODE_ERRORED; else the uid's mode. Nothing is recorded.
    fn r#noteOperation(
        &self,
        code: i32,
        uid: i32,
        package: Option<&str>,
        attribution_tag: Option<&str>,
        _should_collect_async_noted_op: bool,
        _message: Option<&str>,
        _should_collect_message: bool,
    ) -> binder::Result<Option<SyncNotedAppOp>> {
        let caller = ThreadState::get_calling_uid();
        if i64::from(caller) != i64::from(uid)
            && !permission::is_system_or_root(caller)
            && !self
                .permissions
                .holds(caller, &[UPDATE_APP_OPS_STATS], false)
        {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "noteOperation: uid {caller} lacks {UPDATE_APP_OPS_STATS} for uid {uid}"
                )),
            ));
        }
        if app_ops::default_mode(code).is_none() {
            return Err(bad_op(code));
        }
        let package = non_null(package)?;
        let op_mode = match self.verify(uid, package) {
            Verified::Invalid | Verified::WrongUid => app_ops::ERRORED,
            // noteOperationUnchecked :3480-3510: the package mode through uidState.evalMode
            Verified::Ok => self.evaluated(code, uid)?,
        };
        atl_diag::answered(INTERFACE, "noteOperation (not recorded)");
        Ok(Some(SyncNotedAppOp {
            op_mode,
            op_code: code,
            attribution_tag: attribution_tag.map(str::to_owned),
            package_name: Some(package.to_owned()),
        }))
    }

    /// shouldCollectNotes (:4602): AppOpsManager asks once per op before its first noteOp, to decide whether noted ops are reported back to the app.
    fn r#shouldCollectNotes(&self, code: i32) -> binder::Result<bool> {
        app_ops::should_collect_notes(code, |p| self.permissions.is_runtime_or_app_op(p))
            .ok_or_else(|| bad_op(code))
    }

    /// reportRuntimeAppOpAccessMessageAndGetConfig (:7081): AppOpsManager's sConfig starts expired, so an app's first self-noted op with collected notes reports here and stores the answer.
    /// No package is ever sampled (AOSP samples one rarely-used package at a time, switchPackageIfBootTimeOrRarelyUsedLocked), so every caller gets the not-sampled answer: OP_NONE, distance 0,
    /// an hour from now in epoch milliseconds (AOSP's Instant.now(), compared against elapsedRealtime by the client: never expires in practice).
    fn r#reportRuntimeAppOpAccessMessageAndGetConfig(
        &self,
        package: Option<&str>,
        _noted: Option<&SyncNotedAppOp>,
        _message: Option<&str>,
    ) -> binder::Result<Option<MessageSamplingConfig>> {
        non_null(package)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        atl_diag::answered(
            INTERFACE,
            "reportRuntimeAppOpAccessMessageAndGetConfig (not sampled)",
        );
        Ok(Some(MessageSamplingConfig {
            sampled_op_code: OP_NONE,
            acceptable_left_distance: 0,
            expiration_time_since_boot_millis: now.saturating_add(SAMPLING_CONFIG_LIFETIME_MS),
        }))
    }

    /// checkPackage: MODE_ALLOWED when `package` is installed under `uid` and visible to the caller, else MODE_ERRORED.
    fn r#checkPackage(&self, uid: i32, package: Option<&str>) -> binder::Result<i32> {
        let package = non_null(package)?;
        Ok(match self.verify(uid, package) {
            Verified::Ok => app_ops::ALLOWED,
            Verified::Invalid | Verified::WrongUid => app_ops::ERRORED,
        })
    }
}

/// The `appops` binder, for `addService`.
pub fn new_binder(
    permissions: Arc<Permissions>,
    packages: Arc<Packages>,
    uids: UidViews,
) -> SpIBinder {
    BnAppOpsService::new_binder(
        AppOps {
            permissions,
            packages,
            uids,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AppOpsManager.OP_TAKE_AUDIO_FOCUS: MODE_FOREGROUND by default, no capability.
    const OP_TAKE_AUDIO_FOCUS: i32 = 32;

    fn view(state: Option<ProcessState>, restricted_since: Option<u64>, now: u64) -> UidView {
        UidView {
            state,
            top_since: None,
            restricted_since,
            now,
        }
    }

    /// Per state (processStateToUidState): TOP 200 and FGS 400 <= FOREGROUND 500 -> ALLOWED; SERVICE / RECEIVER (BACKGROUND 600), CACHED_EMPTY (CACHED 700) and no process -> IGNORED.
    #[test]
    fn foreground_mode_by_uid_state() {
        for code in [OP_TAKE_AUDIO_FOCUS, OP_CONTROL_AUDIO] {
            let eval = |s| eval_mode(code, app_ops::FOREGROUND, &view(s, None, 0));
            assert_eq!(eval(Some(ProcessState::Top)), app_ops::ALLOWED);
            assert_eq!(
                eval(Some(ProcessState::ForegroundService)),
                app_ops::ALLOWED
            );
            assert_eq!(eval(Some(ProcessState::Service)), app_ops::IGNORED);
            assert_eq!(eval(Some(ProcessState::Receiver)), app_ops::IGNORED);
            assert_eq!(eval(Some(ProcessState::CachedEmpty)), app_ops::IGNORED);
            assert_eq!(eval(None), app_ops::IGNORED);
        }
    }

    /// Only MODE_FOREGROUND is evaluated: other modes stand in every state.
    #[test]
    fn other_modes_stand() {
        let bg = view(Some(ProcessState::CachedEmpty), None, 0);
        let top = view(Some(ProcessState::Top), None, 0);
        assert_eq!(eval_mode(0, app_ops::ALLOWED, &bg), app_ops::ALLOWED);
        assert_eq!(eval_mode(0, app_ops::IGNORED, &top), app_ops::IGNORED);
        assert_eq!(eval_mode(0, app_ops::ERRORED, &top), app_ops::ERRORED);
    }

    /// Settle (updateUidProcState :262-277): a uid that lost its Task / FGS at 1000 stays foreground until 1000 + 5000, then IGNORED; a dead uid (no process) commits NONEXISTENT at once.
    #[test]
    fn leaving_foreground_settles_for_five_seconds() {
        let eval = |s, now| {
            eval_mode(
                OP_TAKE_AUDIO_FOCUS,
                app_ops::FOREGROUND,
                &view(s, Some(1_000), now),
            )
        };
        assert_eq!(eval(Some(ProcessState::Service), 1_000), app_ops::ALLOWED);
        assert_eq!(
            eval(Some(ProcessState::CachedEmpty), 5_999),
            app_ops::ALLOWED
        );
        assert_eq!(
            eval(Some(ProcessState::CachedEmpty), 6_000),
            app_ops::IGNORED
        );
        assert_eq!(eval(None, 2_000), app_ops::IGNORED);
    }
}
