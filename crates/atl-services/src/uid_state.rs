// SPDX-License-Identifier: MIT
//! Per-uid state (AOSP UidRecord): the idle flag, the background clock and its settle sweep, the temp allowlist; and the answers that read them: start mode, FGS start from background, process state, the headless end rule.
//! AOSP references (android-16.0.0_r1, services/core/java/com/android/server/am/ unless named): UidRecord.java :187 (new uid idle), OomAdjuster.java :1585-1631 (background clock, cleared while active), :3984-4043 (idleUids sweep),
//! ActivityManagerService.java :6360-6436 (getAppStartModeLOSP, isOnDeviceIdleAllowlistLOSP), :8109-8112 (isUidActiveLOSP), :15746-15767 (tempAllowlistUidLocked), FgsTempAllowList.java :76-127, ActiveServices.java :1036-1054 and :8954-8985 (FGS start gate), ProcessStateEnum.aidl :36-95.
//! Pure rules, no clock or binder. `now` is elapsed ms. A call that starts a countdown returns the absolute deadline for am_handler (S1); the caller reports back with `settle` / `on_temp_expired` when it fires. The last process leaving calls `on_uid_gone` to clear the app-ops foreground settle (`restricted_since`); uid active reasons (Tasks, FGS, temp grants) are removed by the caller itself.
//! Not modelled: isPendingTopUid (AMS :6362), the legacy restricted-bucket and battery branch (:6288-6296), the background allowlist (:6338), instant apps (:6378), strict background (forcedStandby), app ops other than the default.

use std::collections::BTreeMap;

/// ActivityManagerConstants.java:253 DEFAULT_BACKGROUND_SETTLE_TIME (`60 * 1000`): a uid is idle this long after its last active reason ends.
/// The headless empty-process grace (plan "Linux conventions") defaults to the same value, so a cold start lives at least one settle period.
pub const BACKGROUND_SETTLE_TIME_MS: u64 = 60 * 1000;
/// Build.java:1171 VERSION_CODES.O: targetSdk from which a background start is DELAYED_RIGID (AMS appRestrictedInBackgroundLOSP :6281).
pub const SDK_O: i32 = 26;
/// Build.java:1246 VERSION_CODES.S: targetSdk from which the background FGS start restriction applies (ActiveServices.java :546-548, :555).
pub const SDK_S: i32 = 31;

/// AOSP ProcessStateEnum values (ProcessStateEnum.aidl :36-95). Lower is more important; the discriminant is the AOSP value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum ProcessState {
    /// :36 hosts a visible Task.
    Top = 2,
    /// :42 hosts a foreground service.
    ForegroundService = 4,
    /// :62 a started service, or a service bound with BIND_AUTO_CREATE.
    Service = 10,
    /// :68 an active or pending receiver.
    Receiver = 11,
    /// :95 holds none of the above (a provider is not a connection here, plan P4).
    CachedEmpty = 19,
}

/// What a process holds, derived by the caller (S5). A service counts when it is needed: started, or bound with BIND_AUTO_CREATE.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Holdings {
    /// A Task (visible activity) of this process.
    pub task: bool,
    /// A foreground service.
    pub fgs: bool,
    /// A started service (startRequested).
    pub started_service: bool,
    /// Bound by a live client with BIND_AUTO_CREATE.
    pub auto_create_binding: bool,
    /// An active or pending receiver in the broadcast dispatcher (S4).
    pub receiver: bool,
    /// A published content provider. Never ended: provider connections are not counted (plan P4).
    pub provider: bool,
    /// Running instrumentation.
    pub instrumented: bool,
    /// Manifest FLAG_PERSISTENT.
    pub persistent: bool,
}

/// The state an app can observe for a process: the best of its holdings, CACHED_EMPTY when there are none.
pub fn process_state(holdings: &Holdings) -> ProcessState {
    if holdings.task {
        ProcessState::Top
    } else if holdings.fgs {
        ProcessState::ForegroundService
    } else if holdings.started_service || holdings.auto_create_binding {
        ProcessState::Service
    } else if holdings.receiver {
        ProcessState::Receiver
    } else {
        ProcessState::CachedEmpty
    }
}

/// What a service outside the activity manager reads of a uid at one instant: AMInternal.getUidProcessState plus the uid observer events ShortcutService and AppOpsService keep.
/// The activity manager answers it (ActivityManager::uid_view); shortcut.rs and appops.rs read it through [`UidViews`], so no second tracker exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidView {
    /// The uid's state: the best of its attached processes' (UidRecord.curProcState = min over its processes). None = no attached process (PROCESS_STATE_NONEXISTENT).
    pub state: Option<ProcessState>,
    /// Uids::top_since.
    pub top_since: Option<u64>,
    /// Uids::restricted_since.
    pub restricted_since: Option<u64>,
    /// Elapsed ms on the same clock as the two times above.
    pub now: u64,
}

/// The uid view source a service is given at registration.
pub type UidViews = std::sync::Arc<dyn Fn(u32) -> UidView + Send + Sync>;

/// Headless end rule (plan "Linux conventions", no cached processes): ends once nothing holds the process, its uid is idle, and it has been empty for `grace_ms` since `empty_since`.
/// Times are elapsed ms; the caller sets `empty_since` when the process becomes empty.
pub fn ends(
    holdings: &Holdings,
    uid_idle: bool,
    empty_since: u64,
    grace_ms: u64,
    now: u64,
) -> bool {
    uid_idle && !holds_any(holdings) && now.saturating_sub(empty_since) >= grace_ms
}

fn holds_any(h: &Holdings) -> bool {
    h.task
        || h.fgs
        || h.started_service
        || h.auto_create_binding
        || h.receiver
        || h.provider
        || h.instrumented
        || h.persistent
}

/// AMS getAppStartModeLOSP's answer, reduced to the two S3 needs (ActivityManager.java :1235-1242).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    Normal,
    DelayedRigid,
}

/// Result of `Uids::settle`. BecameIdle: the sweep ran, so the caller runs stopInBackgroundLocked (ActiveServices.java :1685), even when the flag was already set (a never-active uid, idleUids :4000 `idleTime == 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    BecameIdle,
    Unchanged,
}

/// FGS-background restriction (ActiveServices.isBgFgsRestrictionEnabled :8954-8985, flag at its default on): the service targets >= S, and the caller too. SYSTEM_UID (`None`) counts as S+ (:8974).
pub fn fgs_bg_restricted(service_target_sdk: i32, caller_target_sdk: Option<i32>) -> bool {
    let caller_ok = caller_target_sdk.is_none_or(|t| t >= SDK_S);
    service_target_sdk >= SDK_S && caller_ok
}

/// ActiveServices.startServiceLocked's background check (:1080-1105): refused (`?`) when the start is neither of a started service nor a foreground one
/// (AS: `forcedStandby || (!r.startRequested && !fgRequired)`, forcedStandby not modelled) and the target uid's start mode is not NORMAL.
pub fn background_start_refused(
    uids: &Uids,
    uid: u32,
    target_sdk: i32,
    persistent: bool,
    start_requested: bool,
    require_foreground: bool,
    now: u64,
) -> bool {
    !start_requested
        && !require_foreground
        && uids.start_mode(uid, target_sdk, persistent, now) != StartMode::Normal
}

/// BroadcastOptions temp allowlist type FOREGROUND_SERVICE_ALLOWED (PowerExemptionManager.java :94) and FOREGROUND_SERVICE_NOT_ALLOWED (:98).
/// NONE is -1 (:90): any other type grants nothing. APP_FREEZING_DELAYED (:107) is an unfreeze, not a grant (BroadcastQueueImpl :1178-1186).
pub const TEMP_TYPE_NONE: i32 = -1;
pub const TEMP_TYPE_FGS_ALLOWED: i32 = 0;
pub const TEMP_TYPE_FGS_NOT_ALLOWED: i32 = 1;
pub const TEMP_TYPE_APP_FREEZING_DELAYED: i32 = 1 << 2;
/// PowerExemptionManager.REASON_DENIED (:127). DeviceIdleController.getTempAllowListType maps it to NONE (:3196-3197).
pub const REASON_DENIED: i32 = -1;

/// A temp allowlist grant as the receiving uid holds it: `duration_ms` (> 0) and whether FGS starts are allowed (AMS tempAllowlistUidLocked :15746-15765; FGS-typed grants go to
/// mFgsStartTempAllowList).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempGrant {
    pub duration_ms: u64,
    pub fgs: bool,
}

/// The grant a sender's temp allowlist request makes (BroadcastOptions.setTemporaryAppAllowlist -> tempAllowlistUidLocked): none for a duration <= 0, for REASON_DENIED
/// (DeviceIdleController :3196-3197 -> NONE, AMS :15751-15753 returns), for a type other than the two FGS ones, and for APP_FREEZING_DELAYED (not a grant; the caller counts it).
pub fn grant_of(duration_ms: i64, kind: i32, reason: i32) -> Option<TempGrant> {
    let duration_ms = u64::try_from(duration_ms).ok().filter(|d| *d > 0)?;
    if reason == REASON_DENIED {
        return None;
    }
    match kind {
        TEMP_TYPE_FGS_ALLOWED => Some(TempGrant {
            duration_ms,
            fgs: true,
        }),
        TEMP_TYPE_FGS_NOT_ALLOWED => Some(TempGrant {
            duration_ms,
            fgs: false,
        }),
        _ => None,
    }
}

/// Per-uid state. A uid not in the map is idle with nothing held (a new uid: UidRecord.java :187 mIdle = true).
#[derive(Debug, Default)]
pub struct Uids {
    uids: BTreeMap<u32, Uid>,
}

#[derive(Debug)]
struct Uid {
    tasks: u32,
    fgs: u32,
    /// AOSP UidRecord.mIdle: false while an active reason holds, true once the sweep has run.
    idle: bool,
    /// AOSP lastBackgroundTime: set when the uid turns background (OomAdjuster :1592-1595), cleared while active (:1626).
    bg_since: Option<u64>,
    /// Sweep deadline, while the sweep has not run (`bg_since` + BACKGROUND_SETTLE_TIME_MS).
    settle_at: Option<u64>,
    /// Expiry of the latest temp grant of any kind (AMS mPendingTempAllowlist, :15756). Valid while now < until.
    temp_until: Option<u64>,
    /// Expiry of the FGS-typed grants (AMS mFgsStartTempAllowList, :15762-15765). Valid while now < until.
    fgs_until: Option<u64>,
    /// When the uid last turned TOP (its first Task appeared): the uid observer's foreground event ShortcutService records (UidView::top_since).
    top_since: Option<u64>,
    /// When the uid's last Task and FGS ended, while neither holds again and a process of the uid stays alive (UidView::restricted_since).
    restricted_since: Option<u64>,
}

impl Uid {
    fn new() -> Self {
        Self {
            tasks: 0,
            fgs: 0,
            idle: true,
            bg_since: None,
            settle_at: None,
            temp_until: None,
            fgs_until: None,
            top_since: None,
            restricted_since: None,
        }
    }

    /// Task or FGS: the process states at or above FOREGROUND_SERVICE this uid can reach.
    fn unrestricted(&self) -> bool {
        self.tasks > 0 || self.fgs > 0
    }

    /// After a Task or FGS ended at `now`: the restricted clock starts when neither is left.
    fn restriction_observed(&mut self, now: u64) {
        if !self.unrestricted() && self.restricted_since.is_none() {
            self.restricted_since = Some(now);
        }
    }

    /// An active reason holds at `now`: a Task, an FGS, or a live temp grant (OomAdjuster :1585-1586: not background, or allowlisted).
    fn active(&self, now: u64) -> bool {
        self.tasks > 0 || self.fgs > 0 || self.temp_until.is_some_and(|until| now < until)
    }
}

impl Uids {
    fn uid_mut(&mut self, uid: u32) -> &mut Uid {
        self.uids.entry(uid).or_insert_with(Uid::new)
    }

    /// The idle flag (AOSP UidRecord.isIdle; the negation is isUidActiveLOSP :8109-8112).
    pub fn is_idle(&self, uid: u32) -> bool {
        self.uids.get(&uid).is_none_or(|u| u.idle)
    }

    /// A Task of `uid` appears at `now` (its process is TOP): the uid is active and its background clock stops (OomAdjuster :1620-1627). The first Task marks the uid's turn to TOP.
    pub fn on_task_added(&mut self, uid: u32, now: u64) {
        let u = self.uid_mut(uid);
        if u.tasks == 0 {
            u.top_since = Some(now);
        }
        u.tasks += 1;
        u.restricted_since = None;
        set_active(u);
    }

    /// A Task of `uid` goes. Returns the sweep deadline to arm when no active reason is left.
    pub fn on_task_removed(&mut self, uid: u32, now: u64) -> Option<u64> {
        let u = self.uid_mut(uid);
        u.tasks = u.tasks.saturating_sub(1);
        u.restriction_observed(now);
        background_observed(u, now)
    }

    /// A foreground service of `uid` starts: the uid is active.
    pub fn on_fgs_started(&mut self, uid: u32) {
        let u = self.uid_mut(uid);
        u.fgs += 1;
        u.restricted_since = None;
        set_active(u);
    }

    /// A foreground service of `uid` stops. Returns the sweep deadline to arm when no active reason is left.
    pub fn on_fgs_stopped(&mut self, uid: u32, now: u64) -> Option<u64> {
        let u = self.uid_mut(uid);
        u.fgs = u.fgs.saturating_sub(1);
        u.restriction_observed(now);
        background_observed(u, now)
    }

    /// When `uid` last turned TOP (None: never in this atld run).
    pub fn top_since(&self, uid: u32) -> Option<u64> {
        self.uids.get(&uid).and_then(|u| u.top_since)
    }

    /// When `uid` lost its last Task and FGS; None while one holds, before either ever did, or after `on_uid_gone`.
    pub fn restricted_since(&self, uid: u32) -> Option<u64> {
        self.uids.get(&uid).and_then(|u| u.restricted_since)
    }

    /// No live process of `uid` remains (UID_STATE_NONEXISTENT): clears `restricted_since` so a later process gets no settle time from before (AppOpsUidStateTrackerImpl.updateUidProcState :242-243, commitUidPendingState :402-405).
    pub fn on_uid_gone(&mut self, uid: u32) {
        if let Some(u) = self.uids.get_mut(&uid) {
            u.restricted_since = None;
        }
    }

    /// A temp allowlist grant (AMS tempAllowlistUidLocked :15746-15767; `fgs` for the FGS-allowed type). Returns the expiry to arm for `on_temp_expired`.
    /// A later expiry wins (FgsTempAllowList.add :98-101 for the FGS list; the pending list's merge is not read). Zero duration grants nothing.
    pub fn on_temp_allowlist(
        &mut self,
        uid: u32,
        now: u64,
        duration_ms: u64,
        fgs: bool,
    ) -> Option<u64> {
        if duration_ms == 0 {
            return None;
        }
        let until = now.saturating_add(duration_ms);
        let u = self.uid_mut(uid);
        u.temp_until = Some(u.temp_until.map_or(until, |t| t.max(until)));
        if fgs {
            u.fgs_until = Some(u.fgs_until.map_or(until, |t| t.max(until)));
        }
        set_active(u);
        u.temp_until
    }

    /// The temp expiry armed by `on_temp_allowlist` fired at `now`. Returns the sweep deadline when no active reason is left.
    pub fn on_temp_expired(&mut self, uid: u32, now: u64) -> Option<u64> {
        let u = self.uids.get_mut(&uid)?;
        if u.temp_until.is_some_and(|until| until <= now) {
            u.temp_until = None;
        }
        if u.fgs_until.is_some_and(|until| until <= now) {
            u.fgs_until = None;
        }
        background_observed(u, now)
    }

    /// A process of `uid` starts. Only the first background observation of a never-settled uid starts the clock; later ones arm nothing.
    /// Returns the sweep deadline to arm.
    pub fn on_process_started(&mut self, uid: u32, now: u64) -> Option<u64> {
        background_observed(self.uid_mut(uid), now)
    }

    /// The sweep deadline fired at `now`. BecameIdle when it is due and no active reason holds (idleUids :3996-4008 sets idle and calls doStopUidLocked).
    pub fn settle(&mut self, uid: u32, now: u64) -> Settle {
        let Some(u) = self.uids.get_mut(&uid) else {
            return Settle::Unchanged;
        };
        match u.settle_at {
            Some(at) if at <= now && !u.active(now) => {
                u.idle = true;
                u.settle_at = None;
                Settle::BecameIdle
            }
            _ => Settle::Unchanged,
        }
    }

    /// AMS getAppStartModeLOSP (:6360-6420), reduced: a non-idle uid is NORMAL (:6369, :6419); an idle one is NORMAL when persistent (:6329) or temp-granted (:6347, mPendingTempAllowlist);
    /// otherwise DELAYED_RIGID for targetSdk >= O (:6281). Legacy targetSdk keeps the default app op (NORMAL).
    pub fn start_mode(&self, uid: u32, target_sdk: i32, persistent: bool, now: u64) -> StartMode {
        if !self.is_idle(uid) {
            return StartMode::Normal;
        }
        if persistent || self.temp_valid(uid, now) {
            return StartMode::Normal;
        }
        if target_sdk >= SDK_O {
            StartMode::DelayedRigid
        } else {
            StartMode::Normal
        }
    }

    /// ActiveServices :1037 gate, reduced: an FGS start from background needs the uid active, or an FGS-typed grant still valid (AMS isAllowlistedForFgsStartLOSP :6446-6451).
    /// An unknown uid has neither.
    pub fn fgs_start_allowed(&self, uid: u32, now: u64) -> bool {
        self.uids
            .get(&uid)
            .is_some_and(|u| !u.idle || u.fgs_until.is_some_and(|until| now < until))
    }

    fn temp_valid(&self, uid: u32, now: u64) -> bool {
        self.uids
            .get(&uid)
            .is_some_and(|u| u.temp_until.is_some_and(|until| now < until))
    }
}

/// An active reason was added: the uid is active and its background clock stops (OomAdjuster :1620-1627).
fn set_active(u: &mut Uid) {
    u.idle = false;
    u.bg_since = None;
    u.settle_at = None;
}

/// No active reason at `now`: the background clock starts once (OomAdjuster :1592-1595, sweep armed at :1609). Returns the sweep deadline to arm.
fn background_observed(u: &mut Uid, now: u64) -> Option<u64> {
    if u.active(now) || u.bg_since.is_some() {
        return None;
    }
    u.bg_since = Some(now);
    let at = now.saturating_add(BACKGROUND_SETTLE_TIME_MS);
    u.settle_at = Some(at);
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any app uid: the rules key on the uid alone.
    const APP_UID: u32 = 10_000;
    /// ActivityManagerConstants.java:241 DEFAULT_BOOT_TIME_TEMP_ALLOWLIST_DURATION (`20 * 1000`): the BOOT_COMPLETED grant.
    const BOOT_TEMP_ALLOWLIST_MS: u64 = 20 * 1000;

    #[test]
    fn grant_follows_the_aosp_type_and_reason_rules() {
        // PowerExemptionManager.java :90 NONE = -1, :94 FGS_ALLOWED = 0, :98 FGS_NOT_ALLOWED = 1, :107 APP_FREEZING_DELAYED = 1 << 2, :127 REASON_DENIED = -1
        const TEN_SECONDS: i64 = 10_000;
        assert_eq!(
            grant_of(TEN_SECONDS, TEMP_TYPE_FGS_ALLOWED, 302),
            Some(TempGrant {
                duration_ms: 10_000,
                fgs: true
            })
        );
        assert_eq!(
            grant_of(TEN_SECONDS, TEMP_TYPE_FGS_NOT_ALLOWED, 302),
            Some(TempGrant {
                duration_ms: 10_000,
                fgs: false
            })
        );
        // DeviceIdleController.getTempAllowListType maps REASON_DENIED to NONE, whatever the type
        assert_eq!(
            grant_of(TEN_SECONDS, TEMP_TYPE_FGS_NOT_ALLOWED, REASON_DENIED),
            None
        );
        assert_eq!(
            grant_of(TEN_SECONDS, TEMP_TYPE_FGS_ALLOWED, REASON_DENIED),
            None
        );
        assert_eq!(grant_of(0, TEMP_TYPE_FGS_ALLOWED, 302), None);
        assert_eq!(grant_of(-5, TEMP_TYPE_FGS_ALLOWED, 302), None);
        assert_eq!(grant_of(TEN_SECONDS, -1, 302), None);
        assert_eq!(
            grant_of(TEN_SECONDS, TEMP_TYPE_APP_FREEZING_DELAYED, 302),
            None
        );
    }

    #[test]
    fn constants_match_their_aosp_source() {
        // ActivityManagerConstants.java:253 `60 * 1000`; Build.java:1171 O = 26, :1246 S = 31
        assert_eq!(BACKGROUND_SETTLE_TIME_MS, 60_000);
        assert_eq!(SDK_O, 26);
        assert_eq!(SDK_S, 31);
    }

    #[test]
    fn process_state_values_and_order_are_aosp() {
        // ProcessStateEnum.aidl: TOP 2 (:36), FOREGROUND_SERVICE 4 (:42), SERVICE 10 (:62), RECEIVER 11 (:68), CACHED_EMPTY 19 (:95)
        assert_eq!(ProcessState::Top as i32, 2);
        assert_eq!(ProcessState::ForegroundService as i32, 4);
        assert_eq!(ProcessState::Service as i32, 10);
        assert_eq!(ProcessState::Receiver as i32, 11);
        assert_eq!(ProcessState::CachedEmpty as i32, 19);
        assert!(ProcessState::Top < ProcessState::ForegroundService);
        assert!(ProcessState::Service < ProcessState::Receiver);
        assert!(ProcessState::Receiver < ProcessState::CachedEmpty);
    }

    #[test]
    fn process_state_is_the_best_holding() {
        let none = Holdings::default();
        assert_eq!(process_state(&none), ProcessState::CachedEmpty);
        // a provider is not a connection here (P4): nothing above CACHED_EMPTY
        let provider = Holdings {
            provider: true,
            ..Default::default()
        };
        assert_eq!(process_state(&provider), ProcessState::CachedEmpty);
        let receiver = Holdings {
            receiver: true,
            ..Default::default()
        };
        assert_eq!(process_state(&receiver), ProcessState::Receiver);
        let bound = Holdings {
            auto_create_binding: true,
            ..Default::default()
        };
        assert_eq!(process_state(&bound), ProcessState::Service);
        // SERVICE (10) is more important than RECEIVER (11)
        let service_and_receiver = Holdings {
            started_service: true,
            receiver: true,
            ..Default::default()
        };
        assert_eq!(process_state(&service_and_receiver), ProcessState::Service);
        let fgs = Holdings {
            fgs: true,
            started_service: true,
            ..Default::default()
        };
        assert_eq!(process_state(&fgs), ProcessState::ForegroundService);
        let visible = Holdings {
            task: true,
            fgs: true,
            ..Default::default()
        };
        assert_eq!(process_state(&visible), ProcessState::Top);
    }

    #[test]
    fn new_uid_is_idle_and_target_sdk_decides_start_mode() {
        let uids = Uids::default();
        // UidRecord.java:187 mIdle = true; AMS :6281 targetSdk >= O (26) is DELAYED_RIGID, 25 stays NORMAL (app op default)
        assert!(uids.is_idle(APP_UID));
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 0),
            StartMode::DelayedRigid
        );
        assert_eq!(uids.start_mode(APP_UID, 25, false, 0), StartMode::Normal);
        // AMS :6329 persistent is NORMAL
        assert_eq!(uids.start_mode(APP_UID, 26, true, 0), StartMode::Normal);
    }

    #[test]
    fn active_uid_stays_normal_until_the_settle_sweep() {
        let mut uids = Uids::default();
        uids.on_task_added(APP_UID, 0);
        assert!(!uids.is_idle(APP_UID));
        assert_eq!(uids.start_mode(APP_UID, 26, false, 0), StartMode::Normal);
        // the last Task goes at 10 000: sweep due at 10 000 + 60 000
        assert_eq!(uids.on_task_removed(APP_UID, 10_000), Some(70_000));
        assert_eq!(uids.settle(APP_UID, 69_999), Settle::Unchanged);
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 69_999),
            StartMode::Normal
        );
        // idle at exactly +60 000 ms (idleUids :3990 `bgTime <= now - BACKGROUND_SETTLE_TIME`)
        assert_eq!(uids.settle(APP_UID, 70_000), Settle::BecameIdle);
        assert!(uids.is_idle(APP_UID));
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 70_000),
            StartMode::DelayedRigid
        );
        // swept once
        assert_eq!(uids.settle(APP_UID, 70_000), Settle::Unchanged);
    }

    #[test]
    fn a_new_reason_cancels_the_pending_sweep() {
        let mut uids = Uids::default();
        uids.on_task_added(APP_UID, 0);
        assert_eq!(uids.on_task_removed(APP_UID, 10_000), Some(70_000));
        // back at 30 000: the background clock is cleared (OomAdjuster :1626)
        uids.on_task_added(APP_UID, 30_000);
        assert_eq!(uids.settle(APP_UID, 70_000), Settle::Unchanged);
        assert!(!uids.is_idle(APP_UID));
        // the next background period gets its own clock: 80 000 + 60 000
        assert_eq!(uids.on_task_removed(APP_UID, 80_000), Some(140_000));
    }

    #[test]
    fn first_background_process_arms_the_sweep_once() {
        let mut uids = Uids::default();
        // a headless process of a never-active uid starts at 5 000: the sweep is due at 5 000 + 60 000 (OomAdjuster :1592-1595; idleUids :3996-4008 with idleTime == 0)
        assert_eq!(uids.on_process_started(APP_UID, 5_000), Some(65_000));
        // a second process does not restart the clock
        assert_eq!(uids.on_process_started(APP_UID, 6_000), None);
        // the sweep runs though the flag was already set: the caller stops the uid's services
        assert_eq!(uids.settle(APP_UID, 65_000), Settle::BecameIdle);
        assert!(uids.is_idle(APP_UID));
        // swept once: a later process start arms nothing
        assert_eq!(uids.on_process_started(APP_UID, 70_000), None);
        assert_eq!(uids.settle(APP_UID, 200_000), Settle::Unchanged);
    }

    #[test]
    fn temp_allowlist_keeps_the_uid_active_then_settles() {
        let mut uids = Uids::default();
        // a BOOT_COMPLETED grant of 20 s at 1 000 (AMC :241): expires at 1 000 + 20 000
        assert_eq!(
            uids.on_temp_allowlist(APP_UID, 1_000, BOOT_TEMP_ALLOWLIST_MS, false),
            Some(21_000)
        );
        // AMS :6347 counts the pending temp allowlist: NORMAL while it lasts
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 20_999),
            StartMode::Normal
        );
        // still valid at 20 999: no sweep
        assert_eq!(uids.on_temp_expired(APP_UID, 20_999), None);
        // expired at 21 000: the settle starts now (lastBackgroundTime set when leaving the allowlist, OomAdjuster :1592-1594)
        assert_eq!(uids.on_temp_expired(APP_UID, 21_000), Some(81_000));
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 21_000),
            StartMode::Normal
        );
        assert_eq!(uids.settle(APP_UID, 81_000), Settle::BecameIdle);
        assert_eq!(
            uids.start_mode(APP_UID, 26, false, 81_000),
            StartMode::DelayedRigid
        );
    }

    #[test]
    fn a_shorter_grant_does_not_shorten_the_entry() {
        let mut uids = Uids::default();
        assert_eq!(
            uids.on_temp_allowlist(APP_UID, 0, 60_000, false),
            Some(60_000)
        );
        // later expiry wins: 5 000 + 10 000 < 60 000 (FgsTempAllowList.add :98-101)
        assert_eq!(
            uids.on_temp_allowlist(APP_UID, 5_000, 10_000, false),
            Some(60_000)
        );
        // zero duration grants nothing
        assert_eq!(uids.on_temp_allowlist(APP_UID, 5_000, 0, false), None);
    }

    #[test]
    fn fgs_start_from_background_needs_an_active_uid_or_an_fgs_grant() {
        let mut uids = Uids::default();
        // a new uid is idle with nothing allowlisted: ActiveServices :1037 refuses
        assert!(!uids.fgs_start_allowed(APP_UID, 0));
        // an FGS-typed grant of 10 s from 0 (AMS :15762-15765) lets the start through while it lasts
        assert_eq!(
            uids.on_temp_allowlist(APP_UID, 0, 10_000, true),
            Some(10_000)
        );
        assert!(uids.fgs_start_allowed(APP_UID, 9_999));
        assert_eq!(uids.on_temp_expired(APP_UID, 10_000), Some(70_000));
        // active through the settle, then idle with the FGS entry expired
        assert!(uids.fgs_start_allowed(APP_UID, 69_999));
        assert_eq!(uids.settle(APP_UID, 70_000), Settle::BecameIdle);
        assert!(!uids.fgs_start_allowed(APP_UID, 70_000));
    }

    #[test]
    fn a_visible_task_allows_fgs_start_from_background() {
        let mut uids = Uids::default();
        uids.on_task_added(APP_UID, 0);
        assert!(uids.fgs_start_allowed(APP_UID, 0));
    }

    #[test]
    fn fgs_background_restriction_needs_both_targets_at_s() {
        // R = 30 (Build.java:1241); the service and the caller must both target S = 31 (ActiveServices :546-548, :8963-8985); SYSTEM_UID counts as S+ (:8974)
        assert!(fgs_bg_restricted(31, Some(31)));
        assert!(!fgs_bg_restricted(30, Some(31)));
        assert!(!fgs_bg_restricted(31, Some(30)));
        assert!(fgs_bg_restricted(31, None));
    }

    #[test]
    fn headless_process_ends_after_one_settle_period_when_idle_and_empty() {
        let empty = Holdings::default();
        // grace = one settle period: a cold start lives at least that long
        assert!(!ends(
            &empty,
            true,
            0,
            BACKGROUND_SETTLE_TIME_MS,
            BACKGROUND_SETTLE_TIME_MS - 1
        ));
        assert!(ends(
            &empty,
            true,
            0,
            BACKGROUND_SETTLE_TIME_MS,
            BACKGROUND_SETTLE_TIME_MS
        ));
        // an active uid keeps the process
        assert!(!ends(&empty, false, 0, BACKGROUND_SETTLE_TIME_MS, u64::MAX));
    }

    #[test]
    fn every_holding_keeps_the_process() {
        let holdings = [
            Holdings {
                task: true,
                ..Default::default()
            },
            Holdings {
                fgs: true,
                ..Default::default()
            },
            Holdings {
                started_service: true,
                ..Default::default()
            },
            Holdings {
                auto_create_binding: true,
                ..Default::default()
            },
            Holdings {
                receiver: true,
                ..Default::default()
            },
            Holdings {
                provider: true,
                ..Default::default()
            },
            Holdings {
                instrumented: true,
                ..Default::default()
            },
            Holdings {
                persistent: true,
                ..Default::default()
            },
        ];
        for h in &holdings {
            assert!(
                !ends(h, true, 0, BACKGROUND_SETTLE_TIME_MS, u64::MAX),
                "{h:?}"
            );
        }
    }

    /// ActiveServices.startServiceLocked :1085-1100: an idle uid, targetSdk 26+, not started, not foreground -> refused (`?`).
    #[test]
    fn background_start_of_an_idle_app_is_refused() {
        let uids = Uids::default();
        assert!(background_start_refused(
            &uids, APP_UID, SDK_O, false, false, false, 0
        ));
        // targetSdk below O: NORMAL (AMS :6281 applies DELAYED_RIGID from O on)
        assert!(!background_start_refused(
            &uids,
            APP_UID,
            SDK_O - 1,
            false,
            false,
            false,
            0
        ));
    }

    /// The check is skipped for a service already started (`!r.startRequested`) and for a foreground start (`!fgRequired`), and an active, persistent or temp-granted uid passes.
    #[test]
    fn background_start_check_skips_started_and_foreground_starts() {
        let mut uids = Uids::default();
        assert!(!background_start_refused(
            &uids, APP_UID, 36, false, true, false, 0
        ));
        assert!(!background_start_refused(
            &uids, APP_UID, 36, false, false, true, 0
        ));
        assert!(!background_start_refused(
            &uids, APP_UID, 36, true, false, false, 0
        ));
        uids.on_temp_allowlist(APP_UID, 0, BOOT_TEMP_ALLOWLIST_MS, false);
        assert!(!background_start_refused(
            &uids,
            APP_UID,
            36,
            false,
            false,
            false,
            BOOT_TEMP_ALLOWLIST_MS - 1
        ));
        // the grant ends at its expiry, but the uid stays non-idle until its sweep runs 60 s later (AMC :253; AMS :6419 reads isIdle, not the grant)
        let sweep = BOOT_TEMP_ALLOWLIST_MS + BACKGROUND_SETTLE_TIME_MS;
        assert_eq!(
            uids.on_temp_expired(APP_UID, BOOT_TEMP_ALLOWLIST_MS),
            Some(sweep)
        );
        assert!(!background_start_refused(
            &uids,
            APP_UID,
            36,
            false,
            false,
            false,
            BOOT_TEMP_ALLOWLIST_MS
        ));
        assert_eq!(uids.settle(APP_UID, sweep), Settle::BecameIdle);
        assert!(background_start_refused(
            &uids, APP_UID, 36, false, false, false, sweep
        ));
    }

    /// A Task of the uid makes it active: its background starts pass until the settle sweep (BACKGROUND_SETTLE_TIME_MS, AMC :253) has run.
    #[test]
    fn active_uid_passes_until_it_settles() {
        let mut uids = Uids::default();
        uids.on_task_added(APP_UID, 0);
        assert!(!background_start_refused(
            &uids, APP_UID, 36, false, false, false, 0
        ));
        let at = uids.on_task_removed(APP_UID, 5_000).expect("sweep armed");
        assert_eq!(at, 5_000 + BACKGROUND_SETTLE_TIME_MS);
        assert!(!background_start_refused(
            &uids,
            APP_UID,
            36,
            false,
            false,
            false,
            at - 1
        ));
        assert_eq!(uids.settle(APP_UID, at), Settle::BecameIdle);
        assert!(background_start_refused(
            &uids, APP_UID, 36, false, false, false, at
        ));
    }

    /// top_since marks the first Task only; restricted_since starts when the last Task and FGS are gone (not on Task -> FGS) and clears when either returns.
    #[test]
    fn top_and_restricted_times() {
        let mut uids = Uids::default();
        assert_eq!(uids.top_since(APP_UID), None);
        uids.on_task_added(APP_UID, 100);
        uids.on_task_added(APP_UID, 200);
        assert_eq!(uids.top_since(APP_UID), Some(100));
        uids.on_fgs_started(APP_UID);
        uids.on_task_removed(APP_UID, 300);
        uids.on_task_removed(APP_UID, 400);
        assert_eq!(uids.restricted_since(APP_UID), None);
        uids.on_fgs_stopped(APP_UID, 500);
        assert_eq!(uids.restricted_since(APP_UID), Some(500));
        uids.on_task_added(APP_UID, 600);
        assert_eq!(uids.restricted_since(APP_UID), None);
        assert_eq!(uids.top_since(APP_UID), Some(600));
    }

    /// Pins that `on_uid_gone` clears `restricted_since` while keeping `top_since` and the AMS idle state (AppOpsUidStateTrackerImpl.updateUidProcState :242-243, commitUidPendingState :402-405); break: omitting the clear in `on_uid_gone`.
    #[test]
    fn uid_gone_clears_restricted_since() {
        let mut uids = Uids::default();
        uids.on_task_added(APP_UID, 100);
        assert_eq!(uids.on_task_removed(APP_UID, 500), Some(60_500));
        assert_eq!(uids.restricted_since(APP_UID), Some(500));
        uids.on_uid_gone(APP_UID);
        assert_eq!(uids.restricted_since(APP_UID), None);
        assert_eq!(uids.top_since(APP_UID), Some(100));
        assert!(!uids.is_idle(APP_UID));
        uids.on_uid_gone(APP_UID + 1);
        assert_eq!(uids.restricted_since(APP_UID + 1), None);
    }
}
