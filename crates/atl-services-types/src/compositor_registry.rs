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
/// The transaction a UI-mode app process sends at its compositor's start, before any Java runs: no arguments. Reply: the size its window opens at, width then height (i32, physical pixels), then the package of its Task (String). The Task is its launch's (tasks.rs); the size is that activity's initial bounds (desktop_bounds.rs). The package is the window's app id (atl-composer ui_window). The caller's pid names the process.
/// A process with no Task (started for a provider, service or receiver) gets 0 x 0 and an empty package and opens no window: AOSP shows none for it, and a second toplevel takes the desktop's focus from the app's.
pub const TASK_INFO: u32 = 4;
/// The transaction the shell (AID_SHELL 2000) or system (AID_SYSTEM 1000) uid sends to read an app's display: the app's package (String). Reply: the AOSP exception header (ok, or SECURITY for any other caller: AOSP's SecurityException, as captureDisplay needs READ_FRAME_BUFFER, which only those uids hold here), then the compositor binder of the newest UI-mode launch of that package, or null when there is none.
pub const GET_COMPOSER: u32 = 5;
/// The transaction a UI-mode app process sends when the desktop asks to close its window: the Task's number (i32). The Shell's removeTask (WindowContainerTransaction.removeTask). The caller's uid must own that Task, as for [`SET_BOUNDS`]. Reply: a status only.
pub const REMOVE_TASK: u32 = 6;
/// The system property holding the display the in-app compositor answers with (`DisplayModel::to_arg`): the same model atld forwards to the services, delivered to the app the way its other environment is. Its presence marks an ATL app process (atl-composer start).
pub const DISPLAY_PROPERTY: &str = "ro.atl.display";
/// The system property holding the headless pointer script (atld `--headless-pointer`, parsed by atl-composer `input::parse_script`), delivered like [`DISPLAY_PROPERTY`]. Unset: no input thread; the window handoff still works.
pub const POINTER_PROPERTY: &str = "ro.atl.headless.pointer";
/// The system property selecting an app process's UI mode (atld `--ui`): `gtk` presents the compositor's scenes into a GTK window of the process, on its main thread (atl-app-launcher lib.rs `start_ui`, atl-composer). Unset: headless.
pub const UI_PROPERTY: &str = "ro.atl.ui";
/// The system property holding the desktop input adaptations ([`DesktopInput::to_arg`]), read by atl-composer's input. Unset: none, exact AOSP input.
pub const DESKTOP_INPUT_PROPERTY: &str = "ro.atl.desktop-input";
/// The system property holding the app's starting window ([`Splash::to_arg`]): what its window shows from its opening, before ART boots, until the app's first frame (AOSP's splash screen, StartingSurfaceController). Unset: the window opens empty (atl-composer `ui_window`).
pub const SPLASH_PROPERTY: &str = "ro.atl.splash";

/// The starting window's content, from the launcher activity's theme (`windowSplashScreenBackground`, else `windowBackground`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splash {
    /// The background, an Android colour int (ARGB).
    pub background: u32,
}

impl Splash {
    /// `background=#AARRGGBB`; later keys join it `;`-separated, as [`DesktopInput::to_arg`]'s parts.
    pub fn to_arg(&self) -> String {
        format!("background=#{:08X}", self.background)
    }

    /// [`Splash::to_arg`]'s form: the background required, once, eight hex digits.
    pub fn parse_arg(s: &str) -> Result<Splash, String> {
        let bad = || format!("{s}: not background=#AARRGGBB");
        let mut background = None;
        for part in s.split(';') {
            match part.split_once('=') {
                Some(("background", v)) if background.is_none() => {
                    let hex = v
                        .strip_prefix('#')
                        .filter(|h| h.len() == 8)
                        .ok_or_else(bad)?;
                    background = Some(u32::from_str_radix(hex, 16).map_err(|_| bad())?);
                }
                _ => return Err(bad()),
            }
        }
        Ok(Splash {
            background: background.ok_or_else(bad)?,
        })
    }
}

/// Desktop input adaptations: what a desktop lacks that a phone has, emulated with what it has. Each is a switch; off is exact AOSP behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DesktopInput {
    /// The left mouse button is one finger: its down / move / up reach the app as a touchscreen's (atl-composer `input::touch::MouseFinger`). AOSP has none (CursorInputMapper sends SOURCE_MOUSE), and touch-only games ignore the mouse.
    pub mouse_touch: bool,
    /// Esc -> Back: the physical keyboard's map is Generic.kcm combined with ATL's overlay (atl-services `input::ESC_BACK_OVERLAY`), so an unhandled Esc falls back to BACK (KeyCharacterMap.getFallbackAction). Pinned AOSP Generic.kcm has ESCAPE `base: none`. Served by the input service, which takes it from atld's `--desktop-input`.
    pub esc_back: bool,
    /// The key tilt; None: off.
    pub tilt: Option<TiltConfig>,
}

impl DesktopInput {
    /// `;`-separated: `mouse-touch`, `esc-back`, `tilt=` + [`TiltConfig::to_arg`]; `none` when every adaptation is off.
    pub fn to_arg(&self) -> String {
        let mut parts = Vec::new();
        if self.mouse_touch {
            parts.push("mouse-touch".to_owned());
        }
        if self.esc_back {
            parts.push("esc-back".to_owned());
        }
        if let Some(t) = &self.tilt {
            parts.push(format!("tilt={}", t.to_arg()));
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(";")
        }
    }

    /// [`DesktopInput::to_arg`]'s form; each adaptation at most once.
    pub fn parse_arg(s: &str) -> Result<DesktopInput, String> {
        let mut d = DesktopInput::default();
        if s == "none" {
            return Ok(d);
        }
        for part in s.split(';') {
            match part.split_once('=') {
                None if part == "mouse-touch" && !d.mouse_touch => d.mouse_touch = true,
                None if part == "esc-back" && !d.esc_back => d.esc_back = true,
                Some(("tilt", arg)) if d.tilt.is_none() => {
                    d.tilt = Some(TiltConfig::parse_arg(arg)?)
                }
                _ => {
                    return Err(format!(
                        "{s}: `{part}` is not mouse-touch, esc-back or tilt=<arg>, once each, or none"
                    ));
                }
            }
        }
        Ok(d)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination survives the property round trip; a repeated or unknown part is refused.
    #[test]
    fn desktop_input_round_trips() {
        let tilt = Some(TiltConfig::default());
        for esc_back in [false, true] {
            for (mouse_touch, tilt) in [(false, None), (true, None), (false, tilt), (true, tilt)] {
                let d = DesktopInput {
                    mouse_touch,
                    esc_back,
                    tilt,
                };
                assert_eq!(DesktopInput::parse_arg(&d.to_arg()), Ok(d));
            }
        }
        assert_eq!(DesktopInput::default().to_arg(), "none");
        assert!(DesktopInput::parse_arg("mouse-touch;mouse-touch").is_err());
        assert!(DesktopInput::parse_arg("esc-back;esc-back").is_err());
        assert!(DesktopInput::parse_arg("hover").is_err());
    }

    /// The colour int's hex digits are its bytes A, R, G, B in order; the form is strict.
    #[test]
    fn splash_round_trips() {
        let s = Splash {
            background: 0xFF20_4080,
        };
        assert_eq!(s.to_arg(), "background=#FF204080");
        assert_eq!(Splash::parse_arg(&s.to_arg()), Ok(s));
        assert!(Splash::parse_arg("background=#204080").is_err());
        assert!(Splash::parse_arg("background=#FF204080;background=#FF204080").is_err());
        assert!(Splash::parse_arg("icon=x").is_err());
        assert!(Splash::parse_arg("").is_err());
    }
}
