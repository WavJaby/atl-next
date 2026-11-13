// SPDX-License-Identifier: MIT
//! The `power` service (Context.POWER_SERVICE) and the `thermalservice` binder it needs: SystemServiceRegistry builds PowerManager only when both `getServiceOrThrow` lookups succeed,
//! else getSystemService returns null (Angry Birds 2's vungle AndroidPlatform.<init> casts it unchecked and dies).
//! Behaviour = PowerManagerService.BinderService for a desktop that is awake, on mains, with battery saver off: interactive, no power save, no device idle.
//! Wake locks: acquire/release checked and recorded per token as acquireWakeLockInternal does (re-acquire of a held token updates it), counted in the log; they hold nothing on the host yet
//! (a native program would inhibit idle through the desktop; not built). Not modelled: the token's death (AOSP's linkToDeath releases it), display ids, callbacks, work sources (non-null refused by atl-parcelables).
//! IThermalService: registered bare, every method UNKNOWN_TRANSACTION (counted) -- PowerManager only stores it; its thermal methods reach it lazily.

use std::io::Write;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::os::IPowerManager::{BnPowerManager, IPowerManager};
use atl_aidl::aidl::android::os::IThermalService::{BnThermalService, IThermalService};
use atl_aidl::aidl::android::os::IWakeLockCallback::IWakeLockCallback;
use atl_parcelables::android::os::WorkSource;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::packages::Permissions;

/// Manifest.permission.WAKE_LOCK: every acquire / release (BinderService.acquireWakeLock, releaseWakeLock).
const WAKE_LOCK: &str = "android.permission.WAKE_LOCK";
/// Manifest.permission.DEVICE_POWER: DOZE_WAKE_LOCK and SYSTEM_WAKELOCK acquires.
const DEVICE_POWER: &str = "android.permission.DEVICE_POWER";
/// PowerManager level / flag constants (PowerManager.java).
const WAKE_LOCK_LEVEL_MASK: i32 = 0x0000_ffff;
const PARTIAL_WAKE_LOCK: i32 = 0x1;
const SCREEN_DIM_WAKE_LOCK: i32 = 0x6;
const SCREEN_BRIGHT_WAKE_LOCK: i32 = 0xa;
const FULL_WAKE_LOCK: i32 = 0x1a;
const PROXIMITY_SCREEN_OFF_WAKE_LOCK: i32 = 0x20;
const DOZE_WAKE_LOCK: i32 = 0x40;
const DRAW_WAKE_LOCK: i32 = 0x80;
const SCREEN_TIMEOUT_OVERRIDE_WAKE_LOCK: i32 = 0x100;
const SYSTEM_WAKELOCK: i32 = 0x8000_0000_u32 as i32;

/// One held wake lock (PowerManagerService.WakeLock, the fields this service keeps).
struct WakeLock {
    token: SpIBinder,
    flags: i32,
    tag: String,
    uid: u32,
}

struct PowerService {
    permissions: Arc<Permissions>,
    /// PowerManagerService.mWakeLocks.
    wake_locks: Mutex<Vec<WakeLock>>,
}

impl PowerService {
    fn enforce(&self, method: &str, permission: &str) -> binder::Result<()> {
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), &[permission], false)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission}")),
        ))
    }
}

fn illegal(msg: String) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(msg))
}

/// PowerManager.validateWakeLockParameters: a known level (the tag's null check is the caller's).
fn valid_level(flags: i32) -> bool {
    matches!(
        flags & WAKE_LOCK_LEVEL_MASK,
        PARTIAL_WAKE_LOCK
            | SCREEN_DIM_WAKE_LOCK
            | SCREEN_BRIGHT_WAKE_LOCK
            | FULL_WAKE_LOCK
            | PROXIMITY_SCREEN_OFF_WAKE_LOCK
            | DOZE_WAKE_LOCK
            | DRAW_WAKE_LOCK
            | SCREEN_TIMEOUT_OVERRIDE_WAKE_LOCK
    )
}

/// isWakeLockLevelSupportedInternal: no proximity sensor, early screen-timeout detector off.
fn level_supported(level: i32) -> bool {
    matches!(
        level,
        PARTIAL_WAKE_LOCK
            | SCREEN_DIM_WAKE_LOCK
            | SCREEN_BRIGHT_WAKE_LOCK
            | FULL_WAKE_LOCK
            | DOZE_WAKE_LOCK
            | DRAW_WAKE_LOCK
    )
}

impl Interface for PowerService {}

impl IPowerManager for PowerService {
    /// BinderService.acquireWakeLock -> acquireWakeLockInternal. SCREEN_TIMEOUT_OVERRIDE is refused as with the early-screen-timeout flag off.
    fn r#acquireWakeLock(
        &self,
        lock: Option<&SpIBinder>,
        flags: i32,
        tag: Option<&str>,
        package_name: Option<&str>,
        _ws: Option<&WorkSource>,
        _history_tag: Option<&str>,
        _display_id: i32,
        _callback: Option<&Strong<dyn IWakeLockCallback>>,
    ) -> binder::Result<()> {
        let lock = lock.ok_or_else(|| illegal("lock must not be null".to_owned()))?;
        if package_name.is_none() {
            return Err(illegal("packageName must not be null".to_owned()));
        }
        if !valid_level(flags) {
            return Err(illegal("Must specify a valid wake lock level.".to_owned()));
        }
        let tag = tag.ok_or_else(|| illegal("The tag must not be null.".to_owned()))?;
        self.enforce("acquireWakeLock", WAKE_LOCK)?;
        if flags & (DOZE_WAKE_LOCK | SYSTEM_WAKELOCK) != 0 {
            self.enforce("acquireWakeLock", DEVICE_POWER)?;
        }
        if flags & SCREEN_TIMEOUT_OVERRIDE_WAKE_LOCK != 0 {
            return Err(illegal(format!(
                "Acquiring an unsupported wake lock: flags={flags}, tag={tag}"
            )));
        }
        let uid = ThreadState::get_calling_uid();
        let mut locks = self.wake_locks.lock().unwrap_or_else(|e| e.into_inner());
        match locks.iter_mut().find(|w| &w.token == lock) {
            Some(w) => {
                w.flags = flags;
                w.tag = tag.to_owned();
                w.uid = uid;
            }
            None => locks.push(WakeLock {
                token: lock.clone(),
                flags,
                tag: tag.to_owned(),
                uid,
            }),
        }
        let _ = writeln!(
            std::io::stderr(),
            "power: acquire uid={uid} flags={flags:#x} tag={tag} held={}",
            locks.len()
        );
        Ok(())
    }

    /// BinderService.releaseWakeLock -> releaseWakeLockInternal: an unknown token is ignored.
    fn r#releaseWakeLock(&self, lock: Option<&SpIBinder>, _flags: i32) -> binder::Result<()> {
        let lock = lock.ok_or_else(|| illegal("lock must not be null".to_owned()))?;
        self.enforce("releaseWakeLock", WAKE_LOCK)?;
        let mut locks = self.wake_locks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = locks.iter().position(|w| &w.token == lock) {
            let w = locks.remove(i);
            let _ = writeln!(
                std::io::stderr(),
                "power: release uid={} flags={:#x} tag={} held={}",
                w.uid,
                w.flags,
                w.tag,
                locks.len()
            );
        }
        Ok(())
    }

    fn r#isWakeLockLevelSupported(&self, level: i32) -> binder::Result<bool> {
        Ok(level_supported(level))
    }

    /// The display id only matters for the proximity level, unsupported on every display here.
    fn r#isWakeLockLevelSupportedWithDisplayId(
        &self,
        level: i32,
        _display_id: i32,
    ) -> binder::Result<bool> {
        Ok(level_supported(level))
    }

    /// The desktop session is awake while the app runs.
    fn r#isInteractive(&self) -> binder::Result<bool> {
        Ok(true)
    }

    fn r#isDisplayInteractive(&self, _display_id: i32) -> binder::Result<bool> {
        Ok(true)
    }

    /// Battery saver is not enabled (it never turns on: no battery saver state machine).
    fn r#isPowerSaveMode(&self) -> binder::Result<bool> {
        Ok(false)
    }

    fn r#isDeviceIdleMode(&self) -> binder::Result<bool> {
        Ok(false)
    }

    fn r#isLightDeviceIdleMode(&self) -> binder::Result<bool> {
        Ok(false)
    }
}

struct ThermalService;

impl Interface for ThermalService {}

impl IThermalService for ThermalService {}

/// The `power` binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnPowerManager::new_binder(
        PowerService {
            permissions,
            wake_locks: Mutex::new(Vec::new()),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

/// The `thermalservice` binder, for `addService`.
pub fn new_thermal_binder() -> SpIBinder {
    BnThermalService::new_binder(ThermalService, BinderFeatures::default()).as_binder()
}
