// SPDX-License-Identifier: MIT
//! Stage `move` of `xtask test window`: the window moves to an output of another density, and Android follows. The nested compositor has two outputs, [`GATE_OUTPUT`] and [`SCALED_OUTPUT`] to its right; only the first is on when the window opens (with both on, labwc opened it on the second). After its first draw the probe turns the second on and the first off in one configuration ([`OutputControl::only`]), so labwc moves the window to the second.
//! The window reports its new monitor (Window::on_monitor), the composer submits it (TaskReports::submit_monitor -> registry REPORT_MONITOR), the services make it their display ([`checks`]) and answer with it, and the composer takes that answer as its display.
//! Lifecycle, from AOSP: the move changes densityDpi (CONFIG_DENSITY) and screen size; the fixture declares no configChanges, so ActivityRecord.shouldRelaunchLocked (changes & ~info.getRealConfigChanged() != 0) relaunches the activity.
//! Then rotation: once the relaunched instance has run its frames, the second output takes transform [`ROTATED_TRANSFORM`] in place. GDK reports no change for a half turn (same geometry), so only the platform layer's own
//! wl_output listener (atl-platform-ui-linux outputs.rs) can see it; the services must take a second display, the same one at Surface.ROTATION_180. Only windowConfiguration changes, which is not a public
//! config diff (ActivityRecord.getConfigurationChanges: `diffPublicOnly`), so there is no second relaunch.

use std::path::Path;
use std::time::Duration;

use atl_services_types::display_model::{DENSITY_DEFAULT, Declaration, DisplayModel};

use super::{
    ACK_WAIT, CREATE_LINE, DRAW_LINE, INPUT_SETTLE, Liveness, RELAUNCH_COUNTER, RELAUNCHED_COUNTER,
    Sub, counter,
};
use crate::composer::word;
use crate::nested_wayland::{GATE_OUTPUT, Output, OutputControl};
use crate::wait::{Miss, Wait, until};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The second output: the gate output's mode at scale 1.6, so density 160 x 1.6 = 256 (DisplayModel::from_monitor), a fractional scale as on a typical HiDPI laptop panel.
pub const SCALED_OUTPUT: Output = Output {
    scale: 1.6,
    ..GATE_OUTPUT
};
/// What the proxy withholds from the app's security context (wayland_proxy.rs `Withheld`): Hyprland's sandbox policy, where GDK then knows each output's integer wl_output scale only
/// (2 on [`SCALED_OUTPUT`]) and its logical size is the mode / 2. Android's density must still be the fractional one (measured on a scale-1.6 Hyprland laptop 2026-11-22: 320 reported against
/// atld's 256 at start, and WeChat exited on that density change).
pub const WITHHELD: [&str; 1] = ["zxdg_output_manager_v1"];
/// The outputs, left to right; the window opens on the first (the gates' display).
pub const OUTPUTS: [Output; 2] = [GATE_OUTPUT, SCALED_OUTPUT];
/// wl_output transform 180 (wayland.xml), given to the second output after the move: same size, so no GDK signal and no relaunch.
pub const ROTATED_TRANSFORM: i32 = 2;
/// How long the relaunched instance may take to run its frames after the move.
const RELAUNCH_WAIT: Duration = Duration::from_secs(60);
/// The fixture's density line (Main.java logDensity): `AtlWindow: density <created|changed> config=N metrics=N`.
const DENSITY_LINE: &str = "AtlWindow: density ";
/// The services' monitor line (registry_service.rs report_monitor): `services: monitor <m> from uid U: display <d>`, ending [`UNCHANGED`] when the report changed nothing.
const MONITOR_LINE: &str = "services: monitor ";
/// The suffix of a monitor report that changed nothing.
const UNCHANGED: &str = " unchanged";
/// The fixture's size line (testdata Main.java logDensity): `AtlWindow: size <created|changed> sw=N task_sw=N real=WxH`, at each onCreate and onConfigurationChanged.
const SIZE_LINE: &str = "AtlWindow: size ";
/// The fixture's display line (testdata Main.java logDisplay): `AtlWindow: display <created|changed|settled> info=WxH@hz,dpi,rotation sf=WxH@hz,dpi`, what IDisplayManager.getDisplayInfo
/// and SurfaceFlinger's active mode answer the app; `settled` after each instance's last frame callback, when a reply that relaunched it has reached the in-app compositor.
const APP_DISPLAY_LINE: &str = "AtlWindow: display ";
/// The tag of the display line logged after an instance's frames.
const SETTLED: &str = "settled";
/// Main.java's last frame callback line prefix (FRAME_CALLBACKS = 31): each instance's resume runs the whole series.
const LAST_FRAME: &str = "AtlWindow: frame 31 ";

/// The probe: once the window has drawn on the first output, the first output goes off.
pub fn drive(proxy: Proxy, app_log: &Path, outputs: OutputControl) -> Result<Liveness, Miss> {
    until(&Wait::event(
        "configure-ack",
        "the window never acked a configure",
        ACK_WAIT,
        &proxy.events,
        &|e: &ProxyEvent| matches!(e, ProxyEvent::Acked { .. }),
    ))?;
    until(&Wait::log(
        "draw",
        "the fixture never drew",
        ACK_WAIT,
        app_log,
        &|t: &str| t.contains(DRAW_LINE),
    ))?;
    std::thread::sleep(INPUT_SETTLE);
    outputs.only(1).map_err(|e| format!("{e:#}"))?;
    // the relaunched (second) instance ran its frame callbacks: the move's configuration change is over
    until(&Wait::log(
        "relaunch-frames",
        "the relaunched instance never ran its frames",
        RELAUNCH_WAIT,
        app_log,
        &|t: &str| t.matches(LAST_FRAME).count() >= 2,
    ))?;
    outputs
        .set_transform(1, ROTATED_TRANSFORM)
        .map_err(|e| format!("{e:#}"))?;
    // the policy was exercised: the app's context asked for globals and missed the withheld ones
    if proxy.withheld().dropped() == 0 {
        return Err(Miss::Wrong(format!(
            "the app's security context was never denied {WITHHELD:?}: the sandbox policy was not exercised"
        )));
    }
    Ok(Liveness {
        answered: false,
        silent: false,
        resumed_ms: None,
        configured: Vec::new(),
        idle: None,
        minimized: false,
    })
}

/// The sub-gates: display (the services took a display at `moved`'s density), density (the last instance's Configuration and DisplayMetrics say it; the first said `opened`'s), relaunch (AOSP's: one relaunch, ended, two instances), frames (the relaunched instance drew and ran its frame callbacks).
/// `declared` (`--declare-display`) adds the declared sub-gate and changes display: a declared display keeps rotation 0, so the turned output changes nothing.
pub fn checks(
    app: &str,
    services: &str,
    liveness: &Result<Liveness, String>,
    opened: DisplayModel,
    moved: DisplayModel,
    declared: Option<Declaration>,
) -> Vec<Sub> {
    let mut subs = vec![
        (
            "probe",
            liveness
                .as_ref()
                .map(|_| "second output on, first off".to_owned())
                .map_err(Clone::clone),
        ),
        (
            "display",
            display_check(
                services,
                moved,
                Output {
                    transform: ROTATED_TRANSFORM,
                    ..SCALED_OUTPUT
                }
                .display(),
                declared.is_some(),
            ),
        ),
        ("density", density_check(app, opened.dpi, moved.dpi)),
        ("relaunch", relaunch_check(app, services)),
        ("frames", frames_check(app)),
    ];
    if let Some(declared) = declared {
        subs.push(("declared", declared_check(app, declared, opened, moved)));
    }
    subs
}

/// The displays the services took, in order: the reports that changed the display.
fn displays(services: &str) -> Vec<DisplayModel> {
    display_reports(services)
        .into_iter()
        .filter_map(|r| r.changed.then_some(r.display))
        .collect()
}

/// One monitor report the services logged: the monitor the window reported, the display the services answer with, and whether it changed their display.
pub(super) struct MonitorReport {
    pub monitor: DisplayModel,
    pub display: DisplayModel,
    pub changed: bool,
}

/// Every monitor report the services logged, in order (`services: monitor <m> from uid U: display <d>[ unchanged]`).
pub(super) fn display_reports(services: &str) -> Vec<MonitorReport> {
    services
        .lines()
        .filter_map(|l| {
            let (monitor, shown) = l.strip_prefix(MONITOR_LINE)?.split_once(": display ")?;
            Some(MonitorReport {
                monitor: DisplayModel::parse_arg(monitor.split_whitespace().next()?).ok()?,
                display: DisplayModel::parse_arg(shown.split_whitespace().next()?).ok()?,
                changed: !l.ends_with(UNCHANGED),
            })
        })
        .collect()
}

/// The displays the services took: `moved`'s density, rate and rotation, then the same at the rotated output's rotation (`rotated`, [`ROTATED_TRANSFORM`]: Output::display derives it from wayland.xml).
/// The size is the window's monitor as GDK reports it (logical size x scale), which wl_fixed rounding of 1.6 moves off the mode by a few pixels, so the size is shown, not judged.
fn display_check(
    services: &str,
    moved: DisplayModel,
    rotated: DisplayModel,
    declared: bool,
) -> Result<String, String> {
    let all = displays(services);
    let same = |d: &DisplayModel, w: &DisplayModel| {
        d.dpi == w.dpi && d.refresh_hz == w.refresh_hz && d.rotation == w.rotation
    };
    if declared {
        // a declared display keeps rotation 0 (the monitor's turn is invisible to the app): the turned output's own report (its monitor at the turned rotation) changes nothing.
        // Its monitor is matched, not the last line: a repeat of the moved monitor is also an unchanged report and would stand in for a lost turn
        let turned_unchanged = display_reports(services)
            .iter()
            .any(|r| r.monitor.rotation == rotated.rotation && !r.changed);
        if all.len() == 1 && same(&all[0], &moved) && turned_unchanged {
            return Ok(format!(
                "{} then the turned output unchanged",
                all[0].to_arg()
            ));
        }
        return Err(format!(
            "displays {:?}, want one change to dpi {} and an unchanged report of the monitor at rotation {}",
            all.iter().map(DisplayModel::to_arg).collect::<Vec<_>>(),
            moved.dpi,
            rotated.rotation
        ));
    }
    // the window opened on the gates' display (no change), moved once, then its output turned
    if all.len() == 2 && same(&all[0], &moved) && same(&all[1], &rotated) {
        Ok(format!("{} then {}", all[0].to_arg(), all[1].to_arg()))
    } else {
        Err(format!(
            "displays {:?}, want two changes: dpi {} at {} Hz rotation {}, then rotation {}",
            all.iter().map(DisplayModel::to_arg).collect::<Vec<_>>(),
            moved.dpi,
            moved.refresh_hz,
            moved.rotation,
            rotated.rotation
        ))
    }
}

/// One instance's size line: the activity's smallest width (its Task's override, dp) and the display's real size (px). The application context's sw is logged, not read: in a freeform
/// Task it is the process's configuration, which carries the Task's override (AOSP preBindApplication), not the global one (global_configuration, unit tested).
struct InstanceSize {
    task_sw: i32,
    real: (i32, i32),
}

/// The size lines the app logged, one per instance, in order.
fn sizes(app: &str) -> Vec<InstanceSize> {
    app.lines()
        .filter_map(|l| {
            let rest = l.split_once(SIZE_LINE)?.1;
            let field = |key: &str| rest.split_whitespace().find_map(|w| w.strip_prefix(key));
            let (w, h) = field("real=")?.split_once('x')?;
            Some(InstanceSize {
                task_sw: field("task_sw=")?.parse().ok()?,
                real: (w.parse().ok()?, h.parse().ok()?),
            })
        })
        .collect()
}

/// One display line of the app: its tag and what DisplayInfo (`info`) and SurfaceFlinger's active mode (`sf`: natural size, refresh rate, dpi) answered; an answer that failed to parse is None.
struct AppDisplay {
    when: String,
    info: Option<DisplayModel>,
    sf: Option<(i32, i32, f32, i32)>,
}

/// The display lines the app logged, in order.
fn app_displays(app: &str) -> Vec<AppDisplay> {
    app.lines()
        .filter_map(|l| {
            let rest = l.split_once(APP_DISPLAY_LINE)?.1;
            let field = |key: &str| rest.split_whitespace().find_map(|w| w.strip_prefix(key));
            let sf = field("sf=").and_then(|t| {
                let (size, rest) = t.split_once('@')?;
                let (w, h) = size.split_once('x')?;
                let (hz, dpi) = rest.split_once(',')?;
                Some((
                    w.parse().ok()?,
                    h.parse().ok()?,
                    hz.parse().ok()?,
                    dpi.parse().ok()?,
                ))
            });
            Some(AppDisplay {
                when: rest.split_whitespace().next()?.to_owned(),
                info: field("info=").and_then(|t| DisplayModel::parse_arg(t).ok()),
                sf,
            })
        })
        .collect()
}

/// One app display line against the display `want` (size, dpi, refresh; rotation 0): DisplayInfo must be it and SurfaceFlinger's mode its natural size at the same rate and dpi.
fn app_display_problem(line: Option<&AppDisplay>, at: &str, want: DisplayModel) -> Option<String> {
    let Some(line) = line else {
        return Some(format!("no app display line {at}"));
    };
    let want_sf = (want.width, want.height, want.refresh_hz, want.dpi);
    let info_ok = line.info.is_some_and(|i| {
        (i.width, i.height, i.refresh_hz, i.dpi, i.rotation)
            == (want.width, want.height, want.refresh_hz, want.dpi, 0)
    });
    (!info_ok || line.sf != Some(want_sf)).then(|| {
        format!(
            "{at} ({}): DisplayInfo {}, SurfaceFlinger {}, want {}",
            line.when,
            line.info.map_or("unreadable".to_owned(), |i| i.to_arg()),
            line.sf
                .map_or("unreadable".to_owned(), |(w, h, hz, dpi)| format!(
                    "{w}x{h}@{hz},{dpi}"
                )),
            want.to_arg()
        )
    })
}

/// The declared display on the two outputs, as the app reads it. The real display size is the declared size in px at each output's density (px = dp x dpi / 160); what IDisplayManager.getDisplayInfo
/// and SurfaceFlinger's active mode answer the app is that size at the output's dpi and rate, at open (the first instance's create) and after the move (the last `settled` line); the relaunched
/// instance ran its frames. No app sw row: in a freeform Task the app's Configuration is its Task's (AOSP preBindApplication).
fn declared_check(
    app: &str,
    declared: Declaration,
    opened: DisplayModel,
    moved: DisplayModel,
) -> Result<String, String> {
    let sized = sizes(app);
    let [first, .., last] = sized.as_slice() else {
        return Err(format!(
            "{} size lines, want one per instance (opened, moved)",
            sized.len()
        ));
    };
    let px = |dp: i32, dpi: i32| {
        (f64::from(dp) * f64::from(dpi) / f64::from(DENSITY_DEFAULT)).round() as i32
    };
    let want = |d: DisplayModel| (px(declared.width_dp, d.dpi), px(declared.height_dp, d.dpi));
    let shown = |d: DisplayModel| DisplayModel {
        width: want(d).0,
        height: want(d).1,
        rotation: 0,
        ..d
    };
    let mut problems = Vec::new();
    if first.real != want(opened) {
        problems.push(format!(
            "real {}x{} at the opened output, want {}x{}",
            first.real.0,
            first.real.1,
            want(opened).0,
            want(opened).1
        ));
    }
    if last.real != want(moved) {
        problems.push(format!(
            "real {}x{} after the move, want {}x{}",
            last.real.0,
            last.real.1,
            want(moved).0,
            want(moved).1
        ));
    }
    if app.matches(LAST_FRAME).count() < 2 {
        problems.push("the relaunched instance ran no frame after the move".to_owned());
    }
    let lines = app_displays(app);
    let settled = lines.iter().rfind(|l| l.when == SETTLED);
    problems.extend(app_display_problem(lines.first(), "at open", shown(opened)));
    problems.extend(app_display_problem(settled, "after the move", shown(moved)));
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    Ok(format!(
        "real {}x{} then {}x{}, task sw {} then {}, DisplayInfo = SurfaceFlinger {} then {}",
        want(opened).0,
        want(opened).1,
        want(moved).0,
        want(moved).1,
        first.task_sw,
        last.task_sw,
        shown(opened).to_arg(),
        shown(moved).to_arg()
    ))
}

/// (config, metrics) of each density line, in order.
fn densities(app: &str) -> Vec<(i32, i32)> {
    app.lines()
        .filter(|l| l.contains(DENSITY_LINE))
        .filter_map(|l| {
            Some((
                word(l, "config")?.parse().ok()?,
                word(l, "metrics")?.parse().ok()?,
            ))
        })
        .collect()
}

fn density_check(app: &str, opened: i32, moved: i32) -> Result<String, String> {
    let all = densities(app);
    match (all.first(), all.last()) {
        (Some(&first), Some(&last)) if first == (opened, opened) && last == (moved, moved) => {
            Ok(format!("{all:?}"))
        }
        _ => Err(format!(
            "densities (config, metrics) {all:?}, want ({opened}, {opened}) first and ({moved}, {moved}) last"
        )),
    }
}

/// AOSP relaunches on the density change (module doc): exactly one relaunch, its end reported, two instances.
fn relaunch_check(app: &str, services: &str) -> Result<String, String> {
    let creates = app.matches(CREATE_LINE).count() as u32;
    let relaunches = counter(services, RELAUNCH_COUNTER);
    let ended = counter(services, RELAUNCHED_COUNTER);
    let seen = format!("{creates} onCreate, {relaunches} relaunches ({ended} ended)");
    if creates == 2 && relaunches == 1 && ended == 1 {
        Ok(seen)
    } else {
        Err(format!("{seen}, want 2 onCreate and 1 relaunch, ended"))
    }
}

/// The first instance drew before the move (so the relaunch is the move's, not a display change at window open); after the second onCreate, the new instance draws and its frame callbacks run to the last.
fn frames_check(app: &str) -> Result<String, String> {
    let second = app
        .match_indices(CREATE_LINE)
        .nth(1)
        .map(|(at, _)| at)
        .ok_or("no second instance")?;
    if !app[..second].contains(DRAW_LINE) {
        return Err("the relaunch came before the first instance drew: the window did not open on the first output".to_owned());
    }
    let after = &app[second..];
    match (after.contains(DRAW_LINE), after.contains(LAST_FRAME)) {
        (true, true) => Ok("the relaunched instance drew and ran its frame callbacks".to_owned()),
        (drew, framed) => Err(format!(
            "after the relaunch: drew {drew}, last frame callback {framed}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.6 x DENSITY_DEFAULT (160) = 256: the density the scaled output must reach.
    #[test]
    fn scaled_output_is_density_256() {
        let o = SCALED_OUTPUT;
        let d = DisplayModel::from_monitor(o.width as u32, o.height as u32, None, o.scale, 0);
        assert_eq!(d.dpi, 256);
    }

    /// wayland.xml counter-clockwise turns as Android's clockwise ROTATION_*: 180 stays 180, 90 is ROTATION_270 with the sides swapped, 270 is ROTATION_90.
    #[test]
    fn output_transform_is_the_display_rotation() {
        let turned = |transform| {
            let d = Output {
                transform,
                ..GATE_OUTPUT
            }
            .display();
            (d.rotation, d.width, d.height)
        };
        assert_eq!(turned(0), (0, 1920, 1200));
        assert_eq!(turned(ROTATED_TRANSFORM), (2, 1920, 1200));
        assert_eq!(turned(1), (3, 1200, 1920));
        assert_eq!(turned(3), (1, 1200, 1920));
    }

    /// Declared: the turn passes only on the turned monitor's own report. A lost turn whose place a repeat of the moved monitor's report takes (both unchanged) is red.
    #[test]
    fn declared_display_needs_the_turns_own_report() {
        let line = |monitor: &str, unchanged: bool| {
            format!(
                "services: monitor {monitor} from uid 10000: display 658x1462@60,256,0{}\n",
                if unchanged { UNCHANGED } else { "" }
            )
        };
        let (moved, turned) = ("1920x1200@60,256,0", "1920x1200@60,256,2");
        let start = "services: monitor 1920x1200@60,160,0 from uid 10000: display 411x914@60,160,0 unchanged\n";
        let display = DisplayModel::parse_arg("658x1462@60,256,0").expect("model");
        let rotated = DisplayModel::parse_arg(turned).expect("model");
        let check = |log: String| display_check(&log, display, rotated, true);
        assert!(
            check(format!(
                "{start}{}{}",
                line(moved, false),
                line(turned, true)
            ))
            .is_ok()
        );
        let repeat = format!("{start}{}{}", line(moved, false), line(moved, true));
        assert!(check(repeat).is_err());
    }

    #[test]
    fn density_reads_first_and_last() {
        let app = "AtlWindow: density created config=160 metrics=160\nAtlWindow: density created config=256 metrics=256\n";
        assert!(density_check(app, 160, 256).is_ok());
        let stale = "AtlWindow: density created config=160 metrics=160\nAtlWindow: density created config=256 metrics=160\n";
        assert!(density_check(stale, 160, 256).is_err());
    }
}
