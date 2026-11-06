// SPDX-License-Identifier: MIT
//! Stage `surface-view` of `xtask test window` (SV2): software SurfaceViews (xtask/testdata/surface-view, Main.java) in a software window, UI mode on the nested labwc at scale [`SCALE`].
//! The picture is worked out here from the fixture's spec (its colours, and the rectangles it logs after layout), never from the code under test:
//! the window colour everywhere; A (below the window, its hole punched) with the window's overlay drawn above it; B (setZOrderOnTop) covering its overlay; C (below, opaque, never drawn) showing its
//! background colour layer, black (SurfaceView.mBackgroundControl, `setRelativeLayer(.., Integer.MIN_VALUE)`, SurfaceView.java:2150; the colour quad of the compositor's Scene).
//! Sub-gates ([`checks`]): drawn (A and B posted a frame, C was left undrawn), readback (the last settled readback is that picture), pong (the window answered a ping),
//! controls: the picture check rejects pictures with one rule broken (no hole: A hidden under the window; z ignored: B under its overlay; no colour layer: C shows the window).

use std::path::Path;

use super::{ACK_WAIT, Liveness, PONG_WAIT, Sub, await_event, capture_line, wait_for_line};
use crate::composer::{fnv1a64, word};
use crate::nested_wayland::{GATE_OUTPUT, Output};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask/testdata/surface-view";
/// The marker (Main.java onResume).
pub const MARKER: &str = "AtlWindow: onResume reached";
/// HiDPI: a fractional scale, as a laptop's 1.6 (move_output.rs SCALED_OUTPUT).
pub const SCALE: f64 = 1.6;
/// The gate's single output, at [`SCALE`].
pub const OUTPUT: Output = Output {
    scale: SCALE,
    ..GATE_OUTPUT
};
/// The fixture's layout line (Main.java onGlobalLayout).
const LAYOUT: &str = "AtlWindow: sv layout ";
/// Main.java's colours (ARGB ints there), as RGBA bytes.
const WINDOW: [u8; 4] = [0x20, 0x40, 0x60, 0xff];
const A: [u8; 4] = [0xc0, 0x80, 0x20, 0xff];
const B: [u8; 4] = [0x20, 0xc0, 0x80, 0xff];
const OVERLAY: [u8; 4] = [0xf0, 0xf0, 0xf0, 0xff];
/// SurfaceView.mBackgroundColor's default, Color.BLACK (SurfaceView.java:264).
const BACKGROUND: [u8; 4] = [0, 0, 0, 0xff];

/// `l,t,r,b` in window pixels.
#[derive(Clone, Copy, Debug)]
struct Rect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Rect {
    fn parse(s: &str) -> Option<Rect> {
        let v: Vec<u32> = s
            .split(',')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        match v[..] {
            [left, top, right, bottom] => Some(Rect {
                left,
                top,
                right,
                bottom,
            }),
            _ => None,
        }
    }

    fn contains(&self, x: u32, y: u32) -> bool {
        (self.left..self.right).contains(&x) && (self.top..self.bottom).contains(&y)
    }
}

/// What the fixture laid out.
#[derive(Clone, Copy, Debug)]
struct Layout {
    width: u32,
    height: u32,
    a: Rect,
    b: Rect,
    c: Rect,
    over_a: Rect,
    over_b: Rect,
}

fn layout(app: &str) -> Result<Layout, String> {
    let line = app
        .lines()
        .find(|l| l.contains(LAYOUT))
        .ok_or("no layout line from the fixture")?;
    let rect = |key: &str| {
        word(line, key)
            .and_then(Rect::parse)
            .ok_or_else(|| format!("no {key}= in {line}"))
    };
    let (w, h) = word(line, "window")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or_else(|| format!("no window= in {line}"))?;
    Ok(Layout {
        width: w,
        height: h,
        a: rect("a")?,
        b: rect("b")?,
        c: rect("c")?,
        over_a: rect("overA")?,
        over_b: rect("overB")?,
    })
}

/// Which composition rules the expected picture follows; the controls turn one off.
#[derive(Clone, Copy)]
struct Rules {
    hole: bool,
    z_on_top: bool,
    colour_layer: bool,
}

const ALL: Rules = Rules {
    hole: true,
    z_on_top: true,
    colour_layer: true,
};

/// The picture of `l` under `rules`, RGBA rows of the window's width, top first: per pixel the topmost of (window content, SurfaceViews by z).
fn picture(l: &Layout, rules: Rules) -> Vec<u8> {
    let mut out = Vec::with_capacity(l.width as usize * l.height as usize * 4);
    for y in 0..l.height {
        for x in 0..l.width {
            // z ignored: B sits under the window, which draws its own colour there (an on-top SurfaceView punches no hole)
            let px = if rules.z_on_top && l.b.contains(x, y) {
                B
            } else if l.over_a.contains(x, y) || l.over_b.contains(x, y) {
                OVERLAY
            } else if rules.hole && l.a.contains(x, y) {
                A
            } else if rules.hole && rules.colour_layer && l.c.contains(x, y) {
                BACKGROUND
            } else {
                WINDOW
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

fn drawn_check(app: &str) -> Result<String, String> {
    for line in [
        "AtlWindow: sv a drew ",
        "AtlWindow: sv b drew ",
        "AtlWindow: sv c left undrawn ",
    ] {
        if !app.contains(line) {
            return Err(format!("no `{}` line", line.trim_end()));
        }
    }
    Ok("A and B drew with lockCanvas, C left undrawn".to_owned())
}

/// The last readback's size and digest against [`picture`] under `rules`.
fn readback(app: &str, l: &Layout, rules: Rules) -> Result<String, String> {
    capture_line(app)?;
    let line = app
        .lines()
        .rfind(|line| line.contains("atl-composer: ui: capture "))
        .ok_or("no readback")?;
    let size = word(line, "size").unwrap_or("none");
    let want_size = format!("{}x{}", l.width, l.height);
    if size != want_size {
        return Err(format!("last readback is {size}, the window {want_size}"));
    }
    let got = word(line, "fnv1a64").ok_or_else(|| format!("no digest: {line}"))?;
    let want = format!("{:016x}", fnv1a64(&picture(l, rules)));
    if got != want {
        return Err(format!("last readback {got}, the picture {want}"));
    }
    Ok(format!("last readback {want_size} = the picture ({want})"))
}

fn readback_check(app: &str) -> Result<String, String> {
    let l = layout(app)?;
    readback(app, &l, ALL)
}

/// Each picture with one rule broken differs from the real one; with the real readback passing, each would fail it.
fn controls(app: &str) -> Result<String, String> {
    let l = layout(app)?;
    let real = fnv1a64(&picture(&l, ALL));
    let broken = [
        ("no hole", Rules { hole: false, ..ALL }),
        (
            "z ignored",
            Rules {
                z_on_top: false,
                ..ALL
            },
        ),
        (
            "no colour layer",
            Rules {
                colour_layer: false,
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
        Ok("no hole / z ignored / no colour layer each change the picture".to_owned())
    } else {
        Err(format!("broken rules {same:?} give the real picture"))
    }
}

/// The sub-gates.
pub fn checks(app: &str, liveness: &Result<Liveness, String>) -> Vec<Sub> {
    vec![
        ("drawn", drawn_check(app)),
        ("readback", readback_check(app)),
        (
            "pong",
            match liveness {
                Ok(l) if l.answered => Ok("the window answered a ping".to_owned()),
                Ok(_) => Err("no pong".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
        ("controls", controls(app)),
    ]
}

/// The probe: the window's configure is acked, the fixture laid out, then one ping.
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, LAYOUT, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never laid out")?;
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    Ok(Liveness {
        answered,
        ..Liveness::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "1.0 I AtlWindow: sv layout window=8x4 a=0,0,3,3 b=4,0,7,3 c=7,0,8,2 overA=1,1,2,2 overB=5,1,6,2\n";

    fn at(p: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    /// The picture by hand on a small layout: window, A, overlay over A, B over its overlay, C's black background.
    #[test]
    fn picture_by_hand() {
        let l = layout(LINE).expect("layout");
        let p = picture(&l, ALL);
        assert_eq!(at(&p, 0, 0), A);
        assert_eq!(at(&p, 1, 1), OVERLAY);
        assert_eq!(at(&p, 5, 1), B);
        assert_eq!(at(&p, 7, 0), BACKGROUND);
        assert_eq!(at(&p, 7, 3), WINDOW);
        assert_eq!(at(&p, 3, 3), WINDOW);
        assert!(controls(LINE).is_ok());
    }
}
