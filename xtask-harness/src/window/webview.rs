// SPDX-License-Identifier: MIT
//! Stage `webview` of `xtask test window` (webview plan S7 W3a): a WebView whose page is one colour, a native View over part of it (xtask-harness/testdata/webview, Main.java), UI mode on the nested labwc.
//! The picture is worked out here from the fixture's spec (its colours, and the rectangles it logs after layout), never from the code under test: the window colour everywhere, the page's colour in
//! the WebView's rectangle (the carrier SurfaceView's hole, drawn by the platform at the carrier layer's depth), the overlay above it.
//! Sub-gates ([`checks`]): loaded (the page's onPageFinished), readback (the settled capture is that picture, pixel for pixel; the capture route renders the window's own widget, so it holds the page),
//! pong (the window answered a ping), controls: pictures with one rule broken differ (no page: the carrier's black background colour layer; page over the overlay; page under the window).
//! Break `--break-webview-engine`: the run's library dir lacks the engine module, the fixture's WebView cannot be made, the readback must fail.

use std::path::Path;
use std::sync::Mutex;

use super::capture;
use super::{ACK_WAIT, Liveness, PONG_WAIT, Sub, await_event, wait_for_line};
use crate::composer::{fnv1a64, word};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/webview";
/// The marker (Main.java onResume).
pub const MARKER: &str = "AtlWindow: onResume reached";
/// The fixture's layout line (Main.java onGlobalLayout).
const LAYOUT: &str = "AtlWindow: wv layout ";
/// The page's onPageFinished (Main.java).
const LOADED: &str = "AtlWindow: wv page finished";
/// The fixture could not make its WebView (Main.java onCreate, before the layout line).
const NO_WEBVIEW: &str = "AtlWindow: wv no webview";
/// After the load, the time WebKit's first paint of the page may take to reach the window (one web-process frame and one present; generous for a loaded host).
const PAINT_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);
/// Main.java's colours as RGBA bytes: WINDOW 0xff204060, OVERLAY 0xfff0f0f0, the page's CSS rgb(48,112,192).
const WINDOW: [u8; 4] = [0x20, 0x40, 0x60, 0xff];
const OVERLAY: [u8; 4] = [0xf0, 0xf0, 0xf0, 0xff];
const PAGE: [u8; 4] = [0x30, 0x70, 0xc0, 0xff];
/// SurfaceView.mBackgroundColor's default, Color.BLACK (SurfaceView.java:264): the carrier's hole with nothing on its layer.
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
    web: Rect,
    over: Rect,
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
        web: rect("web")?,
        over: rect("over")?,
    })
}

/// Which composition rules the expected picture follows; the controls turn one off.
#[derive(Clone, Copy)]
struct Rules {
    /// The page shows in the carrier's hole (else its black background layer).
    page: bool,
    /// The overlay View is above the page (else the page covers it).
    overlay_above: bool,
    /// The page is in a hole of the window (else the window's colour covers it).
    hole: bool,
}

const ALL: Rules = Rules {
    page: true,
    overlay_above: true,
    hole: true,
};

/// The colour of pixel (`x`, `y`) of `l` under `rules`.
fn pixel(l: &Layout, rules: Rules, x: u32, y: u32) -> [u8; 4] {
    let in_web = l.web.contains(x, y);
    if l.over.contains(x, y) && !(in_web && !rules.overlay_above) {
        OVERLAY
    } else if in_web && rules.hole {
        if rules.page { PAGE } else { BACKGROUND }
    } else {
        WINDOW
    }
}

/// The picture of `l` under `rules`, RGBA rows of the window's width, top first.
fn picture(l: &Layout, rules: Rules) -> Vec<u8> {
    let mut out = Vec::with_capacity(l.width as usize * l.height as usize * 4);
    for y in 0..l.height {
        for x in 0..l.width {
            out.extend_from_slice(&pixel(l, rules, x, y));
        }
    }
    out
}

/// The settled picture against [`picture`]: its size, then every pixel; a difference names how many pixels differ and the first.
fn readback(pictures: &Mutex<capture::Seen>, l: &Layout) -> Result<String, String> {
    let shot = capture::picture(pictures, 0)?;
    if shot.size != (l.width, l.height) {
        return Err(format!(
            "the picture is {}x{}, the window {}x{}",
            shot.size.0, shot.size.1, l.width, l.height
        ));
    }
    let want = picture(l, ALL);
    let mut differ = 0usize;
    let mut first = None;
    for (i, (g, w)) in shot.rgba.chunks(4).zip(want.chunks(4)).enumerate() {
        if g != w {
            differ += 1;
            first.get_or_insert((
                i as u32 % l.width,
                i as u32 / l.width,
                g.to_vec(),
                w.to_vec(),
            ));
        }
    }
    match first {
        None => Ok(format!(
            "picture {}x{} = the picture ({:016x}): page {:?} at web, overlay above",
            l.width,
            l.height,
            fnv1a64(&want),
            PAGE
        )),
        Some((x, y, g, w)) => Err(format!(
            "{differ} pixels differ; first at {x},{y}: {g:?}, want {w:?}"
        )),
    }
}

/// Each picture with one rule broken differs from the real one; with the real readback passing, each would fail it.
fn controls(app: &str) -> Result<String, String> {
    let l = layout(app)?;
    let real = fnv1a64(&picture(&l, ALL));
    let broken = [
        ("no page", Rules { page: false, ..ALL }),
        (
            "page over the overlay",
            Rules {
                overlay_above: false,
                ..ALL
            },
        ),
        ("no hole", Rules { hole: false, ..ALL }),
    ];
    let same: Vec<&str> = broken
        .iter()
        .filter(|(_, r)| fnv1a64(&picture(&l, *r)) == real)
        .map(|(n, _)| *n)
        .collect();
    if same.is_empty() {
        Ok("no page / page over the overlay / no hole each change the picture".to_owned())
    } else {
        Err(format!("broken rules {same:?} give the real picture"))
    }
}

/// The sub-gates.
pub fn checks(
    app: &str,
    liveness: &Result<Liveness, String>,
    pictures: &Mutex<capture::Seen>,
) -> Vec<Sub> {
    vec![
        (
            "loaded",
            if app.contains(LOADED) {
                Ok("onPageFinished".to_owned())
            } else {
                Err(format!("no `{LOADED}` line"))
            },
        ),
        ("readback", layout(app).and_then(|l| readback(pictures, &l))),
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

/// The probe: the window's configure is acked, the fixture laid out and its page loaded (or the fixture said it has no WebView), WebKit's paint settles, then one settled picture and a ping.
pub fn drive(proxy: Proxy, app_log: &Path, probe: &capture::Probe) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, LAYOUT, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never laid out")?;
    // a missing page is the readback's failure (the break), not the probe's: a fixture without a WebView said so in onCreate, before its layout line, and has no load to wait for
    let no_webview = std::fs::read_to_string(app_log).is_ok_and(|l| l.contains(NO_WEBVIEW));
    if !no_webview && wait_for_line(app_log, LOADED, ACK_WAIT) {
        std::thread::sleep(PAINT_SETTLE);
    }
    probe.record_settled();
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

    const LINE: &str = "1.0 I AtlWindow: wv layout window=8x4 web=1,0,6,3 over=4,1,7,2\n";

    fn at(p: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * 8 + x) * 4;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    /// The picture by hand on a small layout: window, page, overlay over the page and past its edge.
    #[test]
    fn picture_by_hand() {
        let l = layout(LINE).expect("layout");
        let p = picture(&l, ALL);
        assert_eq!(at(&p, 0, 0), WINDOW);
        assert_eq!(at(&p, 1, 0), PAGE);
        assert_eq!(at(&p, 4, 1), OVERLAY);
        assert_eq!(at(&p, 6, 1), OVERLAY);
        assert_eq!(at(&p, 6, 0), WINDOW);
        assert_eq!(at(&p, 2, 3), WINDOW);
        let no_page = picture(&l, Rules { page: false, ..ALL });
        assert_eq!(at(&no_page, 1, 0), BACKGROUND);
        let under = picture(
            &l,
            Rules {
                overlay_above: false,
                ..ALL
            },
        );
        assert_eq!(at(&under, 4, 1), PAGE);
        assert_eq!(at(&under, 6, 1), OVERLAY);
        assert!(controls(LINE).is_ok());
    }
}
