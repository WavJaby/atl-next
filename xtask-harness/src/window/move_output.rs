// SPDX-License-Identifier: MIT
//! Stage `move` of `xtask test window`: the window moves to an output of another density, and Android follows. The nested compositor has two outputs, [`GATE_OUTPUT`] and [`SCALED_OUTPUT`] to its right; only the first is on when the window opens (with both on, labwc opened it on the second). After its first draw the probe turns the second on and the first off in one configuration ([`OutputControl::only`]), so labwc moves the window to the second.
//! The window reports its new monitor (Window::on_monitor), the composer submits it as the display (TaskSender::submit_display -> registry SET_DISPLAY), and the services make it every Task's display ([`checks`]).
//! Lifecycle, from AOSP: the move changes densityDpi (CONFIG_DENSITY) and screen size; the fixture declares no configChanges, so ActivityRecord.shouldRelaunchLocked (changes & ~info.getRealConfigChanged() != 0) relaunches the activity.
//! Then rotation: once the relaunched instance has run its frames, the second output takes transform [`ROTATED_TRANSFORM`] in place. GDK reports no change for a half turn (same geometry), so only the platform layer's own
//! wl_output listener (atl-platform-ui-linux outputs.rs) can see it; the services must take a second display, the same one at Surface.ROTATION_180. Only windowConfiguration changes, which is not a public
//! config diff (ActivityRecord.getConfigurationChanges: `diffPublicOnly`), so there is no second relaunch.

use std::path::Path;
use std::time::{Duration, Instant};

use atl_services_types::display_model::DisplayModel;

use super::{
    ACK_WAIT, CREATE_LINE, DRAW_LINE, INPUT_SETTLE, Liveness, RELAUNCH_COUNTER, RELAUNCHED_COUNTER,
    Sub, await_event, counter, wait_for_line,
};
use crate::composer::word;
use crate::nested_wayland::{GATE_OUTPUT, Output, OutputControl};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The second output: the gate output's mode at scale 1.6, so density 160 x 1.6 = 256 (DisplayModel::from_monitor), a fractional scale as on a typical HiDPI laptop panel.
pub const SCALED_OUTPUT: Output = Output {
    scale: 1.6,
    ..GATE_OUTPUT
};
/// The outputs, left to right; the window opens on the first (the gates' display).
pub const OUTPUTS: [Output; 2] = [GATE_OUTPUT, SCALED_OUTPUT];
/// wl_output transform 180 (wayland.xml), given to the second output after the move: same size, so no GDK signal and no relaunch.
pub const ROTATED_TRANSFORM: i32 = 2;
/// How long the relaunched instance may take to run its frames after the move.
const RELAUNCH_WAIT: Duration = Duration::from_secs(60);
/// The fixture's density line (Main.java logDensity): `AtlWindow: density <created|changed> config=N metrics=N`.
const DENSITY_LINE: &str = "AtlWindow: density ";
/// The services' display line (registry_service.rs set_display): `services: display WxH@hz,dpi from uid U`.
const DISPLAY_LINE: &str = "services: display ";
/// Main.java's last frame callback line prefix (FRAME_CALLBACKS = 31): each instance's resume runs the whole series.
const LAST_FRAME: &str = "AtlWindow: frame 31 ";

/// The probe: once the window has drawn on the first output, the first output goes off.
pub fn drive(proxy: Proxy, app_log: &Path, outputs: OutputControl) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    outputs.only(1).map_err(|e| format!("{e:#}"))?;
    // the relaunched (second) instance ran its frame callbacks: the move's configuration change is over
    let start = Instant::now();
    while std::fs::read_to_string(app_log).map_or(0, |log| log.matches(LAST_FRAME).count()) < 2 {
        if start.elapsed() > RELAUNCH_WAIT {
            return Err("the relaunched instance never ran its frames".to_owned());
        }
        std::thread::sleep(INPUT_SETTLE);
    }
    outputs
        .set_transform(1, ROTATED_TRANSFORM)
        .map_err(|e| format!("{e:#}"))?;
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
pub fn checks(
    app: &str,
    services: &str,
    liveness: &Result<Liveness, String>,
    opened: DisplayModel,
    moved: DisplayModel,
) -> Vec<Sub> {
    vec![
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
            ),
        ),
        ("density", density_check(app, opened.dpi, moved.dpi)),
        ("relaunch", relaunch_check(app, services)),
        ("frames", frames_check(app)),
    ]
}

/// The displays the services took, in order.
fn displays(services: &str) -> Vec<DisplayModel> {
    services
        .lines()
        .filter_map(|l| l.strip_prefix(DISPLAY_LINE))
        .filter_map(|rest| DisplayModel::parse_arg(rest.split_whitespace().next()?).ok())
        .collect()
}

/// The displays the services took: `moved`'s density, rate and rotation, then the same at the rotated output's rotation (`rotated`, [`ROTATED_TRANSFORM`]: Output::display derives it from wayland.xml).
/// The size is the window's monitor as GDK reports it (logical size x scale), which wl_fixed rounding of 1.6 moves off the mode by a few pixels, so the size is shown, not judged.
fn display_check(
    services: &str,
    moved: DisplayModel,
    rotated: DisplayModel,
) -> Result<String, String> {
    let all = displays(services);
    let same = |d: &DisplayModel, w: &DisplayModel| {
        d.dpi == w.dpi && d.refresh_hz == w.refresh_hz && d.rotation == w.rotation
    };
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

    #[test]
    fn density_reads_first_and_last() {
        let app = "AtlWindow: density created config=160 metrics=160\nAtlWindow: density created config=256 metrics=256\n";
        assert!(density_check(app, 160, 256).is_ok());
        let stale = "AtlWindow: density created config=160 metrics=160\nAtlWindow: density created config=256 metrics=160\n";
        assert!(density_check(stale, 160, 256).is_err());
    }
}
