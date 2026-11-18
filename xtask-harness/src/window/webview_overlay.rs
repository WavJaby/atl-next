// SPDX-License-Identifier: MIT
//! Stage `webview-overlay` of `xtask test window`: an in-game survey's arrangement (Angry Birds 2, owner 2026-11-18): one activity, an animating focused GLSurfaceView, a full-window WebView added
//! over it at runtime (xtask-harness/testdata/webview-overlay, Main.java). UI mode on the nested labwc; every input goes through the Wayland proxy, so it takes the real GTK path (toplevel key
//! controller, keyboard focus, the sideband gate), not a headless injection.
//! Sub-gates ([`checks`]): loaded (onPageFinished), web-children (the WebView's getChildCount counts only the app's own child, which the fixture then removes by index as Battle Cats TW's consent SDK
//! does: a provider View among the children would be the one removed, taking the page off the screen), resize (after a configure, the WebView's last placement is the window: sideband dst == the fixture's window size), page-tap (a click on the
//! page's button reaches the page: title tap1), then three Escs (AOSP BACK semantics through the Esc -> Back adaptation; `--esc-back`): esc-page-back (WebView.canGoBack -> goBack),
//! esc-dismiss (no history left -> the SDK's onBackPressed removes the WebView), esc-game (the WebView gone -> the activity's back). Android focus stays on the GL view throughout (the click does
//! not move it), so each Esc goes there first and BACK is its fallback. hidden-preload: a second WebView loaded under an INVISIBLE parent (Angry Birds 2's survey SDK) is never placed and takes no click. software-layer: a third WebView under a software-layer ancestor (Angry Birds 2's account panel) is placed at its View's rectangle. Without `--esc-back` (AOSP's map: Esc has no fallback) the Escs reach the GL view as KEYCODE_ESCAPE and no back follows.

use std::path::Path;
use std::time::Duration;

use super::{ACK_WAIT, Liveness, Sub, await_event, wait_for_line, widgets};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

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
/// After the resize's ack: Android's relayout (one traversal) and the platform's placement (one frame); generous.
const RESIZE_SETTLE: Duration = Duration::from_secs(2);

/// The last `wvo layout window=<w>x<h>` size the fixture logged.
fn window_size(app: &str) -> Option<(u32, u32)> {
    let line = app.lines().rev().find(|l| l.contains(LAYOUT))?;
    let (w, h) = word(line, "window")?.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The last sideband placement `dst=l,t,r,b`.
fn last_placed(app: &str) -> Option<[i64; 4]> {
    let line = app
        .lines()
        .rev()
        .find(|l| l.contains(PLACED) && l.contains(" placed dst="))?;
    let v: Vec<i64> = word(line, "dst")?
        .split(',')
        .filter_map(|n| n.parse().ok())
        .collect();
    v.try_into().ok()
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
    let all = placements(app);
    let sw = software_rect(app);
    let software: Vec<&str> = all
        .iter()
        .filter(|(_, dst)| Some(*dst) == sw)
        .map(|(s, _)| *s)
        .collect();
    let mut streams: Vec<&str> = all
        .iter()
        .map(|(s, _)| *s)
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

/// The sub-gates.
pub fn checks(app: &str, esc_back: bool) -> Vec<Sub> {
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
        // AOSP's map: Esc stays KEYCODE_ESCAPE (111) and goes to the focused View, the GL view (the click does not move Android focus here; the toolkit's focus follows Android's)
        let esc_keys = app.matches(&format!("{GL_KEY}111")).count();
        (
            no_back(),
            no_back(),
            if esc_keys == 3 {
                Ok("each Esc reached the focused GL view as KEYCODE_ESCAPE".to_owned())
            } else {
                Err(format!("{esc_keys} Esc keys at the GL view, want 3"))
            },
        )
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
        ("esc-page-back", page_back),
        ("esc-dismiss", dismiss),
        ("esc-game", game),
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
    if (iw * h - ih * w).abs() > w.max(h) {
        return Err(format!(
            "page viewport {iw}x{ih} CSS px does not match the view {w}x{h}"
        ));
    }
    Ok(format!(
        "placement and page viewport follow the window: dst 0,0,{w},{h}, viewport {iw}x{ih} CSS px"
    ))
}

fn esc(proxy: &Proxy) -> Result<(), String> {
    proxy.inject_key(KEY_ESC, true).map_err(|e| e.to_string())?;
    proxy.inject_key(KEY_ESC, false).map_err(|e| e.to_string())
}

/// The probe: configure acked, page loaded and painted; resize; click the page's button; three Escs, each waiting for its answer (a missing answer is its sub-gate's failure, not the probe's).
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
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
    for (w, h, serial) in RESIZES {
        proxy
            .inject_configure(w, h, serial, &[])
            .map_err(|e| e.to_string())?;
        let _ = await_event(&proxy.events, ACK_WAIT, |e| {
            *e == ProxyEvent::Acked { serial }
        });
        std::thread::sleep(RESIZE_SETTLE);
    }
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = super::content_origin(&text)?;
    let (cw, ch) = window_size(&text).ok_or("no layout line from the fixture")?;
    let centre = (
        f64::from(cw / 2) + f64::from(ox),
        f64::from(ch / 2) + f64::from(oy),
    );
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
            widgets::press(true),
            widgets::press(false),
        ],
    )?;
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
                  x AtlWindow: wvo title viewport360x260\nx AtlWindow: wvo title tap1\nx AtlWindow: wvo title scrolled\nx AtlWindow: wvo back page\nx AtlWindow: wvo back dismissed\nx AtlWindow: wvo back game\n\
                  x AtlWindow: wvo preload finished\nx AtlWindow: wvo swlayer web=40,40,200,160\n\
                  y atl-platform-ui: sideband [1, 7] placed dst=40,40,200,160 clip=40,40,200,160 alpha=1\n";
        assert!(checks(ok, true).iter().all(|(_, r)| r.is_ok()));
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
        assert!(checks(&carrier_child, true)[1].1.is_err());
        let no_tap = ok.replace("wvo title tap1", "wvo title x");
        assert!(checks(&no_tap, true)[3].1.is_err());
        let no_back = ok.replace("wvo back page", "");
        assert!(checks(&no_back, true)[5].1.is_err());
        let no_scroll = ok.replace("wvo title scrolled", "");
        assert!(checks(&no_scroll, true)[4].1.is_err());
        // without the adaptation the same backs fail
        assert!(checks(ok, false)[6].1.is_err());
    }
}
