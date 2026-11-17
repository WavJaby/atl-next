// SPDX-License-Identifier: MIT
//! Stage `webview` of `xtask test window` (webview plan S7 W3a): a WebView whose page is one colour, a native View over part of it (xtask-harness/testdata/webview, Main.java), UI mode on the nested labwc.
//! The picture is worked out here from the fixture's spec (its colours, and the rectangles it logs after layout), never from the code under test: the window colour everywhere, the page's colour in
//! the WebView's rectangle (the carrier SurfaceView's hole, drawn by the platform at the carrier layer's depth), the overlay above it.
//! Sub-gates ([`checks`]): loaded (the page's onPageFinished), readback (the settled capture is that picture, pixel for pixel; the capture route renders the window's own widget, so it holds the page),
//! pong (the window answered a ping), controls: pictures with one rule broken differ (no page: the carrier's black background colour layer; page over the overlay; page under the window).
//! Input (W3b, hold-and-replay): page-tap (a click on the page reaches it), view-tap (a click on the overlapping View reaches the View, not the page), intercept (a drag the WebView's parent takes:
//! ACTION_CANCEL after the page's press, `webview.input.intercept-no-cancel` counted, no click), after-intercept (the next click reaches the page).
//! Keys: esc-handled (two Escs reach the page once each, the first taken by its handler with no back), esc-back (the second, left unhandled, reaches the app as BACK through
//! WebViewClient.onUnhandledKeyEvent -> SyntheticKeyboardHandler with `--esc-back`; none without).
//! Break `--break-webview-engine`: the run's library dir lacks the engine module, the fixture's WebView cannot be made, the readback must fail. Break `--esc-back --break-esc-back`: esc-back must fail.

use std::path::Path;
use std::sync::Mutex;

use super::{ACK_WAIT, Liveness, PONG_WAIT, Sub, await_event, wait_for_line};
use super::{capture, widgets};
use crate::composer::{fnv1a64, word};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

/// A page title (Main.java onReceivedTitle): every click the page gets sets `tap<n>`.
const TITLE: &str = "AtlWindow: wv title ";
/// The overlay View's click (Main.java).
const OVERLAY_TAP: &str = "AtlWindow: wv overlay tap";
/// The WebView's parent took the drag (Main.java Intercept).
const INTERCEPTED: &str = "AtlWindow: wv intercepted";
/// The page's titles for the two Escs (Main.java PAGE_HTML onkeydown): the first taken by its handler, the second left.
const ESC_PREVENTED: &str = "key1Escape-prevented";
const ESC_LEFT: &str = "key2Escape";
/// The activity's back callback (Main.java).
const BACK_LINE: &str = "AtlWindow: wv back";
/// input-event-codes.h KEY_ESC.
const KEY_ESC: u32 = 1;
/// After the handled Esc, how long an (unwanted) unhandled report may take to show as a back before the second Esc: WebKitGTK puts back an unhandled press after the release (A0), one web-process
/// round trip, well under a second on a quiet host; esc_handled judges a back seen before the second Esc's title.
const ESC_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
/// The provider's divergence counter (atl-webview jni.rs `INTERCEPT_NO_CANCEL`, webview plan S2.2 amended).
const INTERCEPT_COUNTER: &str = "webview.input.intercept-no-cancel";
/// Its first record in the app log (atl-diag: `diag: first <outcome> <api> ...`).
const DIVERGENCE_LINE: &str = "diag: first divergence webview.input.intercept-no-cancel";
/// How long each input waits for the fixture's line: one Looper trip plus one web-process round trip; generous for a loaded host.
const INPUT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// The drag: steps of this many pixels, past the 8 dp touch slop of a scrolling parent (the fixture's Intercept takes the first move anyway).
const DRAG_STEPS: u32 = 3;
const DRAG_STEP_PX: u32 = 20;
/// Between the drag's press and each move: past the toolkit's 1 ms event clock, so each move has its own time as a device's would; injected in one batch all share the press's ms, and the
/// window drops motion samples timed at or before the last published event (input.rs `motion_samples`), so Android would see Down then Up and no MOVE for the parent to intercept.
const DRAG_STEP_MS: u64 = 10;

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
    esc_back: bool,
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
        ("provider-api", provider_api(app)),
        ("controls", controls(app)),
        ("page-tap", page_tap(app)),
        ("view-tap", view_tap(app)),
        ("intercept", intercept(app)),
        ("after-intercept", after_intercept(app)),
        ("esc-handled", esc_handled(app)),
        ("esc-back", esc_back_check(app, esc_back)),
    ]
}

/// Sub-gate provider-api: the fixture's SDK-style calls after the first onPageFinished threw nothing and answered AOSP's documented values: progress 100 (finished), the
/// loadData `data:` URL as both URL and original URL (no redirect), JavaScript on as set, HitTestResult UNKNOWN_TYPE (0, nothing hit), saveState's failure value null.
fn provider_api(app: &str) -> Result<String, String> {
    const API: &str = "AtlWindow: wv api ";
    let line = app
        .lines()
        .find_map(|l| l.find(API).map(|i| &l[i + API.len()..]))
        .ok_or("no `wv api` line")?;
    let Some(rest) = line.strip_prefix("ok ") else {
        return Err(format!("a WebView call threw: {line}"));
    };
    let field = |k: &str| {
        rest.split(' ')
            .find_map(|f| f.strip_prefix(k))
            .unwrap_or("")
    };
    let url = field("url=");
    let want = [
        ("progress=", "100"),
        ("js=", "true"),
        ("hit=", "0"),
        ("state=", "null"),
    ];
    let bad: Vec<_> = want.iter().filter(|(k, v)| field(k) != *v).collect();
    if !bad.is_empty() || !url.starts_with("data:") || field("original=") != url {
        return Err(format!(
            "answers {rest}; want {want:?}, url data:..., original = url"
        ));
    }
    Ok(
        "timers / onPause / onResume / clear* threw nothing; getters answered AOSP's values"
            .to_owned(),
    )
}

/// The page's key titles (`key<n>...`), in order.
fn key_titles(app: &str) -> Vec<&str> {
    app.lines()
        .filter_map(|l| l.find(TITLE).map(|i| &l[i + TITLE.len()..]))
        .filter(|t| t.starts_with("key"))
        .collect()
}

/// Sub-gate esc-handled: the page got each Esc once (a press published twice would show a third title: WebKit's put-back of an unhandled press must be reported, not published again), and the
/// Esc its handler took brought no back.
fn esc_handled(app: &str) -> Result<String, String> {
    let titles = key_titles(app);
    if titles != [ESC_PREVENTED, ESC_LEFT] {
        return Err(format!(
            "page key titles {titles:?}, want [{ESC_PREVENTED}, {ESC_LEFT}]"
        ));
    }
    let second = app
        .find(&format!("{TITLE}{ESC_LEFT}"))
        .ok_or("no second Esc title")?;
    if app[..second].contains(BACK_LINE) {
        return Err("a back before the second Esc: the handled Esc reached the app".to_owned());
    }
    Ok("each Esc reached the page once; the one its handler took brought no back".to_owned())
}

/// Sub-gate esc-back: with `esc_back` the Esc the page left reached the app's back once, after that Esc's title; without, no back (AOSP's map: Esc has no fallback).
fn esc_back_check(app: &str, esc_back: bool) -> Result<String, String> {
    let backs = app.matches(BACK_LINE).count();
    let want = usize::from(esc_back);
    if backs != want {
        return Err(if backs == 0 {
            "no back after the unhandled Esc".to_owned()
        } else {
            format!("{backs} backs, want {want}")
        });
    }
    if esc_back {
        let second = app
            .find(&format!("{TITLE}{ESC_LEFT}"))
            .ok_or("no second Esc title")?;
        if app.find(BACK_LINE).is_some_and(|b| b < second) {
            return Err("the back came before the unhandled Esc".to_owned());
        }
        Ok(
            "unhandled Esc -> WebViewClient.onUnhandledKeyEvent -> BACK -> back callback"
                .to_owned(),
        )
    } else {
        Ok("unhandled Esc -> no back (AOSP map)".to_owned())
    }
}

/// The fixture's input lines in log order: page titles (`tap<n>`), the overlay's click, the parent's intercept, the divergence counter's first record.
fn input_lines(app: &str) -> Vec<&str> {
    app.lines()
        .filter_map(|l| {
            if let Some(i) = l.find(TITLE) {
                Some(&l[i + TITLE.len()..])
            } else if l.contains(OVERLAY_TAP) {
                Some("overlay")
            } else if l.contains(INTERCEPTED) {
                Some("intercepted")
            } else if l.contains(DIVERGENCE_LINE) {
                Some("divergence")
            } else {
                None
            }
        })
        // the page's own title before any click (the data URL) is not a click; key titles are the Esc sub-gates'
        .filter(|s| !s.starts_with("data:") && !s.starts_with("key"))
        .collect()
}

/// The position of `what` in `lines`, or why it is missing.
fn at(lines: &[&str], what: &str) -> Result<usize, String> {
    lines
        .iter()
        .position(|l| *l == what)
        .ok_or_else(|| format!("no `{what}` in the input lines {lines:?}"))
}

/// Sub-gate page-tap: the first tap (on the page, off the overlay) reached the page: its first title is `tap1`, before anything else.
fn page_tap(app: &str) -> Result<String, String> {
    let lines = input_lines(app);
    match lines.first() {
        Some(&"tap1") => Ok("the tap on the page reached it (title tap1)".to_owned()),
        _ => Err(format!("input lines {lines:?}: want tap1 first")),
    }
}

/// Sub-gate view-tap: the tap on the overlay reached the overlay View and not the page: `overlay` follows `tap1`, and the next page title comes only after the intercept.
fn view_tap(app: &str) -> Result<String, String> {
    let lines = input_lines(app);
    let (t1, o) = (at(&lines, "tap1")?, at(&lines, "overlay")?);
    if o != t1 + 1 {
        return Err(format!(
            "input lines {lines:?}: want overlay right after tap1"
        ));
    }
    if lines.get(o + 1).is_some_and(|l| l.starts_with("tap")) {
        return Err(format!(
            "input lines {lines:?}: the overlay's tap also reached the page"
        ));
    }
    Ok("the tap on the overlay reached the View only".to_owned())
}

/// Sub-gate intercept: the drag on the page was intercepted by the WebView's parent, the divergence counted once, and the drag's release did not click the page.
fn intercept(app: &str) -> Result<String, String> {
    let lines = input_lines(app);
    let (i, d) = (at(&lines, "intercepted")?, at(&lines, "divergence")?);
    if d < i {
        return Err(format!(
            "input lines {lines:?}: divergence before the intercept"
        ));
    }
    if lines[i..d].iter().any(|l| l.starts_with("tap")) {
        return Err(format!(
            "input lines {lines:?}: the intercepted drag clicked the page"
        ));
    }
    Ok(format!(
        "ACTION_CANCEL after the page's press: `{INTERCEPT_COUNTER}` counted"
    ))
}

/// Sub-gate after-intercept: a tap on the page after the intercept reaches it (title tap2), the last input line: the page is not stuck.
fn after_intercept(app: &str) -> Result<String, String> {
    let lines = input_lines(app);
    let i = at(&lines, "intercepted")?;
    match lines.last() {
        Some(&"tap2") if at(&lines, "tap2")? > i => {
            Ok("the next tap reached the page (title tap2)".to_owned())
        }
        _ => Err(format!(
            "input lines {lines:?}: want tap2 last, after the intercept"
        )),
    }
}

/// One click at `(x, y)` (surface-local, logical): move there, press, release.
fn click(proxy: &Proxy, (x, y): (f64, f64)) -> Result<(), String> {
    widgets::inject(
        proxy,
        vec![
            PointerAction::Motion { x, y },
            widgets::press(true),
            widgets::press(false),
        ],
    )
}

/// The input part of the probe: tap the page, tap the overlay, drag on the page (the parent intercepts), tap the page again; each waits for its line (a missing line is the sub-gate's failure).
fn drive_input(proxy: &Proxy, app_log: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let l = layout(&text)?;
    let (ox, oy) = super::content_origin(&text)?;
    // window pixels -> surface (scale 1 on the gate output)
    let s = |x: u32, y: u32| (f64::from(x) + f64::from(ox), f64::from(y) + f64::from(oy));
    // on the page, right of the overlay
    let page = s(l.web.right - 50, l.web.bottom - 50);
    let overlay = s(
        (l.over.left + l.over.right) / 2,
        (l.over.top + l.over.bottom) / 2,
    );
    let (dx, dy) = (l.web.right - 100, l.web.top + 20);
    widgets::inject(
        proxy,
        vec![PointerAction::Enter {
            x: page.0,
            y: page.1,
        }],
    )?;
    click(proxy, page)?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains("wv title tap1"));
    click(proxy, overlay)?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains(OVERLAY_TAP));
    let start = s(dx, dy);
    widgets::inject(
        proxy,
        vec![
            PointerAction::Motion {
                x: start.0,
                y: start.1,
            },
            widgets::press(true),
        ],
    )?;
    for step in 1..=DRAG_STEPS {
        std::thread::sleep(std::time::Duration::from_millis(DRAG_STEP_MS));
        let p = s(dx + step * DRAG_STEP_PX, dy);
        widgets::inject(proxy, vec![PointerAction::Motion { x: p.0, y: p.1 }])?;
    }
    std::thread::sleep(std::time::Duration::from_millis(DRAG_STEP_MS));
    widgets::inject(proxy, vec![widgets::press(false)])?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains(INTERCEPTED));
    click(proxy, page)?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains("wv title tap2"));
    // the page has the focus (clicked): Esc its handler takes, then Esc it leaves
    let esc = || -> Result<(), String> {
        proxy.inject_key(KEY_ESC, true).map_err(|e| e.to_string())?;
        proxy.inject_key(KEY_ESC, false).map_err(|e| e.to_string())
    };
    esc()?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains(ESC_PREVENTED));
    std::thread::sleep(ESC_SETTLE);
    esc()?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains(ESC_LEFT));
    // a back (esc-back on) comes after the title; off, the settle shows none came
    let _ = widgets::wait_until(app_log, ESC_SETTLE, |t| t.contains(BACK_LINE));
    Ok(())
}

/// The probe: the window's configure is acked, the fixture laid out and its page loaded (or the fixture said it has no WebView), WebKit's paint settles, then one settled picture and a ping,
/// then the input ([`drive_input`]).
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
    if !no_webview {
        drive_input(&proxy, app_log)?;
    }
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

    /// The input sub-gates on hand-written logs: the expected order passes; the overlay's tap leaking to the page, a clicking drag, and no second page tap each fail.
    #[test]
    fn input_sub_gates_by_hand() {
        let ok = "x AtlWindow: wv title data:text/html,x\nx AtlWindow: wv title tap1\nx AtlWindow: wv overlay tap\nx AtlWindow: wv intercepted\n\
                  diag: first divergence webview.input.intercept-no-cancel 1 at=1\nx AtlWindow: wv title tap2\n";
        assert!(
            page_tap(ok).is_ok()
                && view_tap(ok).is_ok()
                && intercept(ok).is_ok()
                && after_intercept(ok).is_ok()
        );
        let leaked =
            "x AtlWindow: wv title tap1\nx AtlWindow: wv overlay tap\nx AtlWindow: wv title tap2\n";
        assert!(view_tap(leaked).is_err());
        let clicked = "x AtlWindow: wv title tap1\nx AtlWindow: wv overlay tap\nx AtlWindow: wv intercepted\nx AtlWindow: wv title tap2\n\
                       diag: first divergence webview.input.intercept-no-cancel 1 at=1\n";
        assert!(intercept(clicked).is_err());
        let stuck = "x AtlWindow: wv title tap1\nx AtlWindow: wv overlay tap\nx AtlWindow: wv intercepted\n\
                     diag: first divergence webview.input.intercept-no-cancel 1 at=1\n";
        assert!(after_intercept(stuck).is_err());
        // the Esc titles after tap2 leave the tap sub-gates alone
        let keys = format!(
            "{ok}x AtlWindow: wv title key1Escape-prevented\nx AtlWindow: wv title key2Escape\n"
        );
        assert!(after_intercept(&keys).is_ok());
    }

    /// provider-api on hand-written lines: the owner's crash shape (resumeTimers refused) fails, AOSP's answers pass, a wrong answer fails.
    #[test]
    fn provider_api_by_hand() {
        let threw = "x AtlWindow: wv api threw java.lang.UnsupportedOperationException: WebViewProvider.resumeTimers\n";
        assert!(provider_api(threw).is_err());
        let ok = "x AtlWindow: wv api ok progress=100 url=data:text/html,x original=data:text/html,x js=true hit=0 state=null\n";
        assert!(provider_api(ok).is_ok());
        assert!(provider_api(&ok.replace("original=data:text/html,x", "original=null")).is_err());
        assert!(provider_api("").is_err());
    }

    /// The Esc sub-gates on hand-written logs: handled Esc no back, left Esc one back after it (esc-back on) or none (off); a duplicate key, an early back or a missing back fail.
    #[test]
    fn esc_sub_gates_by_hand() {
        let t = |s: &str| format!("x AtlWindow: wv title {s}\n");
        let back = "x AtlWindow: wv back\n";
        let on = format!("{}{}{back}", t("key1Escape-prevented"), t("key2Escape"));
        assert!(esc_handled(&on).is_ok() && esc_back_check(&on, true).is_ok());
        assert!(esc_back_check(&on, false).is_err());
        let off = format!("{}{}", t("key1Escape-prevented"), t("key2Escape"));
        assert!(esc_handled(&off).is_ok() && esc_back_check(&off, false).is_ok());
        assert_eq!(
            esc_back_check(&off, true),
            Err("no back after the unhandled Esc".to_owned())
        );
        // WebKit's put-back published again: the page sees the second Esc twice
        let dup = format!("{off}{}{back}", t("key3Escape"));
        assert!(esc_handled(&dup).is_err());
        let early = format!("{}{back}{}", t("key1Escape-prevented"), t("key2Escape"));
        assert!(esc_handled(&early).is_err() && esc_back_check(&early, true).is_err());
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
