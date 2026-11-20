// SPDX-License-Identifier: MIT
//! The `power` service (Context.POWER_SERVICE) and the `thermalservice` binder it needs: SystemServiceRegistry builds PowerManager only when both `getServiceOrThrow` lookups succeed,
//! else getSystemService returns null (Angry Birds 2's vungle AndroidPlatform.<init> casts it unchecked and dies).
//! Behaviour = PowerManagerService.BinderService for a desktop that is awake with battery saver off: interactive, no power save, no device idle. The power source is battery.rs's (the health HAL's),
//! the one source; no method here answers a plugged state today (PowerManagerService reads BatteryManagerInternal.getPlugType for wake-on-plug and stay-on, neither app-observable through this binder).
//! Wake locks: acquire/release checked and recorded per token as acquireWakeLockInternal does (re-acquire of a held token updates it), counted in the log; they hold nothing on the host yet
//! (a native program would inhibit idle through the desktop; not built). Not modelled: the token's death (AOSP's linkToDeath releases it), display ids, callbacks, work sources (non-null refused by atl-parcelables).
//! IThermalService: ThermalManagerService on a device with no thermal HAL (ThermalService below).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::os::IPowerManager::{BnPowerManager, IPowerManager};
use atl_aidl::aidl::android::os::IThermalEventListener::IThermalEventListener;
use atl_aidl::aidl::android::os::IThermalHeadroomListener::IThermalHeadroomListener;
use atl_aidl::aidl::android::os::IThermalService::{BnThermalService, IThermalService};
use atl_aidl::aidl::android::os::IThermalStatusListener::IThermalStatusListener;
use atl_aidl::aidl::android::os::IWakeLockCallback::IWakeLockCallback;
use atl_parcelables::android::os::{CoolingDevice, Temperature, WorkSource};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, FromIBinder, IBinder, Interface, SpIBinder,
    Status, Strong, ThreadState,
};

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

/// Temperature.THROTTLING_NONE = PowerManager.THERMAL_STATUS_NONE: ThermalManagerService's mStatus at construction, never changed without a HAL.
const THROTTLING_NONE: i32 = 0;

/// One RemoteCallbackList entry: the listener, its death flag and link (RemoteCallbackList.Callback.binderDied removes it). An event listener's type cookie is not kept: no event is ever sent.
struct Callback<T: ?Sized + FromIBinder> {
    listener: Strong<T>,
    dead: Arc<AtomicBool>,
    _death: DeathRecipient,
}

/// RemoteCallbackList for one listener interface: keyed by binder, an entry whose binder died is dropped at the next access.
struct CallbackList<T: ?Sized + FromIBinder> {
    entries: Mutex<Vec<Callback<T>>>,
}

impl<T: ?Sized + FromIBinder + Interface> CallbackList<T> {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// RemoteCallbackList.register: replaces an entry of the same binder; false when the binder is already dead (linkToDeath's RemoteException).
    fn register(&self, listener: &Strong<T>) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|c| !c.dead.load(Ordering::Acquire));
        let binder = listener.as_binder();
        entries.retain(|c| c.listener.as_binder() != binder);
        let dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dead);
        let mut death = DeathRecipient::new(move || flag.store(true, Ordering::Release));
        if listener.as_binder().link_to_death(&mut death).is_err() {
            return false;
        }
        entries.push(Callback {
            listener: listener.clone(),
            dead,
            _death: death,
        });
        true
    }

    /// RemoteCallbackList.unregister: true when the binder was registered (and alive).
    fn unregister(&self, listener: &Strong<T>) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|c| !c.dead.load(Ordering::Acquire));
        let binder = listener.as_binder();
        let before = entries.len();
        entries.retain(|c| c.listener.as_binder() != binder);
        entries.len() != before
    }
}

/// ThermalManagerService on a device with no thermal HAL: onActivityManagerReady finds none of the AIDL / 2.0 / 1.1 / 1.0 HALs, logs "No Thermal HAL service on this device" and leaves mHalReady false,
/// so mStatus stays THROTTLING_NONE, the temperature map stays empty, and every HAL-backed answer is the not-ready one. A host thermal-zone tier is not built: Android's severities come from a
/// vendor HAL's per-device thresholds and /sys/class/thermal trip points carry no agreed mapping to them.
struct ThermalService {
    permissions: Arc<Permissions>,
    /// mThermalEventListeners: registered, never notified (no temperature is ever reported).
    event_listeners: CallbackList<dyn IThermalEventListener>,
    status_listeners: CallbackList<dyn IThermalStatusListener>,
}

impl ThermalService {
    /// enforceCallingOrSelfPermission(DEVICE_POWER): signature-level, so an app gets SecurityException.
    fn enforce_device_power(&self, method: &str) -> binder::Result<()> {
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), &[DEVICE_POWER], false)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {DEVICE_POWER}")),
        ))
    }
}

fn null_listener() -> Status {
    Status::new_exception_str(ExceptionCode::NULL_POINTER, Some("listener is null"))
}

impl Interface for ThermalService {}

impl IThermalService for ThermalService {
    /// registerThermalEventListener: DEVICE_POWER, register; postEventListenerCurrentTemperaturesLocked posts nothing for an empty temperature map.
    fn r#registerThermalEventListener(
        &self,
        listener: Option<&Strong<dyn IThermalEventListener>>,
    ) -> binder::Result<bool> {
        self.enforce_device_power("registerThermalEventListener")?;
        let listener = listener.ok_or_else(null_listener)?;
        Ok(self.event_listeners.register(listener))
    }

    fn r#registerThermalEventListenerWithType(
        &self,
        listener: Option<&Strong<dyn IThermalEventListener>>,
        _type: i32,
    ) -> binder::Result<bool> {
        self.enforce_device_power("registerThermalEventListenerWithType")?;
        let listener = listener.ok_or_else(null_listener)?;
        Ok(self.event_listeners.register(listener))
    }

    fn r#unregisterThermalEventListener(
        &self,
        listener: Option<&Strong<dyn IThermalEventListener>>,
    ) -> binder::Result<bool> {
        self.enforce_device_power("unregisterThermalEventListener")?;
        let listener = listener.ok_or_else(null_listener)?;
        Ok(self.event_listeners.unregister(listener))
    }

    /// HAL not ready -> new Temperature[0].
    fn r#getCurrentTemperatures(&self) -> binder::Result<Option<Vec<Option<Temperature>>>> {
        self.enforce_device_power("getCurrentTemperatures")?;
        Ok(Some(Vec::new()))
    }

    fn r#getCurrentTemperaturesWithType(
        &self,
        _type: i32,
    ) -> binder::Result<Option<Vec<Option<Temperature>>>> {
        self.enforce_device_power("getCurrentTemperaturesWithType")?;
        Ok(Some(Vec::new()))
    }

    /// register, then postStatusListenerLocked: onStatusChange(mStatus) once (oneway; AOSP posts it on FgThread, here sent before the reply -- the app's executor runs it either way).
    fn r#registerThermalStatusListener(
        &self,
        listener: Option<&Strong<dyn IThermalStatusListener>>,
    ) -> binder::Result<bool> {
        let listener = listener.ok_or_else(null_listener)?;
        if !self.status_listeners.register(listener) {
            return Ok(false);
        }
        if let Err(e) = listener.r#onStatusChange(THROTTLING_NONE) {
            // AOSP: Slog.e "Thermal status callback failed to call", registration kept
            atl_diag::answered(
                "IThermalService",
                &format!("onStatusChange failed: {e}"),
            );
        }
        Ok(true)
    }

    fn r#unregisterThermalStatusListener(
        &self,
        listener: Option<&Strong<dyn IThermalStatusListener>>,
    ) -> binder::Result<bool> {
        let listener = listener.ok_or_else(null_listener)?;
        Ok(self.status_listeners.unregister(listener))
    }

    fn r#getCurrentThermalStatus(&self) -> binder::Result<i32> {
        Ok(THROTTLING_NONE)
    }

    /// HAL not ready -> new CoolingDevice[0].
    fn r#getCurrentCoolingDevices(&self) -> binder::Result<Option<Vec<Option<CoolingDevice>>>> {
        self.enforce_device_power("getCurrentCoolingDevices")?;
        Ok(Some(Vec::new()))
    }

    fn r#getCurrentCoolingDevicesWithType(
        &self,
        _type: i32,
    ) -> binder::Result<Option<Vec<Option<CoolingDevice>>>> {
        self.enforce_device_power("getCurrentCoolingDevicesWithType")?;
        Ok(Some(Vec::new()))
    }

    /// HAL not ready -> NaN, before the forecastSeconds range check.
    fn r#getThermalHeadroom(&self, _forecast_seconds: i32) -> binder::Result<f32> {
        Ok(f32::NAN)
    }

    /// HAL not ready -> IllegalStateException, before the feature-flag check.
    fn r#getThermalHeadroomThresholds(&self) -> binder::Result<Option<Vec<f32>>> {
        Err(Status::new_exception_str(
            ExceptionCode::ILLEGAL_STATE,
            Some("Thermal HAL connection is not initialized"),
        ))
    }

    /// HAL not ready -> false, nothing registered (PowerManager then throws "Thermal headroom listener failed to set").
    fn r#registerThermalHeadroomListener(
        &self,
        _listener: Option<&Strong<dyn IThermalHeadroomListener>>,
    ) -> binder::Result<bool> {
        Ok(false)
    }

    /// mThermalHeadroomListeners.unregister of a listener that could never register -> false.
    fn r#unregisterThermalHeadroomListener(
        &self,
        _listener: Option<&Strong<dyn IThermalHeadroomListener>>,
    ) -> binder::Result<bool> {
        Ok(false)
    }
}

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
pub fn new_thermal_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnThermalService::new_binder(
        ThermalService {
            permissions,
            event_listeners: CallbackList::new(),
            status_listeners: CallbackList::new(),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
