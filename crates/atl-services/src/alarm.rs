// SPDX-License-Identifier: MIT
//! The `alarm` service (Context.ALARM_SERVICE): IAlarmManager-lite. Set alarms are kept in a table (alarm_table.rs) and dropped by cancel, as AlarmManagerService keeps them (set :2684, remove :2900, removeAll :2911).
//! Without this service AlarmManager's constructor gets a null proxy and every call NPEs (SystemServiceRegistry.java:380-388). Nothing is delivered yet: each accepted set is counted as `set (stored, never fires)`, so the gap shows in the log.
//! Not modelled: delivery (doAlarm, wake, quota, idle, batching); the exact-alarm grant (an exact alarm without it throws SecurityException, AlarmManagerService.java:2794-2808); app-start gating.
//! WorkSource and AlarmManager.AlarmClockInfo have no codec: a non-null one is refused and counted by atl-parcelables, so no alarm clock is ever held and getNextAlarmClock answers null (AOSP would answer the user's next clock, :3703).
//! Counted UNKNOWN (the stub default): canScheduleExactAlarms, hasScheduleExactAlarm, getNextWakeFromIdleTime, and setTime / setTimeZone / getConfigVersion after their permission check.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::app::IAlarmManager::{
    BnAlarmManager, EnforcePermission, IAlarmManager,
};
use atl_aidl::mangled::_7_android_3_app_14_IAlarmListener as IAlarmListener;
use atl_parcelables::android::app::AlarmManager_AlarmClockInfo as AlarmClockInfo;
use atl_parcelables::android::app::PendingIntent;
use atl_parcelables::android::os::WorkSource;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::alarm_table::{Alarm, AlarmTable};
use crate::counters;
use crate::packages::Packages;
use crate::permission::{self, Permissions};

/// The interface name counters use.
const INTERFACE: &str = "IAlarmManager";

/// Keys are binders: a PendingIntent by its IIntentSender's, a listener by its own (PendingIntent.equals and the listener compare these).
type Table = AlarmTable<SpIBinder, SpIBinder>;

struct Inner {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    table: Mutex<Table>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The caller is `package`'s uid, or system / root (notification.rs's rule). AlarmManagerService compares the package's uid (:2694, :2918).
    fn owns(&self, uid: u32, package: &str) -> bool {
        permission::is_system_or_root(uid) || self.packages.is_same_app(package, uid)
    }
}

struct AlarmService(Arc<Inner>);

impl Interface for AlarmService {}

/// A call that names no package is malformed: AlarmManager always passes its op package. Refused and counted.
fn named<'a>(package: Option<&'a str>, method: &str) -> binder::Result<&'a str> {
    package.ok_or_else(|| {
        counters::refused(INTERFACE, &format!("{method} (no package)"));
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

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// set (:2684): the caller must be the package's uid, and a repeating alarm takes no listener (:2701). Stored under its key, replacing the alarm that key held (setImplLocked:2271).
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
        self.0.lock().set(Alarm {
            uid,
            package: package.to_owned(),
            kind,
            trigger: trigger_at_time,
            window: window_length,
            interval,
            flags,
            operation: operation.map(|p| p.target.clone()),
            listener: listener.map(|l| l.as_binder()),
        });
        counters::answered(INTERFACE, "set (stored, never fires)");
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
        self.0.lock().remove(operation.as_ref(), listener.as_ref());
        counters::answered(INTERFACE, "remove");
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
        self.0.lock().remove_all(uid, package);
        counters::answered(INTERFACE, "removeAll");
        Ok(())
    }

    /// getNextAlarmClock (:2936): null. No alarm clock is ever held (module doc), and AOSP answers null for a user with none (getNextAlarmClockImpl :3703).
    fn r#getNextAlarmClock(&self, _user_id: i32) -> binder::Result<Option<AlarmClockInfo>> {
        counters::answered(INTERFACE, "getNextAlarmClock (null)");
        Ok(None)
    }
}

/// The service's binder, for `addService`: the packages decide who owns a package name, the permissions the @EnforcePermission methods.
pub fn new_binder(packages: Arc<Packages>, permissions: Arc<Permissions>) -> SpIBinder {
    BnAlarmManager::new_binder(
        AlarmService(Arc::new(Inner {
            packages,
            permissions,
            table: Mutex::new(AlarmTable::default()),
        })),
        BinderFeatures::default(),
    )
    .as_binder()
}
