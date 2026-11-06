// SPDX-License-Identifier: MIT
//! Stage `dialog` of `xtask test window`: a dimming dialog with surfaceInsets and a popup near the window's bottom (xtask/testdata/dialog, Main.java), software windows, UI mode at scale 1.
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

use std::path::Path;

use super::widgets::{inject, press};
use super::{
    ACK_WAIT, Liveness, PONG_WAIT, RESIZE_SETTLE, Sub, await_event, capture_line, content_origin,
    wait_for_line,
};
use crate::composer::{fnv1a64, word};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask/testdata/dialog";
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

/// The last readback's size and digest against [`picture`].
fn readback_check(app: &str) -> Result<String, String> {
    let l = layout(app)?;
    capture_line(app)?;
    let line = app
        .lines()
        .rfind(|line| line.contains(super::widgets::CAPTURE_LINE))
        .ok_or("no readback")?;
    let size = word(line, "size").unwrap_or("none");
    let want_size = format!("{}x{}", l.width, l.height);
    if size != want_size {
        return Err(format!("last readback is {size}, the window {want_size}"));
    }
    let got = word(line, "fnv1a64").ok_or_else(|| format!("no digest: {line}"))?;
    let want = format!("{:016x}", fnv1a64(&picture(&l, ALL)));
    if got != want {
        return Err(format!("last readback {got}, the picture {want}"));
    }
    Ok(format!("last readback {want_size} = the picture ({want})"))
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

/// The sub-gates.
pub fn checks(app: &str, liveness: &Result<Liveness, String>) -> Vec<Sub> {
    vec![
        (
            "shown",
            layout(app).map(|l| format!("window {}x{}, anchor {:?}", l.width, l.height, l.anchor)),
        ),
        ("readback", readback_check(app)),
        (
            "pong",
            match liveness {
                Ok(l) if l.answered => Ok("the window answered a ping".to_owned()),
                Ok(_) => Err("no pong".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
        ("touch", touch_check(app)),
        ("controls", controls(app)),
    ]
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

/// The probe: the window's configure is acked, the dialog drew, one ping, a drag on the dialog; the window is resized with it open, and a second drag.
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, SHOWN, ACK_WAIT)
        .then_some(())
        .ok_or("the dialog never drew")?;
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    drag(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    let (width, height, serial) = RESIZED;
    proxy
        .inject_configure(width, height, serial)
        .map_err(|e| e.to_string())?;
    await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Acked { serial }
    })
    .ok_or("the resize was never acked")?;
    std::thread::sleep(RESIZE_SETTLE);
    drag(&proxy, app_log)?;
    std::thread::sleep(RESIZE_SETTLE);
    Ok(Liveness {
        answered,
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
}
