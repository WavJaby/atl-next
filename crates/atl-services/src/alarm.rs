// SPDX-License-Identifier: MIT
//! The `alarm` service (Context.ALARM_SERVICE): IAlarmManager. Set alarms are held in a table (alarm_table.rs) and delivered when due (alarm_timer.rs): an operation is sent through its IIntentSender (pending_intent.rs), a listener gets doAlarm with a completion callback, which AlarmManager's ListenerWrapper reports completion through (AlarmManager.java:371-386).
//! One thread waits on the timers, takes the due alarms under the table lock and delivers them outside it. Every change to the table re-arms the timers under that same lock, so the timers always hold the table's earliest triggers.
//! A _WAKEUP type delivers like its non-wakeup type: a desktop program does not wake the machine from suspend. An inexact alarm fires at its trigger (the window start); nothing is batched.
//! Not modelled: doze, battery saver, app standby, quota, the exact-alarm grant (an exact alarm without it throws SecurityException, AlarmManagerService.java:2794-2808), app-start gating, EXTRA_ALARM_COUNT in the delivered intent (:5359).
//! A delivery that fails (a listener or an operation whose process is gone) is counted and dropped; no process is started for it.
//! WorkSource and AlarmManager.AlarmClockInfo have no codec: a non-null one is refused and counted by atl-parcelables, so no alarm clock is ever held and getNextAlarmClock answers null (AOSP would answer the user's next clock, :3703).
//! Counted UNKNOWN (the stub default): canScheduleExactAlarms, hasScheduleExactAlarm, getNextWakeFromIdleTime, and setTime / setTimeZone / getConfigVersion after their permission check.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::app::IAlarmCompleteListener::{
    BnAlarmCompleteListener, IAlarmCompleteListener,
};
use atl_aidl::aidl::android::app::IAlarmManager::{
    BnAlarmManager, EnforcePermission, IAlarmManager,
};
use atl_aidl::mangled::_7_android_3_app_14_IAlarmListener as IAlarmListener;
use atl_parcelables::android::app::AlarmManager_AlarmClockInfo as AlarmClockInfo;
use atl_parcelables::android::app::PendingIntent;
use atl_parcelables::android::os::{Bundle, WorkSource};
use atl_parcelables::codecs::android::os::Bundle::Value;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::activity::log;
use crate::alarm_table::{
    Alarm, AlarmTable, Clock, ELAPSED_REALTIME, FLAG_ALLOW_WHILE_IDLE, FLAG_PRIORITIZE,
    IdleOptions, RTC_WAKEUP, idle_options,
};
use crate::alarm_timer::{self, Timers};
use crate::packages::Packages;
use crate::packages::Permissions;
use crate::pending_intent;
use crate::uid_state;
use atl_services_types::permission;

/// The interface name counters use.
const INTERFACE: &str = "IAlarmManager";

/// AlarmManagerService.Constants: DEFAULT_MIN_INTERVAL = 60 * 1000, DEFAULT_MAX_INTERVAL = 365 * INTERVAL_DAY (:726-727).
const INTERVAL_DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MIN_INTERVAL_MS: i64 = 60 * 1000;
const MAX_INTERVAL_MS: i64 = 365 * INTERVAL_DAY_MS;

/// Keys are binders: a PendingIntent by its IIntentSender's, a listener by its own (PendingIntent.equals and the listener compare these).
type Table = AlarmTable<SpIBinder, SpIBinder>;

struct Inner {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    table: Mutex<Table>,
    timers: Timers,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The caller is `package`'s uid, or system / root (notification.rs's rule). AlarmManagerService compares the package's uid (:2694, :2918).
    fn owns(&self, uid: u32, package: &str) -> bool {
        permission::is_system_or_root(uid) || self.packages.is_same_app(package, uid)
    }

    /// Arms the timers to the table's earliest trigger on each clock. Called with the table lock held, so the arms follow the table's order.
    fn rearm(&self, table: &Table) {
        if let Err(e) = self.timers.arm(
            table.next_due(Clock::Boottime),
            table.next_due(Clock::Realtime),
        ) {
            atl_diag::refused(INTERFACE, "timer arm failed");
            log(format_args!("alarm: timer arm: {e}"));
        }
    }
}

struct AlarmService(Arc<Inner>);

impl Interface for AlarmService {}

/// A call that names no package is malformed: AlarmManager always passes its op package. Refused and counted.
fn named<'a>(package: Option<&'a str>, method: &str) -> binder::Result<&'a str> {
    package.ok_or_else(|| {
        atl_diag::refused(INTERFACE, &format!("{method} (no package)"));
        Status::new_exception_str(
            ExceptionCode::ILLEGAL_ARGUMENT,
            Some(format!("{method}: no package")),
        )
    })
}

fn not_owner(package: &str, uid: u32) -> Status {
    Status::new_exception_str(
        ExceptionCode::SECURITY,
        Some(format!(
            "Package {package} does not belong to the calling uid {uid}"
        )),
    )
}

/// setImpl's interval check (AlarmManagerService.java:2173-2186): a positive interval under the minimum is raised to it, one over the maximum is lowered.
fn clamp_interval(interval: i64) -> i64 {
    if interval > 0 && interval < MIN_INTERVAL_MS {
        MIN_INTERVAL_MS
    } else if interval > MAX_INTERVAL_MS {
        MAX_INTERVAL_MS
    } else {
        interval
    }
}

impl IAlarmManager for AlarmService {
    /// @EnforcePermission (setTime, setTimeZone, getConfigVersion): root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
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
            .0
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

    /// set (:2684): the caller must be the package's uid, and a repeating alarm takes no listener (:2701). Neither or both of operation and listener is dropped (setImpl:2156). Stored under its key, replacing the alarm that key held (setImplLocked:2271), and the timers re-armed.
    #[allow(clippy::too_many_arguments)]
    fn r#set(
        &self,
        calling_package: Option<&str>,
        kind: i32,
        trigger_at_time: i64,
        window_length: i64,
        interval: i64,
        flags: i32,
        operation: Option<&PendingIntent>,
        listener: Option<&Strong<dyn IAlarmListener>>,
        _listener_tag: Option<&str>,
        _work_source: Option<&WorkSource>,
        _alarm_clock: Option<&AlarmClockInfo>,
    ) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        let package = named(calling_package, "set")?;
        if !self.0.owns(uid, package) {
            return Err(not_owner(package, uid));
        }
        if interval != 0 && listener.is_some() {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("Repeating alarms cannot use AlarmReceivers".to_owned()),
            ));
        }
        if operation.is_some() == listener.is_some() {
            atl_diag::refused(
                INTERFACE,
                "set (operation and listener both or neither: dropped)",
            );
            return Ok(());
        }
        if !(RTC_WAKEUP..=ELAPSED_REALTIME).contains(&kind) {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("Invalid alarm type {kind}")),
            ));
        }
        // setImpl's idleOptions (AlarmManagerService :2735-2820): a core caller's allow-while-idle and prioritize are cleared (:2735-2748, UserHandle.isCore);
        // a package the services do not know reads as the current targetSdk
        let core = permission::sees_all_packages(uid);
        let exact_change = self
            .0
            .packages
            .by_name(package)
            .is_none_or(|p| p.app().target_sdk_version >= uid_state::SDK_S);
        let idle = operation
            .is_some()
            .then(|| {
                idle_options(
                    window_length == 0,
                    flags & FLAG_ALLOW_WHILE_IDLE != 0 && !core,
                    flags & FLAG_PRIORITIZE != 0 && !core,
                    exact_change,
                )
            })
            .flatten();
        let mut table = self.0.lock();
        table.set(Alarm {
            uid,
            package: package.to_owned(),
            kind,
            trigger: trigger_at_time.max(0),
            window: window_length,
            interval: clamp_interval(interval),
            flags,
            idle,
            operation: operation.map(|p| p.target.clone()),
            listener: listener.map(|l| l.as_binder()),
        });
        self.0.rearm(&table);
        atl_diag::answered(INTERFACE, "set (delivered when due)");
        Ok(())
    }

    /// remove (:2900): by operation, else by listener; neither is nothing. Counted whether or not it dropped an alarm.
    fn r#remove(
        &self,
        operation: Option<&PendingIntent>,
        listener: Option<&Strong<dyn IAlarmListener>>,
    ) -> binder::Result<()> {
        let operation = operation.map(|p| p.target.clone());
        let listener = listener.map(|l| l.as_binder());
        let mut table = self.0.lock();
        table.remove(operation.as_ref(), listener.as_ref());
        self.0.rearm(&table);
        atl_diag::answered(INTERFACE, "remove");
        Ok(())
    }

    /// removeAll (:2911): the caller's own package only. The system uid answers nothing (AOSP wtfs and returns, :2913-2916).
    fn r#removeAll(&self, calling_package: Option<&str>) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        let package = named(calling_package, "removeAll")?;
        if uid == permission::SYSTEM_UID {
            return Ok(());
        }
        if !self.0.owns(uid, package) {
            return Err(not_owner(package, uid));
        }
        let mut table = self.0.lock();
        table.remove_all(uid, package);
        self.0.rearm(&table);
        atl_diag::answered(INTERFACE, "removeAll");
        Ok(())
    }

    /// getNextAlarmClock (:2936): null. No alarm clock is ever held (module doc), and AOSP answers null for a user with none (getNextAlarmClockImpl :3703).
    fn r#getNextAlarmClock(&self, _user_id: i32) -> binder::Result<Option<AlarmClockInfo>> {
        atl_diag::answered(INTERFACE, "getNextAlarmClock (null)");
        Ok(None)
    }
}

/// The timer thread (module doc): the due alarms are taken and the timers re-armed under the table lock, then the due ones are delivered with no lock held.
fn run(inner: Arc<Inner>) {
    loop {
        if let Err(e) = inner.timers.wait() {
            atl_diag::refused(INTERFACE, "timer wait failed: alarms stop");
            log(format_args!("alarm: timer wait: {e}"));
            return;
        }
        let due = {
            let mut table = inner.lock();
            let due = table.take_due(alarm_timer::now());
            inner.rearm(&table);
            due
        };
        for alarm in &due {
            if let Some(operation) = &alarm.operation {
                send(operation, alarm.idle);
            } else if let Some(listener) = &alarm.listener {
                do_alarm(listener);
            }
        }
    }
}

/// PendingIntent.send with no intent (AlarmManagerService's mPendingIntent.send, with getAlarmOperationBundle's options): the record's request intent through IIntentSender.send,
/// in-process as the services (pending_intent.rs send_in_process).
fn send(operation: &SpIBinder, idle: Option<IdleOptions>) {
    let result = pending_intent::send_in_process(operation, &operation_options(idle));
    match result {
        Ok(()) => {
            atl_diag::sent(INTERFACE, "alarm operation");
        }
        Err(e) => {
            atl_diag::refused(INTERFACE, "alarm operation dropped");
            log(format_args!("alarm: operation: {e}"));
        }
    }
}

/// The Bundle an alarm's send carries (AlarmManagerService.getAlarmOperationBundle): the alarm flag on every send (makeBasicAlarmBroadcastOptions :353), and the temp allowlist
/// setImpl gave it (pending_intent.rs reads both back).
fn operation_options(idle: Option<IdleOptions>) -> Bundle {
    let mut entries = vec![(
        pending_intent::KEY_FLAGS.to_owned(),
        Value::Int(pending_intent::FLAG_IS_ALARM_BROADCAST),
    )];
    if let Some(idle) = idle {
        entries.push((
            pending_intent::KEY_TEMP_DURATION.to_owned(),
            Value::Long(idle.duration_ms),
        ));
        entries.push((
            pending_intent::KEY_TEMP_TYPE.to_owned(),
            Value::Int(idle.kind),
        ));
        entries.push((
            pending_intent::KEY_TEMP_REASON_CODE.to_owned(),
            Value::Int(idle.reason),
        ));
        // mOptsWithFgs / mOptsWithoutFgs carry the empty reason string (AlarmManagerService :881-889)
        entries.push((
            pending_intent::KEY_TEMP_REASON.to_owned(),
            Value::String(Some(String::new())),
        ));
    }
    Bundle::from_entries(entries)
}

/// OnAlarmListener's delivery (IAlarmListener.doAlarm, oneway): the app's onAlarm runs on its executor, then alarmComplete comes back to Completion.
fn do_alarm(listener: &SpIBinder) {
    let result = match listener.clone().into_interface::<dyn IAlarmListener>() {
        Ok(listener) => {
            let completion =
                BnAlarmCompleteListener::new_binder(Completion, BinderFeatures::default());
            listener.r#doAlarm(Some(&completion))
        }
        Err(e) => Err(e.into()),
    };
    match result {
        Ok(()) => {
            atl_diag::sent(INTERFACE, "doAlarm");
        }
        Err(e) => {
            atl_diag::refused(INTERFACE, "alarm listener dropped");
            log(format_args!("alarm: listener: {e}"));
        }
    }
}

/// The completion callback each doAlarm is handed. Nothing waits on it: no wake lock is held (module doc), so it only counts.
struct Completion;

impl Interface for Completion {}

impl IAlarmCompleteListener for Completion {
    fn r#alarmComplete(&self, _who: Option<&SpIBinder>) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "alarmComplete");
        Ok(())
    }
}

/// The service's binder, for `addService`: the packages decide who owns a package name, the permissions the @EnforcePermission methods. Starts the timer thread (one per service).
pub fn new_binder(packages: Arc<Packages>, permissions: Arc<Permissions>) -> SpIBinder {
    let timers = Timers::new().expect("alarm timerfd");
    let inner = Arc::new(Inner {
        packages,
        permissions,
        table: Mutex::new(AlarmTable::default()),
        timers,
    });
    let timer_inner = Arc::clone(&inner);
    std::thread::Builder::new()
        .name("alarm-timer".to_owned())
        .spawn(move || run(timer_inner))
        .expect("alarm timer thread");
    BnAlarmManager::new_binder(AlarmService(inner), BinderFeatures::default()).as_binder()
}
