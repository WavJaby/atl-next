// SPDX-License-Identifier: MIT
//! Stage `gles2` of `xtask test window` (GS2.5): the GLES2 fixture (xtask/testdata/gles2, Main.java) in UI mode on the gate's compositor, its `.so` a guest library through the native bridge, so every GL call crosses it and the frame reaches the window's picture ([`capture::Probe`]).
//! Sub-gates ([`checks`]): `probe` (the window acked, the fixture read back, the picture settled), `readback` (the app's glReadPixels at each probe is the expectation), `composed` (the picture at each probe is the expectation), `present` (no failed present), `balance` (the bridge's exit accounting).
//! The expectation is [`gles2_fixture::PROBES`], worked out from the scene in Main.java. GL's window origin is bottom-left, the picture's top-left: probe (x, y) is in the picture at row `height - 1 - y`, `height` the window's opening height (the picture's size, checked).

use std::path::Path;
use std::sync::Mutex;

use atl_services_types::display_model::DisplayModel;

use super::capture;
use super::{ACK_WAIT, Liveness, Sub, await_event, opening_size, wait_for_line};
use crate::gles2_fixture::PROBES;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The marker: the fixture's surface line (Main.java onSurfaceChanged), logged once the GL surface exists.
pub const MARKER: &str = "AtlGles2: surface ";
/// Main.java's readback lines: `readback pass`, or one `readback FAIL` per wrong probe.
const READBACK_LINE: &str = "AtlGles2: readback";
const READBACK_PASS: &str = "AtlGles2: readback pass";
const READBACK_FAIL: &str = "AtlGles2: readback FAIL";
/// The composer's failed present (atl-composer ui_window.rs).
const PRESENT_WARN: &str = "atl-composer: ui present:";
/// The bridge's exit line (atl-native-bridge engine/thread.rs `detach`): `attached A, detached D`.
const DETACH_LINE: &str = "atl-native-bridge: thread detached (attached ";
/// Per-channel slack of Main.java's `matches` (`SLACK`): the clear colour goes float -> unorm8.
const SLACK: u32 = 1;

/// Row `y` of GL's bottom-left origin in a window `height` tall, as the picture's top-left row; None when `y` is outside.
fn composed_row(height: u32, y: u32) -> Option<u32> {
    height.checked_sub(1)?.checked_sub(y)
}

/// The probe: the window is acked, the fixture has read back, and its picture has settled (after the app's last present).
pub fn drive(proxy: Proxy, app_log: &Path, probe: &capture::Probe) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, READBACK_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never read back")?;
    probe.record_settled();
    Ok(Liveness::default())
}

/// The sub-gates; `display` gives the window's opening size, the size the picture must have.
pub fn checks(
    app: &str,
    display: DisplayModel,
    liveness: &Result<Liveness, String>,
    seen: &Mutex<capture::Seen>,
) -> Vec<Sub> {
    let (width, height) = opening_size(display);
    vec![
        (
            "probe",
            match liveness {
                Ok(_) => Ok("acked, read back, captured".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
        ("readback", readback_check(app)),
        ("composed", composed_check(seen, width, height)),
        ("present", present_check(app)),
        ("balance", balance_check(app)),
    ]
}

/// The app's own glReadPixels at each probe: its `readback pass` line, and no `readback FAIL` line.
fn readback_check(app: &str) -> Result<String, String> {
    let pass = app.contains(READBACK_PASS);
    let fails: Vec<&str> = app.lines().filter(|l| l.contains(READBACK_FAIL)).collect();
    match (pass, fails.first()) {
        (true, None) => Ok(format!("{} probes read back as PROBES", PROBES.len())),
        (_, first) => Err(format!(
            "`{READBACK_PASS}` logged: {pass}; {} FAIL lines, first: {}",
            fails.len(),
            first.copied().unwrap_or("none")
        )),
    }
}

/// The RRGGBBAA of the picture's pixel at column `x`, row `y` (top-left origin); the caller keeps both inside the picture.
fn pixel(shot: &capture::Shot, x: u32, y: u32) -> u32 {
    let i = (y as usize * shot.size.0 as usize + x as usize) * 4;
    u32::from_be_bytes([
        shot.rgba[i],
        shot.rgba[i + 1],
        shot.rgba[i + 2],
        shot.rgba[i + 3],
    ])
}

/// Each channel within [`SLACK`] of the expectation.
fn same(got: u32, want: u32) -> bool {
    [0, 8, 16, 24]
        .into_iter()
        .all(|shift| ((got >> shift) & 0xff).abs_diff((want >> shift) & 0xff) <= SLACK)
}

/// The window's picture at each probe: the settled picture, at the window's opening size, holds PROBES at their composed rows, each channel within [`SLACK`].
fn composed_check(seen: &Mutex<capture::Seen>, width: u32, height: u32) -> Result<String, String> {
    let shot = capture::picture(seen, 0)?;
    if shot.size != (width, height) {
        return Err(format!(
            "the picture is {}x{}, the window {width}x{height}",
            shot.size.0, shot.size.1
        ));
    }
    let mut fails = Vec::new();
    for (x, y, want) in PROBES {
        let Some(row) = composed_row(height, y).filter(|_| x < width) else {
            fails.push(format!("({x},{y}) outside the window"));
            continue;
        };
        let got = pixel(&shot, x, row);
        if !same(got, want) {
            fails.push(format!("({x},{y}) composed {got:08x}, want {want:08x}"));
        }
    }
    if fails.is_empty() {
        Ok(format!(
            "the picture ({width}x{height}) = PROBES at {} points",
            PROBES.len()
        ))
    } else {
        Err(fails.join("; "))
    }
}

/// No failed present in the app's log.
fn present_check(app: &str) -> Result<String, String> {
    if app.contains(PRESENT_WARN) {
        Err(format!("`{PRESENT_WARN}` logged"))
    } else {
        Ok("no failed present".to_owned())
    }
}

/// `(attached, detached)` of an exit line.
fn exit_counts(line: &str) -> Option<(u32, u32)> {
    let (_, rest) = line.split_once(DETACH_LINE)?;
    let (a, d) = rest
        .trim_end()
        .strip_suffix(')')?
        .split_once(", detached ")?;
    Some((a.parse().ok()?, d.parse().ok()?))
}

/// Thread balance: every thread exit leaves the main thread attached, so attached > detached on each exit line. The window's GL thread lives to the end of the run, so a run with no exit has nothing to balance: it passes, and says so.
fn balance_check(app: &str) -> Result<String, String> {
    let exits: Vec<(u32, u32)> = app.lines().filter_map(exit_counts).collect();
    if let Some(&(a, d)) = exits.iter().find(|&&(a, d)| a <= d) {
        return Err(format!(
            "an exit leaves attached {a}, detached {d}: the main thread is not attached"
        ));
    }
    match exits.last() {
        Some(&(a, d)) => Ok(format!("the last exit leaves attached {a}, detached {d}")),
        None => Ok("no thread exited in the run (the GL thread lives on)".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window's size at the gate's display (opening size: 3/4 of 1280x800).
    const WIDTH: u32 = 960;
    const HEIGHT: u32 = 600;

    /// The probe's record of a picture of `WIDTH`x`height` with `pixels` (`(x, y, RRGGBBAA)` in the picture's rows) and black elsewhere.
    fn seen_with(height: u32, pixels: &[(u32, u32, u32)]) -> Mutex<capture::Seen> {
        let mut rgba = vec![0u8; (WIDTH * height * 4) as usize];
        for &(x, y, word) in pixels {
            let i = ((y * WIDTH + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&word.to_be_bytes());
        }
        let shot = capture::Shot {
            size: (WIDTH, height),
            digest: 0,
            rgba,
        };
        Mutex::new(capture::Seen {
            pictures: vec![Ok(shot)],
            ..Default::default()
        })
    }

    /// PROBES at the rows the picture draws them on.
    fn composed_probes() -> Vec<(u32, u32, u32)> {
        PROBES
            .iter()
            .map(|&(x, y, want)| (x, composed_row(HEIGHT, y).expect("probe inside"), want))
            .collect()
    }

    #[test]
    fn gl_rows_flip_onto_the_picture_rows() {
        assert_eq!(composed_row(HEIGHT, 8), Some(591));
        assert_eq!(composed_row(HEIGHT, 599), Some(0));
        assert_eq!(composed_row(HEIGHT, 600), None);
        assert_eq!(composed_row(0, 0), None);
    }

    #[test]
    fn composed_probes_pass_and_a_wrong_one_is_named() {
        assert!(composed_check(&seen_with(HEIGHT, &composed_probes()), WIDTH, HEIGHT).is_ok());
        let mut bad = composed_probes();
        bad[0].2 = 0x00ff_00ff;
        let err =
            composed_check(&seen_with(HEIGHT, &bad), WIDTH, HEIGHT).expect_err("a wrong pixel");
        assert!(
            err.contains("(8,8) composed 00ff00ff, want ff0000ff"),
            "{err}"
        );
    }

    /// Control: the probes at their GL rows, not the picture's, are not where the picture draws them, so the gate rejects them.
    #[test]
    fn unflipped_rows_are_rejected() {
        let unflipped: Vec<(u32, u32, u32)> = PROBES.to_vec();
        assert!(composed_check(&seen_with(HEIGHT, &unflipped), WIDTH, HEIGHT).is_err());
    }

    #[test]
    fn a_picture_of_another_size_is_named() {
        let err =
            composed_check(&seen_with(HEIGHT, &composed_probes()), 1280, 800).expect_err("a size");
        assert!(err.contains("the picture is 960x600"), "{err}");
    }

    #[test]
    fn slack_is_one_channel_step() {
        assert!(same(0xff00_00ff, 0xfe00_00ff));
        assert!(!same(0xff00_00ff, 0xfd00_00ff));
    }

    #[test]
    fn readback_needs_the_pass_line_and_no_fail() {
        assert!(readback_check("AtlGles2: readback pass\n").is_ok());
        assert!(readback_check("").is_err());
        let err = readback_check("AtlGles2: readback FAIL (8,8): got 00 want ff\n")
            .expect_err("a fail line");
        assert!(err.contains("1 FAIL lines"), "{err}");
    }

    #[test]
    fn a_failed_present_fails() {
        assert!(present_check("1.0 D atl-composer: ui: present n=1\n").is_ok());
        assert!(present_check("1.0 W atl-composer: ui present: boom\n").is_err());
    }

    #[test]
    fn balance_judges_the_exits_it_sees() {
        let none = balance_check("1.0 I AtlGles2: surface 960x600\n").expect("no exit");
        assert!(none.contains("no thread exited"), "{none}");
        let one = "atl-native-bridge: thread detached (attached 3, detached 1)\n";
        assert!(balance_check(one).is_ok());
        let broken = "atl-native-bridge: thread detached (attached 2, detached 2)\n";
        assert!(balance_check(broken).is_err());
    }
}
