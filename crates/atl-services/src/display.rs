// SPDX-License-Identifier: MIT
//! The `display` service (Context.DISPLAY_SERVICE): the converter's IDisplayManager stub answering for display 0 from the display the Tasks own (tasks.rs `Tasks::display`: atld's `--display` at start, then the window's monitor via registry SET_DISPLAY); registered callbacks hear each change ([`DisplayCallbacks`]).
//! getDisplayInfo is display 0's record (DisplayManagerService.getDisplayInfo: null for any other id) and getDisplayIds lists display 0. Every other method keeps the stub's default body,
//! so each call is counted (counters.rs) and answered UNKNOWN_TRANSACTION. An activity launch made these calls before this service answered (measured by `xtask test app`):
//! `getDisplayInfo` 7 times, `getPreferredWideGamutColorSpaceId` and `getOverlaySupport` once each.

use atl_aidl::aidl::android::hardware::display::IDisplayManager::{
    BnDisplayManager, EnforcePermission, IDisplayManager,
};
use atl_parcelables::android::view::DisplayInfo;
use atl_parcelables::parts::DisplayMode;
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status,
    StatusCode, ThreadState,
};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::hardware::display::IDisplayManagerCallback::IDisplayManagerCallback;
use binder::Strong;

use crate::counters;
use crate::display_model::DisplayModel;
use crate::permission::Permissions;
use crate::tasks::Tasks;

/// The interface name counters use.
const INTERFACE: &str = "IDisplayManager";
/// Display.DEFAULT_DISPLAY (Display.java:134): the only display.
const DEFAULT_DISPLAY: i32 = 0;
/// Display.TYPE_INTERNAL (Display.java:437): the desktop's own panel.
const TYPE_INTERNAL: i32 = 1;
/// Display.STATE_ON (ViewProtoEnums.DISPLAY_STATE_ON = 2, Display.java:496).
const STATE_ON: i32 = 2;
/// Display.REMOVE_MODE_MOVE_CONTENT_TO_PRIMARY (Display.java:671).
const REMOVE_MODE_MOVE_CONTENT_TO_PRIMARY: i32 = 0;
/// Display.INVALID_MODE_ID (Display.java:2402): no user-preferred mode.
const INVALID_MODE_ID: i32 = -1;
/// Surface.ROTATION_0 and ColorMode.COLOR_MODE_DEFAULT: the panel's natural orientation and colour mode.
const ROTATION_0: i32 = 0;
const COLOR_MODE_DEFAULT: i32 = 0;
/// The one mode the display has (DisplayInfo.modeId; a mode id is positive, INVALID_MODE_ID is not).
const MODE_ID: i32 = 1;

/// DisplayManagerGlobal.EVENT_DISPLAY_BASIC_CHANGED (DisplayManagerGlobal.java:123): the display's size, density or mode changed.
const EVENT_DISPLAY_BASIC_CHANGED: i32 = 2;
/// DisplayManagerGlobal.INTERNAL_EVENT_FLAG_DISPLAY_BASIC_CHANGED (DisplayManagerGlobal.java:151): the mask bit a callback takes basic changes with (DisplayManagerService shouldSendEvent:4424).
const INTERNAL_EVENT_FLAG_DISPLAY_BASIC_CHANGED: i64 = 1 << 1;

/// DisplayManagerService's CallbackRecord: the calling pid, the callback, the event mask, and its death link (CallbackRecord.binderDied -> onCallbackDied removes the record).
struct CallbackRecord {
    pid: i32,
    callback: Strong<dyn IDisplayManagerCallback>,
    mask: i64,
    dead: Arc<AtomicBool>,
    _death: DeathRecipient,
}

/// The registered IDisplayManagerCallbacks, one per process (keyed by calling pid; a second registration of a pid replaces its mask). A record whose process died is dropped at the next register or delivery.
#[derive(Default)]
pub struct DisplayCallbacks {
    records: Mutex<Vec<CallbackRecord>>,
}

impl DisplayCallbacks {
    /// Fails DEAD_OBJECT when the callback is already dead (AOSP: linkToDeath's RemoteException -> RuntimeException).
    fn register(
        &self,
        pid: i32,
        callback: &Strong<dyn IDisplayManagerCallback>,
        mask: i64,
    ) -> Result<(), StatusCode> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records.retain(|r| !r.dead.load(Ordering::Acquire));
        if let Some(r) = records.iter_mut().find(|r| r.pid == pid) {
            r.mask = mask;
            return Ok(());
        }
        let dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dead);
        let mut death = DeathRecipient::new(move || flag.store(true, Ordering::Release));
        callback.as_binder().link_to_death(&mut death)?;
        records.push(CallbackRecord {
            pid,
            callback: callback.clone(),
            mask,
            dead,
            _death: death,
        });
        Ok(())
    }

    /// Display 0 changed (Tasks::set_display): onDisplayEvent(0, EVENT_DISPLAY_BASIC_CHANGED) to each live callback whose mask takes it (DisplayManagerService.deliverDisplayEvent). Returns the callbacks told.
    pub fn display_changed(&self) -> usize {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        records.retain(|r| !r.dead.load(Ordering::Acquire));
        let mut told = 0;
        for CallbackRecord { callback, mask, .. } in records.iter() {
            if mask & INTERNAL_EVENT_FLAG_DISPLAY_BASIC_CHANGED == 0 {
                continue;
            }
            match callback.r#onDisplayEvent(DEFAULT_DISPLAY, EVENT_DISPLAY_BASIC_CHANGED) {
                Ok(()) => told += 1,
                Err(e) => {
                    counters::count(INTERFACE, &format!("onDisplayEvent failed: {e}"));
                }
            }
        }
        told
    }
}

struct DisplayManager {
    /// the owner of the display values (tasks.rs Tasks::display): read at each call, never copied
    tasks: Arc<Tasks<SpIBinder>>,
    callbacks: Arc<DisplayCallbacks>,
    permissions: Arc<Permissions>,
}

impl Interface for DisplayManager {}

impl IDisplayManager for DisplayManager {
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

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// DisplayManagerService.getDisplayInfo: the record of display 0 from the model; null for any other display (there is no other).
    fn r#getDisplayInfo(&self, display_id: i32) -> binder::Result<Option<DisplayInfo>> {
        Ok((display_id == DEFAULT_DISPLAY).then(|| display_info(&self.tasks.display())))
    }

    /// DisplayManagerService.getPreferredWideGamutColorSpaceIdInternal: mWideColorSpace's id, the second of SurfaceControl.getCompositionColorSpaces; a display with no wide-gamut
    /// composition (the desktop surface is composited as sRGB) reports sRGB: ColorSpace.Named.SRGB, ordinal 0.
    fn r#getPreferredWideGamutColorSpaceId(&self) -> binder::Result<i32> {
        const COLOR_SPACE_SRGB: i32 = 0;
        Ok(COLOR_SPACE_SRGB)
    }

    /// DisplayManagerService.getDisplayIds: display 0, whether or not disabled displays are asked for (the model has no other).
    fn r#getDisplayIds(&self, _include_disabled: bool) -> binder::Result<Option<Vec<i32>>> {
        Ok(Some(vec![DEFAULT_DISPLAY]))
    }

    /// DisplayManagerService.registerCallback: registerCallbackInternal with the added / changed / removed events. Only basic changes are ever sent (one display, never added or removed).
    fn r#registerCallback(
        &self,
        callback: Option<&Strong<dyn IDisplayManagerCallback>>,
    ) -> binder::Result<()> {
        self.register(callback, INTERNAL_EVENT_FLAG_DISPLAY_BASIC_CHANGED)
    }

    /// DisplayManagerService.registerCallbackWithEventMask:4735: null is IllegalArgumentException. The connection-changed bit needs MANAGE_DISPLAYS there; no event of it is ever sent here, so the mask is kept unchecked.
    fn r#registerCallbackWithEventMask(
        &self,
        callback: Option<&Strong<dyn IDisplayManagerCallback>>,
        mask: i64,
    ) -> binder::Result<()> {
        self.register(callback, mask)
    }
}

impl DisplayManager {
    fn register(
        &self,
        callback: Option<&Strong<dyn IDisplayManagerCallback>>,
        mask: i64,
    ) -> binder::Result<()> {
        let callback = callback.ok_or_else(|| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some("listener must not be null"),
            )
        })?;
        self.callbacks
            .register(ThreadState::get_calling_pid(), callback, mask)
            .map_err(Status::from)
    }
}

/// The DisplayInfo of the model's display: its size as the app area and the logical size (no insets, rotation 0), its one mode (refresh rate as both peak and vsync rate), its density
/// (logicalDensityDpi, and physical dpi equal to it: no physical size is known), the state ON, the panel's kind TYPE_INTERNAL. Values with no source in the model (unique id, brightness,
/// vsync offset and presentation deadline, owner) are test values until the platform layer supplies them; nested parcelables the codec does not model stay null.
pub fn display_info(m: &DisplayModel) -> DisplayInfo {
    let mode = DisplayMode {
        mode_id: MODE_ID,
        width: m.width,
        height: m.height,
        peak_refresh_rate: m.refresh_hz,
        vsync_rate: m.refresh_hz,
        is_synthetic: false,
        alternative_refresh_rates: Some(Vec::new()),
        supported_hdr_types: Some(Vec::new()),
    };
    let short_side = m.width.min(m.height);
    let long_side = m.width.max(m.height);
    DisplayInfo {
        kind: TYPE_INTERNAL,
        display_id: DEFAULT_DISPLAY,
        name: Some("ATL display".to_owned()),
        unique_id: Some("local:0".to_owned()),
        app_width: m.width,
        app_height: m.height,
        smallest_nominal_app_width: short_side,
        smallest_nominal_app_height: short_side,
        largest_nominal_app_width: long_side,
        largest_nominal_app_height: long_side,
        logical_width: m.width,
        logical_height: m.height,
        rotation: ROTATION_0,
        mode_id: MODE_ID,
        render_frame_rate: m.refresh_hz,
        supported_refresh_rates: vec![m.refresh_hz],
        default_mode_id: MODE_ID,
        user_preferred_mode_id: INVALID_MODE_ID,
        supported_modes: vec![mode.clone()],
        apps_supported_modes: vec![mode],
        color_mode: COLOR_MODE_DEFAULT,
        supported_color_modes: vec![COLOR_MODE_DEFAULT],
        logical_density_dpi: m.dpi,
        physical_x_dpi: m.dpi as f32,
        physical_y_dpi: m.dpi as f32,
        state: STATE_ON,
        committed_state: STATE_ON,
        remove_mode: REMOVE_MODE_MOVE_CONTENT_TO_PRIMARY,
        brightness_minimum: 0.0,
        brightness_maximum: 1.0,
        brightness_default: 1.0,
        brightness_dim: 0.5,
        hdr_sdr_ratio: f32::NAN,
        can_host_tasks: true,
        ..Default::default()
    }
}

/// A new display service binder answering from the display  owns; callbacks registered with it go to  (the registry tells them of a change).
pub fn new_binder(
    tasks: Arc<Tasks<SpIBinder>>,
    callbacks: Arc<DisplayCallbacks>,
    permissions: Arc<Permissions>,
) -> SpIBinder {
    BnDisplayManager::new_binder(
        DisplayManager {
            tasks,
            callbacks,
            permissions,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
