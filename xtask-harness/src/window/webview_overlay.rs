// SPDX-License-Identifier: MIT
//! Stage `webview-overlay` of `xtask test window`: an in-game survey's arrangement (Angry Birds 2, owner 2026-11-18): one activity, an animating focused GLSurfaceView, a full-window WebView added
//! over it at runtime (xtask-harness/testdata/webview-overlay, Main.java). UI mode on the nested labwc; every input goes through the Wayland proxy, so it takes the real GTK path (toplevel key
//! controller, keyboard focus, the sideband gate), not a headless injection.
//! Sub-gates ([`checks`]): loaded (onPageFinished), web-children (the WebView's getChildCount counts only the app's own child, which the fixture then removes by index as Battle Cats TW's consent SDK
//! does: a provider View among the children would be the one removed, taking the page off the screen), resize (after a configure, the WebView's last placement is the window: sideband dst == the fixture's window size), page-tap (a click on the
//! page's button reaches the page: title tap1), then three Escs (AOSP BACK semantics through the Esc -> Back adaptation; `--esc-back`): esc-page-back (WebView.canGoBack -> goBack),
//! esc-dismiss (no history left -> the SDK's onBackPressed removes the WebView), esc-game (the WebView gone -> the activity's back). tap-focus: a mouse click focuses the WebView, a touch tap does not
//! (`--tap-origin`, [`tap_focus_check`]); esc-keys: each Esc goes to the focused View ([`esc_targets`]); focus-return: once the WebView is removed, focus is on the GL view again. hidden-preload: a second WebView loaded under an INVISIBLE parent (Angry Birds 2's survey SDK) is never placed and takes no click. software-layer: a third WebView under a software-layer ancestor (Angry Birds 2's account panel) is placed at its View's rectangle. Without `--esc-back` (AOSP's map: Esc has no fallback) no back follows.

use std::path::Path;
use std::time::Duration;

use super::{ACK_WAIT, Liveness, Sub, await_event, wait_for_line, widgets};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy, TouchAction};

/// What the page tap is (`--tap-origin`): the physical input decides whether it focuses the WebView ([`tap_focus_check`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TapOrigin {
    /// A left mouse click; the app sees SOURCE_MOUSE (AOSP input).
    Mouse,
    /// A left mouse click with the desktop's mouse-as-one-finger adaptation on (atld `--mouse-touch true`): the app sees a touchscreen finger, the page still a mouse click.
    MouseFinger,
    /// A real touch contact on the proxy's own `wl_touch`.
    Touch,
}

impl TapOrigin {
    /// The press focuses an unfocused WebView: a mouse button's does, in both adaptation states; a touch contact's does not.
    pub fn focuses(self) -> bool {
        self != Self::Touch
    }
}

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/webview-overlay";
/// The marker (Main.java onResume).
pub const MARKER: &str = "AtlWindow: onResume reached";
/// Main.java's lines.
const LOADED: &str = "AtlWindow: wvo page finished";
const TITLE: &str = "AtlWindow: wvo title ";
const LAYOUT: &str = "AtlWindow: wvo layout ";
const BACK_PAGE: &str = "AtlWindow: wvo back page";
const BACK_DISMISSED: &str = "AtlWindow: wvo back dismissed";
const BACK_GAME: &str = "AtlWindow: wvo back game";
const GL_KEY: &str = "AtlWindow: wvo gl key ";
/// Main.java's bridge line for each key the page's keydown gets: logged before the handler returns, so before the key's unhandled answer and the back it causes. Not the page title: onReceivedTitle
/// is unordered with that answer, and the dismissal's destroy() drops a title still on its way (Esc 2's count lost in 6 of 20 runs, 2026-11-21).
const PAGE_KEY: &str = "AtlWindow: wvo page key ";
/// Main.java's line at every Android focus move: `gl`, `web`, `none` or the View's class.
const FOCUS: &str = "AtlWindow: wvo focus ";
/// Main.java's count of the WebView's children before it removes its indicator: 1 = the app's own child only (AOSP: the WebView's rendering adds no View child).
const WEB_CHILDREN: &str = "AtlWindow: wvo web children ";
/// Main.java's hidden preload (addPreload): its load, and the title its page sets on a click.
const PRELOAD_LOADED: &str = "AtlWindow: wvo preload finished";
const PRELOAD_TAP: &str = "AtlWindow: wvo preload title ptap";
/// Main.java's software-layer WebView (addSoftwareLayer): its rectangle `l,t,r,b` in the window.
const SOFTWARE_LAYER: &str = "AtlWindow: wvo swlayer web=";
/// The platform's placement line of a sideband widget (atl-platform-ui-linux view.rs): `dst=l,t,r,b` in physical pixels.
const PLACED: &str = "atl-platform-ui: sideband ";
/// input-event-codes.h KEY_ESC.
const KEY_ESC: u32 = 1;
/// After the load, WebKit's first paint (one web-process frame and one present; generous for a loaded host), as the webview stage.
const PAINT_SETTLE: Duration = Duration::from_secs(2);
/// One Looper trip plus one web-process round trip per input; generous for a loaded host.
const INPUT_WAIT: Duration = Duration::from_secs(5);
/// The resizes: logical sizes (gate output scale 1: physical too), serials past the first configures'. A shrink from the default window in both axes, then a grow in height only (the owner's
/// case: window made taller, page kept its old height). The check runs on the last one.
const RESIZES: [(i32, i32, u32); 2] = [(720, 400, 4242), (720, 800, 4243)];
const RESIZE: (i32, i32, u32) = RESIZES[1];
/// Wheel notches down: any positive count scrolls a page taller than the window; 3 = one typical wheel flick.
const WHEEL_NOTCHES: i32 = 3;

/// The last `wvo layout window=<w>x<h>` size the fixture logged.
fn window_size(app: &str) -> Option<(u32, u32)> {
    let line = app.lines().rev().find(|l| l.contains(LAYOUT))?;
    let (w, h) = word(line, "window")?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The last sideband placement `dst=l,t,r,b` of a stream other than the software-layer WebView's.
fn last_placed(app: &str) -> Option<[i64; 4]> {
    let software = software_streams(app);
    placements(app)
        .into_iter()
        .rev()
        .find(|(s, _)| !software.contains(s))
        .map(|(_, dst)| dst)
}

/// The streams ever placed at the software-layer WebView's rectangle.
fn software_streams(app: &str) -> Vec<&str> {
    let sw = software_rect(app);
    placements(app)
        .into_iter()
        .filter(|(_, dst)| Some(*dst) == sw)
        .map(|(s, _)| s)
        .collect()
}

/// Every sideband placement: stream, `dst`.
fn placements(app: &str) -> Vec<(&str, [i64; 4])> {
    app.lines()
        .filter(|l| l.contains(PLACED) && l.contains(" placed dst="))
        .filter_map(|l| {
            let stream = l.split(PLACED).nth(1)?.split(" placed").next()?;
            let v: Vec<i64> = word(l, "dst")?
                .split(',')
                .filter_map(|n| n.parse().ok())
                .collect();
            Some((stream, v.try_into().ok()?))
        })
        .collect()
}

/// The software-layer WebView's last rectangle the fixture logged (window coordinates).
fn software_rect(app: &str) -> Option<[i64; 4]> {
    let v: Vec<i64> = app
        .lines()
        .rev()
        .find_map(|l| l.split(SOFTWARE_LAYER).nth(1))?
        .trim()
        .split(',')
        .filter_map(|n| n.parse().ok())
        .collect();
    v.try_into().ok()
}

/// Sub-gate software-layer: the WebView under a software-layer ancestor (Angry Birds 2's account panel) is placed where the View is: some stream's placement `dst` is the fixture's rectangle.
/// AOSP: its page shows at the View's place; on ATL nothing places the carrier's layer unless the provider does (RenderThread never sees a RenderNode under a software layer).
fn software_layer_check(app: &str) -> Result<String, String> {
    let rect = software_rect(app).ok_or(format!("no `{SOFTWARE_LAYER}` line"))?;
    match placements(app).into_iter().find(|(_, dst)| *dst == rect) {
        Some((stream, _)) => Ok(format!(
            "stream {stream} placed at the View's rectangle {rect:?}"
        )),
        None => Err(format!(
            "no sideband placement at the View's rectangle {rect:?}: the carrier under a software layer was not placed"
        )),
    }
}

/// Sub-gate hidden-preload: the WebView under an INVISIBLE parent loaded but was never placed (one full-window sideband stream placed: the visible WebView's; the software-layer one is told apart by
/// its rectangle) and its page got no click (AOSP: its SurfaceView layer is created hidden and shown only when drawn; the click goes to the visible page).
fn preload_check(app: &str) -> Result<String, String> {
    if !app.contains(PRELOAD_LOADED) {
        return Err(format!("no `{PRELOAD_LOADED}` line"));
    }
    let software = software_streams(app);
    let mut streams: Vec<&str> = placements(app)
        .into_iter()
        .map(|(s, _)| s)
        .filter(|s| !software.contains(s))
        .collect();
    streams.sort_unstable();
    streams.dedup();
    if streams.len() != 1 {
        return Err(format!(
            "sideband streams placed {streams:?}: the hidden preload's layer was shown"
        ));
    }
    if app.contains(PRELOAD_TAP) {
        return Err("the click reached the hidden preload's page".to_owned());
    }
    Ok("the hidden preload stayed off screen and got no click".to_owned())
}

/// Where the three Escs go, (page, GL view), and the sub-gate's reading. Android sends a key to the focused View (ViewRootImpl); the page's onkeydown takes each Esc it gets and leaves it
/// unhandled (no preventDefault). `focused`: the click moved Android focus to the WebView. AOSP map: every Esc goes to the focused View. `--esc-back`: an Esc the page leaves unhandled falls back
/// to BACK: the first goes back in the page's history, the second dismisses the WebView, after which focus is back on the GL view (ViewGroup.removeViewInternal -> rootViewRequestFocus) and the
/// third reaches it; an unfocused WebView never sees an Esc, each reaches the GL view and its BACK fallback does the same three steps.
fn esc_targets(esc_back: bool, focused: bool) -> (usize, usize) {
    match (focused, esc_back) {
        (false, _) => (0, 3),
        (true, false) => (3, 0),
        (true, true) => (2, 1),
    }
}

/// Sub-gate tap-focus: focus on press is keyed on the physical input, as Chromium's WebView keys it: a mouse-button press focuses an unfocused WebView (RenderWidgetHostViewAndroid
/// kFocusRenderWidgetHostViewAndroidOnMouseDown, on for WebView), a touch press does not (kFocusRenderWidgetHostViewAndroidOnActionDown, disabled for WebView in aw_field_trials.cc; View.onTouchEvent
/// focuses only clickable views, and WebView is not clickable). Read from the fixture's `wvo focus` lines.
fn tap_focus_check(app: &str, focused: bool) -> Result<String, String> {
    let moved = app.contains(&format!("{FOCUS}web"));
    match (focused, moved) {
        (true, true) => Ok("the mouse click moved Android focus to the WebView".to_owned()),
        (false, false) => Ok("the touch tap left Android focus on the GL view".to_owned()),
        (true, false) => {
            Err("the mouse click did not move Android focus to the WebView".to_owned())
        }
        (false, true) => Err("the touch tap moved Android focus to the WebView".to_owned()),
    }
}

/// Sub-gate focus-return: once the SDK removed its WebView, Android focus is on the GL view again (ViewGroup.removeViewInternal: unFocus, clearChildFocus, rootViewRequestFocus; the game must get
/// Back/Esc after the overlay is dismissed). Without a removal (AOSP map) nothing to judge. The last focus move is read, not the ones after the `back dismissed` line: Main.java logs that after removeView.
fn focus_return_check(app: &str) -> Result<String, String> {
    if !app.contains(BACK_DISMISSED) {
        return Ok("no removal (no back)".to_owned());
    }
    match app.lines().rev().find(|l| l.contains(FOCUS)) {
        Some(l) if l.trim_end().ends_with(&format!("{FOCUS}gl")) => {
            Ok("after the removal Android focus is on the GL view".to_owned())
        }
        Some(l) => Err(format!("last focus move after the removal: {}", l.trim())),
        // gl.requestFocus in onCreate runs before the window is attached, so the global listener first hears of focus when it moves: no line = it never left the GL view
        None => Ok("Android focus never left the GL view".to_owned()),
    }
}

/// The sub-gates. `focused`: the click is expected to move Android focus to the WebView ([`tap_focus_check`]).
pub fn checks(app: &str, esc_back: bool, focused: bool) -> Vec<Sub> {
    let has = |s: &str| app.contains(s);
    let line = |what: &str, s: &str| {
        if has(s) {
            Ok(what.to_owned())
        } else {
            Err(format!("no `{s}` line"))
        }
    };
    let no_back = || {
        if [BACK_PAGE, BACK_DISMISSED, BACK_GAME]
            .iter()
            .any(|b| has(b))
        {
            Err("a back without the Esc -> Back adaptation (AOSP: Esc has no fallback)".to_owned())
        } else {
            Ok("no back (AOSP map)".to_owned())
        }
    };
    let (page_back, dismiss, game) = if esc_back {
        (
            line("Esc on the page -> BACK -> WebView.goBack", BACK_PAGE),
            line(
                "Esc, no history -> BACK -> the SDK removed its WebView",
                BACK_DISMISSED,
            ),
            line("Esc on the GL view -> BACK -> the activity", BACK_GAME),
        )
    } else {
        (no_back(), no_back(), no_back())
    };
    let keys = (
        app.matches(PAGE_KEY).count(),
        app.matches(&format!("{GL_KEY}111")).count(),
    );
    let want = esc_targets(esc_back, focused);
    let esc_keys = if keys == want {
        Ok(format!(
            "Escs at the page {}, at the GL view {} (KEYCODE_ESCAPE)",
            keys.0, keys.1
        ))
    } else {
        Err(format!(
            "Escs at the page {}, at the GL view {}; want {} and {}",
            keys.0, keys.1, want.0, want.1
        ))
    };
    let children = match app
        .lines()
        .find_map(|l| l.split(WEB_CHILDREN).nth(1))
        .map(str::trim)
    {
        Some("1") => Ok(
            "the WebView's only child is the app's (removeViewAt(0) took the indicator)".to_owned(),
        ),
        Some(n) => Err(format!(
            "WebView.getChildCount() {n} with one app child: the provider's own View is visible to the app"
        )),
        None => Err(format!("no `{WEB_CHILDREN}` line")),
    };
    vec![
        ("loaded", line("onPageFinished", LOADED)),
        ("web-children", children),
        ("resize", resize_check(app)),
        (
            "page-tap",
            line(
                "the click reached the page (title tap1)",
                &format!("{TITLE}tap1"),
            ),
        ),
        (
            "page-scroll",
            line(
                "the wheel scrolled the page (title scrolled)",
                &format!("{TITLE}scrolled"),
            ),
        ),
        ("tap-focus", tap_focus_check(app, focused)),
        ("esc-keys", esc_keys),
        ("esc-page-back", page_back),
        ("esc-dismiss", dismiss),
        ("esc-game", game),
        ("focus-return", focus_return_check(app)),
        ("hidden-preload", preload_check(app)),
        ("software-layer", software_layer_check(app)),
    ]
}

/// Sub-gate resize: the WebView's last placement covers the fixture's last window size (AOSP lays the View out with its window: a stale rectangle shows the game around the page).
/// The configure sizes the toplevel, chrome included, so the content's height is not the configured one; the configured width is (no side chrome on the gate output, measured).
fn resize_check(app: &str) -> Result<String, String> {
    let (w, h) = window_size(app).ok_or("no layout line from the fixture")?;
    if i64::from(w) != i64::from(RESIZE.0) {
        return Err(format!(
            "fixture window {w}x{h} after the resize, configured width {}",
            RESIZE.0
        ));
    }
    let dst = last_placed(app).ok_or("no sideband placement line")?;
    if dst != [0, 0, i64::from(w), i64::from(h)] {
        return Err(format!("last placement dst={dst:?}, window {w}x{h}"));
    }
    // the page's own viewport, not only the rectangle: a widget placed right whose content kept its old size shows the game below the page (owner screenshot 2026-11-18)
    let vp = app
        .lines()
        .rev()
        .find_map(|l| l.split(&format!("{TITLE}viewport")).nth(1))
        .and_then(|s| s.trim().split_once('x'))
        .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
        .ok_or("no viewport title from the page after the resize")?;
    // CSS px = physical / density, unknown here: compare aspect; integer CSS sizes round, so allow one CSS px on either axis (|iw*h - ih*w| <= max(w, h))
    let (iw, ih) = vp;
    let (w, h) = (i64::from(w), i64::from(h));
    if !viewport_fits(vp, (w, h)) {
        return Err(format!(
            "page viewport {iw}x{ih} CSS px does not match the view {w}x{h}"
        ));
    }
    Ok(format!(
        "placement and page viewport follow the window: dst 0,0,{w},{h}, viewport {iw}x{ih} CSS px"
    ))
}

/// The page's viewport `(iw, ih)` (CSS px) has the view's `(w, h)` (physical px) aspect. CSS px = physical / density, unknown here: compare aspect; integer CSS sizes round, so allow one CSS px
/// on either axis (|iw*h - ih*w| <= max(w, h)).
fn viewport_fits((iw, ih): (i64, i64), (w, h): (i64, i64)) -> bool {
    (iw * h - ih * w).abs() <= w.max(h)
}

/// The resize `layouts_before` preceded is answered: the fixture logged a new layout, and after that layout the page's viewport title has its aspect and the WebView's last placement is its
/// rectangle. Input before all three can land outside the page: a click at y=380 sent while the page was still 361 px high (its 761 px viewport title came 1.1 s later) reached Android
/// (focus moved) but not the page (no tap1), with renders at 1-2.7 s (GPU starved + load 11, 2026-11-22).
fn resize_answered(app: &str, layouts_before: usize) -> bool {
    if app.matches(LAYOUT).count() <= layouts_before {
        return false;
    }
    let (Some((w, h)), Some(at)) = (window_size(app), app.rfind(LAYOUT)) else {
        return false;
    };
    let (w, h) = (i64::from(w), i64::from(h));
    let viewport = app[at..]
        .lines()
        .filter_map(|l| l.split(&format!("{TITLE}viewport")).nth(1))
        .filter_map(|s| s.trim().split_once('x'))
        .filter_map(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
        .any(|vp| viewport_fits(vp, (w, h)));
    viewport && last_placed(app) == Some([0, 0, w, h])
}

fn esc(proxy: &Proxy) -> Result<(), String> {
    proxy.inject_key(KEY_ESC, true).map_err(|e| e.to_string())?;
    proxy.inject_key(KEY_ESC, false).map_err(|e| e.to_string())
}

/// The probe: configure acked, page loaded and painted; resize; click the page's button; three Escs, each waiting for its answer (a missing answer is its sub-gate's failure, not the probe's).
pub fn drive(proxy: Proxy, app_log: &Path, origin: TapOrigin) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, LOADED, ACK_WAIT)
        .then_some(())
        .ok_or("the page never finished loading")?;
    // the hidden preload loads too before any input: shown by mistake, it is the topmost widget and takes the click (its sub-gate reports a missing load)
    let _ = wait_for_line(app_log, PRELOAD_LOADED, ACK_WAIT);
    std::thread::sleep(PAINT_SETTLE);
    // each resize waits for its answers, not a fixed time ([`resize_answered`]); one never answered is the resize sub-gate's failure, and the probe goes on after ACK_WAIT
    for (w, h, serial) in RESIZES {
        let layouts = std::fs::read_to_string(app_log)
            .map(|t| t.matches(LAYOUT).count())
            .unwrap_or(0);
        proxy
            .inject_configure(w, h, serial, &[])
            .map_err(|e| e.to_string())?;
        let _ = await_event(&proxy.events, ACK_WAIT, |e| {
            *e == ProxyEvent::Acked { serial }
        });
        let _ = widgets::wait_until(app_log, ACK_WAIT, |t| resize_answered(t, layouts));
    }
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = super::content_origin(&text)?;
    let (cw, ch) = window_size(&text).ok_or("no layout line from the fixture")?;
    let centre = (
        f64::from(cw / 2) + f64::from(ox),
        f64::from(ch / 2) + f64::from(oy),
    );
    // the pointer enters either way (the wheel after the tap needs it); only the press differs
    widgets::inject(
        &proxy,
        vec![
            PointerAction::Enter {
                x: centre.0,
                y: centre.1,
            },
            PointerAction::Motion {
                x: centre.0,
                y: centre.1,
            },
        ],
    )?;
    match origin {
        TapOrigin::Mouse | TapOrigin::MouseFinger => {
            widgets::inject(&proxy, vec![widgets::press(true), widgets::press(false)])?
        }
        TapOrigin::Touch => {
            let (x, y) = centre;
            proxy
                .inject_touch(TouchAction::Down { id: 0, x, y })
                .map_err(|e| e.to_string())?;
            proxy
                .inject_touch(TouchAction::Up { id: 0 })
                .map_err(|e| e.to_string())?;
        }
    }
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| t.contains(&format!("{TITLE}tap1")));
    widgets::inject(
        &proxy,
        vec![PointerAction::Wheel {
            notches: WHEEL_NOTCHES,
        }],
    )?;
    let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| {
        t.contains(&format!("{TITLE}scrolled"))
    });
    // each Esc waits for one more back line of any kind, so a wrong answer does not cost the next Esc its time
    let backs = |t: &str| t.matches("AtlWindow: wvo back ").count();
    for _ in 0..3 {
        let before = std::fs::read_to_string(app_log)
            .map(|t| backs(&t))
            .unwrap_or(0);
        esc(&proxy)?;
        let _ = widgets::wait_until(app_log, INPUT_WAIT, |t| backs(t) > before);
    }
    Ok(Liveness::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sub-gates on hand-written logs: the full answer passes with esc-back; a stale placement, a missing tap and a missing back each fail.
    #[test]
    fn sub_gates_by_hand() {
        let ok = "x AtlWindow: wvo web children 1\nx AtlWindow: wvo page finished\nx AtlWindow: wvo layout window=720x520 web=0,0,720,520\n\
                  y atl-platform-ui: sideband [1, 5] placed dst=0,0,720,520 clip=0,0,720,520 alpha=1\n\
                  x AtlWindow: wvo focus gl\nx AtlWindow: wvo title viewport360x260\nx AtlWindow: wvo focus web\nx AtlWindow: wvo title tap1\nx AtlWindow: wvo title scrolled\n\
                  x AtlWindow: wvo page key 1Escape\nx AtlWindow: wvo back page\nx AtlWindow: wvo page key 2Escape\nx AtlWindow: wvo focus gl\nx AtlWindow: wvo back dismissed\nx AtlWindow: wvo gl key 111\nx AtlWindow: wvo back game\n\
                  x AtlWindow: wvo preload finished\nx AtlWindow: wvo swlayer web=40,40,200,160\n\
                  y atl-platform-ui: sideband [1, 7] placed dst=40,40,200,160 clip=40,40,200,160 alpha=1\n";
        assert!(checks(ok, true, true).iter().all(|(_, r)| r.is_ok()));
        // the Angry Birds 2 account panel: the software-layer WebView's carrier never placed, or left at the layer's origin
        let unplaced = ok.replace(
            "[1, 7] placed dst=40,40,200,160",
            "[1, 7] placed dst=0,0,160,120",
        );
        assert!(software_layer_check(&unplaced).is_err());
        assert!(software_layer_check(&ok.replace("wvo swlayer", "wvo other")).is_err());
        // the Angry Birds 2 case: the hidden preload's layer shown (a second stream placed) and taking the click
        let shown = ok.replace(
            "x AtlWindow: wvo preload finished\n",
            "x AtlWindow: wvo preload finished\ny atl-platform-ui: sideband [1, 6] placed dst=0,0,720,520 clip=0,0,720,520 alpha=1\n",
        );
        assert!(preload_check(&shown).is_err());
        let tapped = format!("{ok}x AtlWindow: wvo preload title ptap\n");
        assert!(preload_check(&tapped).is_err());
        let stale = ok.replace("dst=0,0,720,520", "dst=0,0,960,600");
        assert!(resize_check(&stale).is_err());
        // the owner's case: placed right, page kept its old height
        let old_page = ok.replace("viewport360x260", "viewport360x300");
        assert!(resize_check(&old_page).is_err());
        // the Battle Cats case: the provider's carrier counted as a child, so the app's removeViewAt(0) took the page off the screen
        let carrier_child = ok.replace("wvo web children 1", "wvo web children 2");
        let sub = |log: &str, esc_back: bool, focused: bool, name: &str| {
            checks(log, esc_back, focused)
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| r)
                .expect("sub-gate")
        };
        assert!(sub(&carrier_child, true, true, "web-children").is_err());
        let no_tap = ok.replace("wvo title tap1", "wvo title x");
        assert!(sub(&no_tap, true, true, "page-tap").is_err());
        let no_back = ok.replace("wvo back page", "");
        assert!(sub(&no_back, true, true, "esc-page-back").is_err());
        let no_scroll = ok.replace("wvo title scrolled", "");
        assert!(sub(&no_scroll, true, true, "page-scroll").is_err());
        // esc-keys counts the page's bridge lines only: one missing fails; a key title (the old signal, unordered with the dismissal's destroy) neither adds nor replaces one
        let one_key = ok.replacen("x AtlWindow: wvo page key 2Escape\n", "", 1);
        assert!(sub(&one_key, true, true, "esc-keys").is_err());
        let titled = one_key.replacen(
            "x AtlWindow: wvo back page\n",
            "x AtlWindow: wvo back page\nx AtlWindow: wvo title key2Escape\n",
            1,
        );
        assert!(sub(&titled, true, true, "esc-keys").is_err());
        // without the adaptation the same backs fail
        assert!(sub(ok, false, true, "esc-page-back").is_err());
        // the origin's break: a mouse run judged by the touch rule fails tap-focus and esc-keys
        assert!(
            sub(ok, true, false, "tap-focus").is_err() && sub(ok, true, false, "esc-keys").is_err()
        );
        // the Angry Birds 2 guard: the WebView removed but focus left nowhere (or on the gone WebView) fails focus-return
        let lost = ok.replacen(
            "x AtlWindow: wvo focus gl\nx AtlWindow: wvo back dismissed",
            "x AtlWindow: wvo focus none\nx AtlWindow: wvo back dismissed",
            1,
        );
        assert!(sub(&lost, true, true, "focus-return").is_err());
    }

    /// The red run's order (2026-11-22): the 761 layout, the late 361 viewport title, the 761 placement -- not answered (the click then went outside the page); the 761 viewport title answers it.
    /// No layout past the count before the configure: not answered, whatever else is logged.
    #[test]
    fn a_resize_is_answered_by_layout_viewport_and_placement() {
        let before = "x AtlWindow: wvo layout window=720x361 web=0,0,720,361\n\
                      y atl-platform-ui: sideband [1, 1] placed dst=0,0,720,361 clip=0,0,720,361 alpha=1\n\
                      x AtlWindow: wvo layout window=720x761 web=0,0,720,761\nx AtlWindow: wvo title viewport720x361\n\
                      y atl-platform-ui: sideband [1, 1] placed dst=0,0,720,761 clip=0,0,720,761 alpha=1\n";
        assert!(!resize_answered(before, 1));
        let after = format!("{before}x AtlWindow: wvo title viewport720x761\n");
        assert!(resize_answered(&after, 1));
        assert!(!resize_answered(&after, 2));
    }

    /// Where the Escs go, from AOSP's routing (key -> focused View; the page leaves Esc unhandled): touch tap -> all 3 at the GL view either map; mouse click -> AOSP map 3 at the page;
    /// `--esc-back` 2 at the page (goBack, dismiss), then focus returns and the third reaches the GL view. A touch run with the old GL-only answer passes the touch rule, fails the mouse one.
    #[test]
    fn esc_targets_follow_focus() {
        assert_eq!(esc_targets(false, false), (0, 3));
        assert_eq!(esc_targets(true, false), (0, 3));
        assert_eq!(esc_targets(false, true), (3, 0));
        assert_eq!(esc_targets(true, true), (2, 1));
        let touch = "x AtlWindow: wvo focus gl\nx AtlWindow: wvo gl key 111\nx AtlWindow: wvo gl key 111\nx AtlWindow: wvo gl key 111\n";
        assert!(tap_focus_check(touch, false).is_ok() && tap_focus_check(touch, true).is_err());
    }
}
