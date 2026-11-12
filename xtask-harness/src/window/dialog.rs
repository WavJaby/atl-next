// SPDX-License-Identifier: MIT
//! Stage `dialog` of `xtask test window`: a dimming dialog with surfaceInsets and a popup near the window's bottom (xtask-harness/testdata/dialog, Main.java), software windows, UI mode at scale 1.
//! The picture is worked out here from the fixture's spec (its colours and sizes, the window size and anchor rectangle it logs), never from the code under test:
//! - popup: PopupWindow.showAsDropDown finds no room below the anchor inside the display frame (= the Task, WindowLayout.java:86-87), so it opens above it: left = anchor left, bottom = anchor top
//!   (PopupWindow.tryFitVertical, PopupWindow.java:1828-1834);
//! - dialog: centred (gravity 0, Gravity.apply), its content at the frame; the surface is surfaceInsets larger on each side and placed that far up-left (WindowState.transformFrameToSurfacePosition),
//!   the inset ring transparent (software draw clears it, ViewRootImpl.drawSoftware);
//! - dim: every pixel below the dialog times (1 - dimAmount): a black colour layer at alpha dimAmount over the Task, just below the dialog (Dimmer.java:169-173, DimmerAnimationHelper.java:257, :286-293).
//!
//! Sub-gates ([`checks`]): shown (the dialog drew), readback (the last settled readback is that picture), pong, controls (no dim / surface origin without the insets / popup below the anchor each give
//! another picture, so each would fail the readback), touch (real wl_pointer drags on the off-origin dialog, before and after a resize with it open, reach it in its own coordinates: InputDispatcher's
//! per-window transform, raw coordinates staying the Task's; `--break-input-frames` must fail it). The readback is the picture at the resized size.
//! Back is the app's first (owner 2026-11-12; ActivityClientController.onBackPressed:1720-1747 decides only what the app left unhandled):
//! back (Main starts Sub after the first draw; Sub leaves its Back unhandled, so the services' requestFinish finishes it, Main resumes and the services remove Sub's window; `--break-back-finish` must fail it);
//! consumed (after the resize, Main's first Back is consumed by its OnBackInvokedCallback: logged, no move to the back for it); minimize (Main's second Back is unhandled on the launcher root: the services
//! move the Task back once, the window sends `xdg_toplevel.set_minimized`, Main is not destroyed and the app is still running at the watch's end; `--break-root-minimize` (policy close) must fail it).

use std::path::Path;
use std::time::Duration;

use std::sync::Mutex;

use super::capture;
use super::widgets::{BTN_SIDE, inject, press, wait_until};
use super::{
    ACK_WAIT, Liveness, PONG_WAIT, RESIZE_SETTLE, Sub, await_event, content_origin, wait_for_line,
};
use crate::composer::{fnv1a64, word};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/dialog";
/// The marker (Main.java onResume).
pub const MARKER: &str = "AtlWindow: onResume reached";
/// The fixture's line after the dialog's first draw: window size and anchor rectangle.
const SHOWN: &str = "AtlWindow: dialog shown ";
/// Main.java's colours (ARGB ints there), as RGBA bytes, and its sizes in pixels.
const WINDOW: [u8; 4] = [0x20, 0x40, 0x60, 0xff];
const ANCHOR: [u8; 4] = [0x60, 0x60, 0x20, 0xff];
const POPUP: [u8; 4] = [0x80, 0x40, 0x00, 0xff];
const DIALOG: [u8; 4] = [0xc0, 0x80, 0x20, 0xff];
const DIALOG_SIZE: (u32, u32) = (240, 120);
const INSET: u32 = 16;
const POPUP_SIZE: (u32, u32) = (160, 80);
/// Main.java DIM = 0.5: each channel under the dim is halved. Main.java keeps every dimmed channel even and at most 0x80, so the halving is exact whether the compositor's alpha is 0.5
/// or the 8-bit 128/255 (0x80 * (1 - 128/255) = 63.75 rounds to 64).
const DIM_DIVISOR: u8 = 2;
/// The fixture's line per touch on the dialog.
const TOUCH: &str = "AtlWindow: dialog touch ";
/// The back case's lines: Sub.java's onResume / onDestroy, Main.java's onDestroy and its callback's consumed Back (MARKER is Main's onResume).
const SUB_RESUMED: &str = "AtlWindow: sub onResume reached";
const SUB_DESTROYED: &str = "AtlWindow: sub onDestroy reached";
const DESTROYED: &str = "AtlWindow: onDestroy reached";
const CONSUMED: &str = "AtlWindow: back consumed";
/// The services' line per move to the back (activity.rs ActivityClientController::task_to_back).
const TASK_TO_BACK: &str = "services: onBackPressed: moveTaskToBack uid=";
/// Each back wait polls this long for its line or event; a break leaves it unmet and the verdict names what is missing.
const BACK_WAIT: Duration = Duration::from_secs(5);
/// A drag starts at the dialog's centre and ends this far from it: inside the dialog, so a window-relative event stays on it.
const DRAG: (u32, u32) = (30, 20);
/// The configure with the dialog open: xdg_toplevel size and an injected serial far from the compositor's. Smaller than the gates' opening size, so the dialog moves and the window stays under the display.
const RESIZED: (i32, i32, u32) = (1000, 700, 0xd1a1);
/// MotionEvent ACTION_DOWN / ACTION_UP / ACTION_MOVE.
const ACTION_DOWN: u32 = 0;
const ACTION_UP: u32 = 1;
const ACTION_MOVE: u32 = 2;

/// `l,t,r,b` in window pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Rect {
    fn sized(left: u32, top: u32, (w, h): (u32, u32)) -> Rect {
        Rect {
            left,
            top,
            right: left + w,
            bottom: top + h,
        }
    }

    fn contains(&self, x: u32, y: u32) -> bool {
        (self.left..self.right).contains(&x) && (self.top..self.bottom).contains(&y)
    }
}

/// What the fixture logged.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: u32,
    height: u32,
    anchor: Rect,
}

/// The window's last layout.
fn layout(app: &str) -> Result<Layout, String> {
    parse_layout(
        app.lines()
            .rfind(|l| l.contains(SHOWN))
            .ok_or("the dialog never drew")?,
    )
}

fn parse_layout(line: &str) -> Result<Layout, String> {
    let anchor: Vec<u32> = word(line, "anchor")
        .map(|s| s.split(',').filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default();
    let [left, top, right, bottom] = anchor[..] else {
        return Err(format!("no anchor= in {line}"));
    };
    let (width, height) = word(line, "window")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or_else(|| format!("no window= in {line}"))?;
    Ok(Layout {
        width,
        height,
        anchor: Rect {
            left,
            top,
            right,
            bottom,
        },
    })
}

/// Which rules the expected picture follows; the controls turn one off.
#[derive(Clone, Copy)]
struct Rules {
    /// the dim layer below the dialog
    dim: bool,
    /// the dialog's surface placed up-left of its frame by the insets (off: at the frame, so the content shows INSET right and down)
    surface_insets: bool,
    /// the popup flips above the anchor (off: opens below it, as with a display frame larger than the Task)
    popup_inside: bool,
}

const ALL: Rules = Rules {
    dim: true,
    surface_insets: true,
    popup_inside: true,
};

/// The picture of `l` under `rules`, RGBA rows of the window's width, top first.
fn picture(l: &Layout, rules: Rules) -> Vec<u8> {
    let popup_top = if rules.popup_inside {
        l.anchor.top - POPUP_SIZE.1
    } else {
        l.anchor.bottom
    };
    let popup = Rect::sized(l.anchor.left, popup_top, POPUP_SIZE);
    // Gravity.apply centre: (parent - size) / 2, integer division as in Gravity.java
    let frame = Rect::sized(
        (l.width - DIALOG_SIZE.0) / 2,
        (l.height - DIALOG_SIZE.1) / 2,
        DIALOG_SIZE,
    );
    let shift = if rules.surface_insets { 0 } else { INSET };
    let content = Rect::sized(frame.left + shift, frame.top + shift, DIALOG_SIZE);
    let mut out = Vec::with_capacity((l.width * l.height * 4) as usize);
    for y in 0..l.height {
        for x in 0..l.width {
            let px = if content.contains(x, y) {
                DIALOG
            } else {
                let below = if popup.contains(x, y) {
                    POPUP
                } else if l.anchor.contains(x, y) {
                    ANCHOR
                } else {
                    WINDOW
                };
                if rules.dim {
                    [
                        below[0] / DIM_DIVISOR,
                        below[1] / DIM_DIVISOR,
                        below[2] / DIM_DIVISOR,
                        below[3],
                    ]
                } else {
                    below
                }
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

/// The picture drive takes before Main's Back, its size and digest against [`picture`]. The back-close case closes Main last, so the picture comes before its onDestroy.
fn readback_check(pictures: &Mutex<capture::Seen>, app: &str) -> Result<String, String> {
    let l = layout(app)?;
    let shot = capture::picture(pictures, 0)?;
    let size = format!("{}x{}", shot.size.0, shot.size.1);
    let want_size = format!("{}x{}", l.width, l.height);
    if size != want_size {
        return Err(format!("the picture is {size}, the window {want_size}"));
    }
    let got = format!("{:016x}", shot.digest);
    let want = format!("{:016x}", fnv1a64(&picture(&l, ALL)));
    if got != want {
        return Err(format!("picture {got}, the picture {want}"));
    }
    Ok(format!("picture {want_size} = the picture ({want})"))
}

/// Each picture with one rule broken differs from the real one; with the real readback passing, each would fail it.
fn controls(app: &str) -> Result<String, String> {
    let l = layout(app)?;
    let real = fnv1a64(&picture(&l, ALL));
    let broken = [
        ("no dim", Rules { dim: false, ..ALL }),
        (
            "surface origin without the insets",
            Rules {
                surface_insets: false,
                ..ALL
            },
        ),
        (
            "popup below the anchor",
            Rules {
                popup_inside: false,
                ..ALL
            },
        ),
    ];
    let same: Vec<&str> = broken
        .iter()
        .filter(|(_, r)| fnv1a64(&picture(&l, *r)) == real)
        .map(|(n, _)| *n)
        .collect();
    if same.is_empty() {
        Ok("no dim / origin without insets / popup below each change the picture".to_owned())
    } else {
        Err(format!("broken rules {same:?} give the real picture"))
    }
}

/// The dialog's frame origin in a window of `l` (Gravity.apply centre, as [`picture`]).
fn dialog_origin(l: &Layout) -> (u32, u32) {
    (
        (l.width - DIALOG_SIZE.0) / 2,
        (l.height - DIALOG_SIZE.1) / 2,
    )
}

/// One logged touch: action, the dialog's x and y, raw x and y.
fn touches(app: &str) -> Vec<(u32, [f32; 4])> {
    app.lines()
        .filter(|l| l.contains(TOUCH))
        .filter_map(|l| {
            let f = |k| word(l, k)?.parse::<f32>().ok();
            Some((
                word(l, "action")?.parse().ok()?,
                [f("x")?, f("y")?, f("rawx")?, f("rawy")?],
            ))
        })
        .collect()
}

/// Two drags, down at the dialog's centre, up DRAG from it, each in the dialog's coordinates and raw = its frame origin plus them; the first at the first layout, the second at the last.
fn touch_check(app: &str) -> Result<String, String> {
    let first = parse_layout(
        app.lines()
            .find(|l| l.contains(SHOWN))
            .ok_or("the dialog never drew")?,
    )?;
    let last = layout(app)?;
    if (first.width, first.height) == (last.width, last.height) {
        return Err(format!(
            "the window never resized from {}x{}",
            last.width, last.height
        ));
    }
    let all = touches(app);
    let mut gestures = all.split_inclusive(|(action, _)| *action == ACTION_UP);
    for (name, l) in [("first", &first), ("resized", &last)] {
        let g = gestures
            .next()
            .ok_or_else(|| format!("no {name} drag reached the dialog"))?;
        let (ox, oy) = dialog_origin(l);
        let (cx, cy) = (DIALOG_SIZE.0 / 2, DIALOG_SIZE.1 / 2);
        let want = |action, x: u32, y: u32| {
            (
                action,
                [x as f32, y as f32, (ox + x) as f32, (oy + y) as f32],
            )
        };
        let (down, up) = (g[0], g[g.len() - 1]);
        if down != want(ACTION_DOWN, cx, cy) {
            return Err(format!(
                "{name} down {down:?}, want {:?}",
                want(ACTION_DOWN, cx, cy)
            ));
        }
        if up != want(ACTION_UP, cx + DRAG.0, cy + DRAG.1) {
            return Err(format!(
                "{name} up {up:?}, want {:?}",
                want(ACTION_UP, cx + DRAG.0, cy + DRAG.1)
            ));
        }
        if g[1..g.len() - 1].iter().any(|(a, _)| *a != ACTION_MOVE) || g.len() < 3 {
            return Err(format!(
                "{name} drag has no moves between down and up: {g:?}"
            ));
        }
    }
    Ok(format!(
        "{} touches: both drags in the dialog's coordinates, raw in the Task's",
        all.len()
    ))
}

/// The sub-gates. `services` is the services log: the back case reads the window removals and the moves to the back there. `ended`: the app or atld ended before the watch did.
pub fn checks(
    app: &str,
    services: &str,
    liveness: &Result<Liveness, String>,
    pictures: &Mutex<capture::Seen>,
    ended: bool,
) -> Vec<Sub> {
    let minimized = liveness.as_ref().is_ok_and(|l| l.minimized);
    vec![
        // first: the probe's first step, and a break of it leaves Sub on top for everything after
        ("back", back_check(app, services)),
        ("consumed", consumed_check(app, services)),
        ("minimize", minimize_check(app, services, minimized, ended)),
        (
            "shown",
            layout(app).map(|l| format!("window {}x{}, anchor {:?}", l.width, l.height, l.anchor)),
        ),
        ("readback", readback_check(pictures, app)),
        (
            "pong",
            match liveness {
                Ok(l) if l.answered => Ok("the window answered a ping".to_owned()),
                Ok(_) => Err("no pong".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
        ("touch", touch_check(app)),
        ("relayoutAsync", super::relayout_async_check(services)),
        ("controls", controls(app)),
    ]
}

/// The services removed the window of the activity whose class ends with `title`.
fn removed(services: &str, title: &str) -> bool {
    services
        .lines()
        .any(|l| l.contains("window ") && l.ends_with(&format!("{title} removed")))
}

/// The back verdict: Sub's unhandled Back finished Sub (Main resumed before Sub's onDestroy, as AOSP resumes the activity below first) and the services removed Sub's window.
/// A services break (`--no-back-finish`: no requestFinish) misses the sub onDestroy.
fn back_check(app: &str, services: &str) -> Result<String, String> {
    let started = app
        .find(SUB_RESUMED)
        .ok_or("Main never started Sub (no sub onResume)")?;
    let sub = app[started..]
        .find(SUB_DESTROYED)
        .map(|i| started + i)
        .ok_or("Sub's Back never finished Sub (no sub onDestroy)")?;
    if !app[started..sub].contains(MARKER) {
        return Err("Main did not resume when Sub's Back popped it".to_owned());
    }
    if !removed(services, ".Sub") {
        return Err("the services removed no Sub window".to_owned());
    }
    Ok(
        "Sub's unhandled Back finished Sub, Main resumed, the services removed Sub's window"
            .to_owned(),
    )
}

/// The consumed verdict: Main's callback logged its Back, and that Back reached no system default: the services moved the Task back once at most (the second, unhandled Back's), and Main lives on.
fn consumed_check(app: &str, services: &str) -> Result<String, String> {
    app.find(CONSUMED)
        .ok_or("Main's OnBackInvokedCallback never got its Back (no back consumed)")?;
    let moves = services.matches(TASK_TO_BACK).count();
    if moves > 1 {
        return Err(format!(
            "{moves} moves to the back: the consumed Back reached the services too"
        ));
    }
    if app.contains(DESTROYED) {
        return Err("Main was destroyed: a Back finished it".to_owned());
    }
    Ok("the app's callback consumed Main's first Back; no system default ran for it".to_owned())
}

/// The minimize verdict: Main's unhandled Back on the launcher root moved the Task back once, the window asked to be minimized, Main was not destroyed or its window removed, and the app outlived
/// the watch. The close policy (`--root-back close`) finishes Main instead: no move, no set_minimized, onDestroy, and the app ends with its last window.
fn minimize_check(
    app: &str,
    services: &str,
    minimized: bool,
    ended: bool,
) -> Result<String, String> {
    let moves = services.matches(TASK_TO_BACK).count();
    if moves != 1 {
        return Err(format!(
            "{moves} moves to the back for Main's unhandled Back, want 1"
        ));
    }
    if !minimized {
        return Err("the window sent no xdg_toplevel.set_minimized".to_owned());
    }
    if app.contains(DESTROYED) || removed(services, ".Main") {
        return Err(
            "Main was finished (onDestroy or its window removed), not moved back".to_owned(),
        );
    }
    if ended {
        return Err("the app ended before the watch did".to_owned());
    }
    Ok(
        "Main's unhandled Back moved the Task back: window minimized, Main alive, app running"
            .to_owned(),
    )
}

/// A drag on the dialog of the log's last layout: pointer in at its centre, press, move DRAG in two steps, release.
fn drag(proxy: &Proxy, app_log: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = content_origin(&text)?;
    let (fx, fy) = dialog_origin(&layout(&text)?);
    let x = f64::from(ox) + f64::from(fx + DIALOG_SIZE.0 / 2);
    let y = f64::from(oy) + f64::from(fy + DIALOG_SIZE.1 / 2);
    let (dx, dy) = (f64::from(DRAG.0), f64::from(DRAG.1));
    inject(
        proxy,
        vec![
            PointerAction::Enter { x, y },
            press(true),
            PointerAction::Motion {
                x: x + dx / 2.0,
                y: y + dy / 2.0,
            },
            PointerAction::Motion {
                x: x + dx,
                y: y + dy,
            },
            press(false),
        ],
    )
}

/// The anchor's centre in surface pixels, from the last layout line.
fn anchor_centre(app_log: &Path) -> Result<(f64, f64), String> {
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = content_origin(&text)?;
    let a = layout(&text)?.anchor;
    Ok((
        f64::from(ox) + f64::from((a.left + a.right) / 2),
        f64::from(oy) + f64::from((a.top + a.bottom) / 2),
    ))
}

/// BTN_SIDE down and up over (x, y): KEYCODE_BACK from the mouse (widgets' buttons check).
fn back(proxy: &Proxy, x: f64, y: f64) -> Result<(), String> {
    let button = |pressed| PointerAction::Button {
        button: BTN_SIDE,
        pressed,
    };
    inject(
        proxy,
        vec![PointerAction::Enter { x, y }, button(true), button(false)],
    )
}

/// The back case, first step: Main started Sub after the dialog's first draw; Sub's unhandled Back pops to Main, which resumes and stays.
/// The dialog is touch-modal, so no touch reaches the anchor (a click there lands in the dialog); the Back is a mouse button, which goes to the focused window.
/// Waits only; [`back_check`] judges the lines, so a break fails there, not here.
fn sub_back(proxy: &Proxy, app_log: &Path) -> Result<(), String> {
    wait_until(app_log, BACK_WAIT, |t| t.contains(SUB_RESUMED));
    let (x, y) = anchor_centre(app_log)?;
    back(proxy, x, y)?;
    wait_until(app_log, BACK_WAIT, |t| t.contains(SUB_DESTROYED));
    Ok(())
}

/// The back case, last steps: Main's first Back (its callback consumes it, [`consumed_check`]), then its second, unhandled one. Returns whether the window then sent `set_minimized`
/// ([`minimize_check`] judges it with the lines).
fn root_backs(proxy: &Proxy, app_log: &Path) -> Result<bool, String> {
    let (x, y) = anchor_centre(app_log)?;
    back(proxy, x, y)?;
    wait_until(app_log, BACK_WAIT, |t| t.contains(CONSUMED));
    back(proxy, x, y)?;
    Ok(await_event(&proxy.events, BACK_WAIT, |e| *e == ProxyEvent::Minimized).is_some())
}

/// The probe: the window's configure is acked, the dialog drew, one ping, a drag on the dialog; the window is resized with it open, and a second drag.
/// The back case's Sub Back comes first, before any other input (Sub covers the dialog until it pops); Main's Backs come last, since the window minimizes with the second.
pub fn drive(proxy: Proxy, app_log: &Path, probe: &capture::Probe) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, SHOWN, ACK_WAIT)
        .then_some(())
        .ok_or("the dialog never drew")?;
    sub_back(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    drag(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    let (width, height, serial) = RESIZED;
    proxy
        .inject_configure(width, height, serial, &[])
        .map_err(|e| e.to_string())?;
    await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Acked { serial }
    })
    .ok_or("the resize was never acked")?;
    std::thread::sleep(RESIZE_SETTLE);
    drag(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    // the picture of the dialog as the last drag left it, before Main's Backs minimize the window
    probe.record_settled();
    let minimized = root_backs(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    Ok(Liveness {
        answered,
        minimized,
        ..Liveness::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 400x300 window, the anchor 40,252..160,292 (Main.java: 120x40, left 40, bottom margin 8).
    const LINE: &str = "1.0 I AtlWindow: dialog shown window=400x300 anchor=40,252,160,292\n";

    fn at(p: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * 400 + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    /// The picture by hand: dialog frame ((400-240)/2, (300-120)/2) = 80,90..320,210 undimmed; the popup 40,172..200,252 above the anchor; everything else halved.
    #[test]
    fn picture_by_hand() {
        let l = layout(LINE).expect("layout");
        let p = picture(&l, ALL);
        assert_eq!(at(&p, 80, 90), DIALOG);
        assert_eq!(at(&p, 319, 209), DIALOG);
        assert_eq!(at(&p, 79, 90), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(at(&p, 320, 209), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(at(&p, 40, 251), [0x40, 0x20, 0x00, 0xff]);
        assert_eq!(at(&p, 40, 171), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(at(&p, 199, 251), [0x40, 0x20, 0x00, 0xff]);
        assert_eq!(at(&p, 200, 251), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(at(&p, 40, 252), [0x30, 0x30, 0x10, 0xff]);
        assert!(controls(LINE).is_ok());
    }

    /// The fixture's lines in the back case: Main resumes when Sub's Back pops it, before Sub's onDestroy (AOSP resumes the activity below first); Main's callback consumes its first Back.
    const BACK: &str = "1.0 I AtlWindow: onResume reached\n1.1 I AtlWindow: sub onResume reached\n1.2 I AtlWindow: onResume reached\n1.3 I AtlWindow: sub onDestroy reached\n1.4 I AtlWindow: back consumed\n";
    /// The services' lines: Sub's window removed (its Back), then one move to the back (Main's unhandled Back).
    const SERVICES: &str = "services: window com.example.atldialog/com.example.atldialog.Sub removed\nservices: onBackPressed: moveTaskToBack uid=10001\n";
    /// The close policy's services lines: no move, Main's window removed.
    const CLOSED: &str = "services: window com.example.atldialog/com.example.atldialog.Sub removed\nservices: window com.example.atldialog/com.example.atldialog.Main removed\n";

    #[test]
    fn back_case_passes_on_the_fixture_lines() {
        assert!(back_check(BACK, SERVICES).is_ok());
        assert!(consumed_check(BACK, SERVICES).is_ok());
        assert!(minimize_check(BACK, SERVICES, true, false).is_ok());
    }

    #[test]
    fn back_needs_sub_finished_and_main_resumed() {
        // --no-back-finish: no requestFinish, so Sub never destroys
        let no_finish =
            "1.0 I AtlWindow: onResume reached\n1.1 I AtlWindow: sub onResume reached\n";
        assert!(
            back_check(no_finish, SERVICES)
                .unwrap_err()
                .contains("no sub onDestroy")
        );
        // Sub's Back took Main down with it: no Main resume between Sub's onResume and onDestroy
        let no_resume = "1.0 I AtlWindow: onResume reached\n1.1 I AtlWindow: sub onResume reached\n1.2 I AtlWindow: sub onDestroy reached\n";
        assert!(
            back_check(no_resume, SERVICES)
                .unwrap_err()
                .contains("did not resume")
        );
        let no_removal = "services: onBackPressed: moveTaskToBack uid=10001\n";
        assert!(
            back_check(BACK, no_removal)
                .unwrap_err()
                .contains("no Sub window")
        );
    }

    #[test]
    fn consumed_needs_the_callback_and_no_extra_default() {
        let unconsumed = BACK.replace("back consumed", "something else");
        assert!(
            consumed_check(&unconsumed, SERVICES)
                .unwrap_err()
                .contains("no back consumed")
        );
        // both of Main's Backs reached the services
        let twice = format!("{SERVICES}services: onBackPressed: moveTaskToBack uid=10001\n");
        assert!(
            consumed_check(BACK, &twice)
                .unwrap_err()
                .contains("2 moves")
        );
    }

    /// `--root-back close`: Main finishes on its unhandled Back, the window closes and the app ends with it; every condition of the minimize check fails on that run's lines.
    #[test]
    fn minimize_fails_under_the_close_policy() {
        let destroyed = format!("{BACK}1.5 I AtlWindow: onDestroy reached\n");
        assert!(
            minimize_check(&destroyed, CLOSED, false, true)
                .unwrap_err()
                .contains("0 moves")
        );
        assert!(
            minimize_check(BACK, SERVICES, false, false)
                .unwrap_err()
                .contains("set_minimized")
        );
        assert!(
            minimize_check(&destroyed, SERVICES, true, false)
                .unwrap_err()
                .contains("finished")
        );
        assert!(
            minimize_check(BACK, SERVICES, true, true)
                .unwrap_err()
                .contains("ended")
        );
    }
}
