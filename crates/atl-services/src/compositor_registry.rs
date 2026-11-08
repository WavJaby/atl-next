// SPDX-License-Identifier: MIT
//! The compositor registry (M3 slice 4, plan C3 "Amended 2026-11-04"): how an app process's in-process SurfaceFlinger tells the system process about its UI agent (ui_agent.rs) and its window.
//! The app process serves its service objects under AOSP's names to its own lookups (atl-service-view), so libgui finds them unchanged. REGISTER hands the registry its compositor too, and GET_COMPOSER gives it to the shell and system uids (the harness's capture, screencap.rs), as AOSP's SurfaceFlinger serves capture to READ_FRAME_BUFFER holders.
//! The registry keys what it holds by the caller's uid, taken from the transaction. Processes of one uid trust each other (one app); a restarted process re-registers and replaces the dead entry.
//!
//! This module is the protocol (names, codes, properties): plain Rust, shared by the app side (atl-composer) and the registry service (`registry_service`, feature `converter`).

/// The registry's name in servicemanager.
pub const SERVICE: &str = "atl.compositor_registry";
/// Its interface descriptor.
pub const DESCRIPTOR: &str = "atl.ICompositorRegistry";
/// `IBinder.FIRST_CALL_TRANSACTION`: the app's UI agent binder, then its compositor (`android.gui.ISurfaceComposer`); reply: a status only.
pub const REGISTER: u32 = 1;
/// The transaction a UI-mode app process sends when its window's content size changes: the Task's number (the one its UI agent was named, ui_agent.rs TASK), then width and height (i32, physical pixels). The caller's uid must own that Task (tasks.rs). Reply: a status only.
pub const SET_BOUNDS: u32 = 2;
/// The transaction a UI-mode app process sends when the monitor its window is on changes (another monitor, or its scale or mode): the monitor as Android's display, a String16 in the `--display` form (display_model.rs `DisplayModel::to_arg`). The caller's uid must own a Task (an app with a window). Reply: a status only.
pub const SET_DISPLAY: u32 = 3;
/// The transaction a UI-mode app process sends when its window is installed, before any Java runs: no arguments. Reply: the size its window opens at, width then height (i32, physical pixels), then the package of its Task (String, empty when it has none). The Task is its launch's (tasks.rs); the size is that activity's initial bounds (desktop_bounds.rs), or for a process with no launch the default activity's bounds on the display. The package is the window's app id (atl-composer ui_window). The caller's pid names the process.
pub const TASK_INFO: u32 = 4;
/// The transaction the shell (AID_SHELL 2000) or system (AID_SYSTEM 1000) uid sends to read an app's display: the app's package (String). Reply: the AOSP exception header (ok, or SECURITY for any other caller: AOSP's SecurityException, as captureDisplay needs READ_FRAME_BUFFER, which only those uids hold here), then the compositor binder of the newest UI-mode launch of that package, or null when there is none.
pub const GET_COMPOSER: u32 = 5;
/// The system property holding the display the in-app compositor answers with (`DisplayModel::to_arg`): the same model atld forwards to the services, delivered to the app the way its other environment is. Its presence marks an ATL app process (atl-composer start).
pub const DISPLAY_PROPERTY: &str = "ro.atl.display";
/// The system property holding the headless pointer script (atld `--headless-pointer`, parsed by atl-composer `input::parse_script`), delivered like [`DISPLAY_PROPERTY`]. Unset: no input thread; the window handoff still works.
pub const POINTER_PROPERTY: &str = "ro.atl.headless.pointer";
/// The system property selecting an app process's UI mode (atld `--ui`): `gtk` presents the compositor's scenes into a GTK window of the process, on its main thread (atl-app-launcher lib.rs `start_ui`, atl-composer). Unset: headless.
pub const UI_PROPERTY: &str = "ro.atl.ui";
/// The system property holding the key tilt settings (atld `--tilt-keys`, [`TiltConfig::to_arg`]), read by atl-composer's tilt. Unset: no tilt.
pub const TILT_PROPERTY: &str = "ro.atl.tilt";

/// The key tilt: the arrow / A-D keys tilt the device for accelerometer games (sensor data injection, atl-composer `tilt`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TiltConfig {
    /// The angle a held key reaches, degrees.
    pub max_deg: f32,
    /// How long a held key takes from level to `max_deg`, and a released one back, ms.
    pub ramp_ms: u32,
    /// The injected events' period, ms.
    pub period_ms: u32,
    /// Test only (atld `--tilt-probe-unfocused`): while no window of the process has focus, inject the 90 deg probe reading anyway, the misbehaving injector the services' focus check must refuse.
    pub probe_unfocused: bool,
}

impl Default for TiltConfig {
    /// 30 deg: x = g sin 30 = g/2, a firm tilt below a game's own clamp; 250 ms: a wrist's quick tilt; 20 ms: SENSOR_DELAY_GAME (SensorManager.getDelay), what games register with.
    fn default() -> Self {
        TiltConfig {
            max_deg: 30.0,
            ramp_ms: 250,
            period_ms: 20,
            probe_unfocused: false,
        }
    }
}

impl TiltConfig {
    /// `max_deg,ramp_ms,period_ms`, then `,probe` when [`TiltConfig::probe_unfocused`].
    pub fn to_arg(&self) -> String {
        let probe = if self.probe_unfocused { ",probe" } else { "" };
        format!(
            "{},{},{}{probe}",
            self.max_deg, self.ramp_ms, self.period_ms
        )
    }

    /// [`TiltConfig::to_arg`]'s form; the angle in (0, 90], the period > 0.
    pub fn parse_arg(s: &str) -> Result<TiltConfig, String> {
        let bad =
            || format!("{s}: not max_deg,ramp_ms,period_ms[,probe] (angle in (0, 90], period > 0)");
        let mut it = s.split(',');
        let (Some(a), Some(r), Some(p), probe, None) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Err(bad());
        };
        let c = TiltConfig {
            max_deg: a.parse().map_err(|_| bad())?,
            ramp_ms: r.parse().map_err(|_| bad())?,
            period_ms: p.parse().map_err(|_| bad())?,
            probe_unfocused: match probe {
                None => false,
                Some("probe") => true,
                Some(_) => return Err(bad()),
            },
        };
        if !(c.max_deg > 0.0 && c.max_deg <= 90.0) || c.period_ms == 0 {
            return Err(bad());
        }
        Ok(c)
    }
}
/// AOSP's service names, libgui's lookups (SurfaceComposerClient.cpp ComposerService / ComposerServiceAIDL), served in-process.
pub const SURFACE_FLINGER: &str = "SurfaceFlinger";
pub const SURFACE_FLINGER_AIDL: &str = "SurfaceFlingerAIDL";
