// SPDX-License-Identifier: MIT
//! Stage `widgets` of `xtask test window`: the widget fixture (xtask/testdata/widgets, real resources under the Material theme) in UI mode, checked from the fixture's log lines.
//! The probe presses the header bar, then taps the Button and the EditText and drags the ListView, at the fixture's own rectangles shifted by the content origin the platform logs (the proxy injects surface points).
//! Sub-gates ([`SUB_NAMES`]): probe (every injection got its answer), layout (geometry from the XML), text (the fixture's settled software frame is the first readback's digest, so the text is in the capture),
//! header (the press reached the client, no touch reached Android), click, focus, window (the second activity the focus starts and finishes leaves input on Main), list, readback (the last readback is the fixture's final frame and differs from the first),
//! presents (more than 2 scenes), cadence (the frame callbacks while the list redraws, as the frames stage).

use std::path::Path;
use std::time::{Duration, Instant};

use atl_services::display_model::DisplayModel;

use super::{
    ACK_WAIT, BTN_LEFT, Liveness, POLL, await_event, cadence_check, capture_line, content_origin,
    wait_for_line,
};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask/testdata/widgets";
/// The sub-gates, in print order (each prints `window: <name>: pass|FAIL`).
pub const SUB_NAMES: &[&str] = &[
    "probe", "layout", "text", "header", "click", "focus", "window", "list", "readback",
    "presents", "cadence", "pacing", "window-focus",
];
/// The fixture's first-frame line, after which it takes taps (`first` exists for `changed`).
const SOFTWARE_LINE: &str = "AtlWindow: software frame";
/// The fixture's line after the cadence run: the picture the last readback must show.
const FINAL_LINE: &str = "AtlWindow: final frame";
const CAPTURE_LINE: &str = "atl-composer: ui: capture ";
/// The fixture's lines around its second window (activity .Second, started by the EditText's focus; `closed` once Main is back and Second's window is gone).
const SECOND_SHOWN: &str = "AtlWindow: window second shown";
const SECOND_CLOSED: &str = "AtlWindow: window second closed";
/// A surface point in the header bar: below the shadow's 12 px, above the content's origin (as the gtk stage's chrome press).
const HEADER_POINT: (f64, f64) = (100.0, 20.0);
/// How long a fixture line may take after the injection that should cause it.
const LINE_WAIT: Duration = Duration::from_secs(5);
/// The run's display, smaller than the default: WSLg paces a full-size window at about half the display rate whatever draws it (vkcube: 31 ms frame callbacks at 1308x875, 17 ms at 640x400), and the cadence gate needs a host that keeps the display rate.
/// The size is this run's; the refresh rate is the monitor's (`monitor`, from [`crate::atld_run::run_display`]).
pub fn display(monitor: DisplayModel) -> DisplayModel {
    DisplayModel {
        width: 640,
        height: 400,
        ..monitor
    }
}

/// A tap's injections: enter, press, release.
const HEADER_ACTIONS: u32 = 3;
/// The list drag: DRAG_PX up in SCROLL_STEPS motions.
const DRAG_PX: f64 = 200.0;
const SCROLL_STEPS: u32 = 10;
/// The expected geometry from the layout XML: `android:padding="16dp"`, the list's `android:layout_height="200dp"`.
const PADDING_DP: f32 = 16.0;
const LIST_DP: f32 = 200.0;

/// A widget's rectangle in the content, from its `AtlWindow: layout <name>` line.
#[derive(Debug, Clone, Copy)]
struct Rect {
    left: i64,
    top: i64,
    width: i64,
    height: i64,
}

/// The `key=` value on the first app log line containing `prefix`.
fn field<'a>(app: &'a str, prefix: &str, key: &str) -> Option<&'a str> {
    app.lines()
        .find(|l| l.contains(prefix))
        .and_then(|l| word(l, key))
}

/// The text after `prefix` on the first app log line containing it.
fn rest_of<'a>(app: &'a str, prefix: &str) -> Option<&'a str> {
    app.lines()
        .find_map(|l| l.split_once(prefix).map(|(_, rest)| rest))
}

fn number<T: std::str::FromStr>(value: Option<&str>, what: &str) -> Result<T, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("no {what} in the app log"))
}

fn widget(app: &str, name: &str) -> Result<Rect, String> {
    let rest = rest_of(app, &format!("AtlWindow: layout {name} "))
        .ok_or_else(|| format!("no layout line for {name}"))?;
    let num = |key: &str| number::<i64>(word(rest, key), &format!("{name} {key}"));
    Ok(Rect {
        left: num("left")?,
        top: num("top")?,
        width: num("width")?,
        height: num("height")?,
    })
}

/// The top of the list's visible items, the largest the fixture logged.
fn list_first(app: &str) -> u32 {
    app.lines()
        .filter_map(|l| l.split_once("AtlWindow: list first="))
        .filter_map(|(_, v)| v.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

fn layout_check(app: &str) -> Result<String, String> {
    let density: f32 = number(field(app, "AtlWindow: density=", "density"), "density")?;
    let size = field(app, "AtlWindow: software frame", "size")
        .ok_or("no software frame line (the fixture's first layout)")?;
    let frame_w: i64 = size
        .split('x')
        .next()
        .and_then(|w| w.parse().ok())
        .ok_or_else(|| format!("software frame size {size:?}"))?;
    let (label, button, input, list) = (
        widget(app, "label")?,
        widget(app, "button")?,
        widget(app, "input")?,
        widget(app, "list")?,
    );
    let pad = (PADDING_DP * density).round() as i64;
    for (name, r) in [
        ("label", label),
        ("button", button),
        ("input", input),
        ("list", list),
    ] {
        if r.width <= 0 || r.height <= 0 {
            return Err(format!("{name} measures {}x{}", r.width, r.height));
        }
        if r.left != pad {
            return Err(format!(
                "{name} starts at x={}, the padding is {pad}",
                r.left
            ));
        }
    }
    let content_w = frame_w - 2 * pad;
    for (name, r) in [("label", label), ("input", input), ("list", list)] {
        if r.width != content_w {
            return Err(format!(
                "{name} is {} px wide, match_parent is {content_w}",
                r.width
            ));
        }
    }
    if ![label, button, input, list]
        .windows(2)
        .all(|p| p[0].top + p[0].height <= p[1].top)
    {
        return Err("the widgets are not stacked in XML order".to_owned());
    }
    let list_h = (LIST_DP * density).round() as i64;
    if list.height != list_h {
        return Err(format!(
            "list is {} px tall, 200dp is {list_h}",
            list.height
        ));
    }
    Ok(format!(
        "4 widgets measured, stacked from the {pad} px padding in XML order; list {list_h} px (density {density})"
    ))
}

fn text_check(app: &str) -> Result<String, String> {
    let software =
        field(app, "AtlWindow: software frame", "fnv1a64").ok_or("no software frame line")?;
    let readback = word(capture_line(app)?, "fnv1a64").ok_or("no digest in the readback line")?;
    let label: u64 = number(field(app, "AtlWindow: pixels ", "label"), "label ink")?;
    let input: u64 = number(field(app, "AtlWindow: pixels ", "input"), "input ink")?;
    if readback != software {
        return Err(format!(
            "readback {readback}, the fixture's software frame {software}"
        ));
    }
    if label == 0 || input == 0 {
        return Err(format!("no text pixels: label {label}, input {input}"));
    }
    Ok(format!(
        "readback {readback} = the software frame; text pixels inside the label {label}, the input {input}"
    ))
}

/// `drive` saw the proxy report the header press before the first tap. GTK keeps a header press, so the first touch Android gets must be the Button's tap.
fn header_check(app: &str) -> Result<String, String> {
    let (ox, oy) = button_centre(app)?;
    let line = app
        .lines()
        .find_map(|l| l.split_once("AtlWindow: touch ").map(|(_, rest)| rest))
        .ok_or("no touch line at all")?;
    let x: f64 = number(word(line, "x"), "touch x")?;
    let y: f64 = number(word(line, "y"), "touch y")?;
    if (x, y) != (ox, oy) {
        return Err(format!(
            "first touch at ({x}, {y}), the Button's centre is ({ox}, {oy}): the header press reached Android"
        ));
    }
    Ok("proxy injected the press; the first touch Android got is the Button's tap".to_owned())
}

/// The centre of the Button in the content, where the probe taps.
fn button_centre(app: &str) -> Result<(f64, f64), String> {
    let r = widget(app, "button")?;
    Ok(((r.left + r.width / 2) as f64, (r.top + r.height / 2) as f64))
}

/// After the taps and the drag: the last settled readback is the fixture's final frame, and not the first picture.
fn readback_check(app: &str) -> Result<String, String> {
    let wanted = field(app, FINAL_LINE, "fnv1a64").ok_or("no final frame line")?;
    let first = word(capture_line(app)?, "fnv1a64").ok_or("no digest in the first readback")?;
    let last = app
        .lines()
        .rfind(|l| l.contains(CAPTURE_LINE))
        .and_then(|l| word(l, "fnv1a64"))
        .ok_or("no digest in the last readback")?;
    if last != wanted {
        return Err(format!("last readback {last}, the final frame {wanted}"));
    }
    if last == first {
        return Err(format!("the readback stayed {first} through the input"));
    }
    Ok(format!(
        "last readback {last} = the final frame, first readback {first}"
    ))
}

fn click_check(app: &str) -> Result<String, String> {
    let count: u32 = number(
        field(app, "AtlWindow: click ", "count"),
        "click line (the OnClickListener did not run)",
    )?;
    let text =
        rest_of(app, "AtlWindow: text label=").ok_or("no label text line after the click")?;
    let changed: u64 = number(
        field(app, "AtlWindow: changed ", "label"),
        "changed line after the click",
    )?;
    if count != 1 {
        return Err(format!("OnClickListener ran {count} times, want 1"));
    }
    if text != "Clicked 1" {
        return Err(format!(
            "label reads {text:?} after the click, want \"Clicked 1\""
        ));
    }
    if changed == 0 {
        return Err("no pixel changed inside the label after the click".to_owned());
    }
    Ok(format!(
        "OnClickListener ran once; label reads {text:?}; {changed} px changed inside the label"
    ))
}

fn focus_check(app: &str) -> Result<String, String> {
    match rest_of(app, "AtlWindow: focus input=") {
        Some("true") => Ok("the touch focused the EditText".to_owned()),
        other => Err(format!("EditText focus line {other:?}, want true")),
    }
}

/// The fixture started and finished its second activity, and the Button tap after the close reached Main (its second click).
fn window_check(app: &str) -> Result<String, String> {
    let at = |needle: &str| {
        app.find(needle)
            .ok_or_else(|| format!("no `{needle}` line"))
    };
    let (shown, closed, click) = (
        at(SECOND_SHOWN)?,
        at(SECOND_CLOSED)?,
        at("AtlWindow: click count=2")?,
    );
    if !(shown < closed && closed < click) {
        return Err("the second click came before the second window closed".to_owned());
    }
    Ok("second window shown and closed; the tap after it reached Main (click 2)".to_owned())
}

fn list_check(app: &str) -> Result<String, String> {
    match list_first(app) {
        0 => Err("the drag did not scroll the list: first visible item stayed 0".to_owned()),
        first => Ok(format!(
            "drag scrolled the list: first visible item {first} of 50"
        )),
    }
}

fn presents_check(app: &str) -> Result<String, String> {
    let scenes: Vec<i64> = app
        .lines()
        .filter_map(|l| {
            let (_, rest) = l.split_once("atl-composer: ui: present ")?;
            word(rest, "time_ns")?.parse().ok()
        })
        .collect();
    let n = scenes.len();
    if n <= 2 {
        return Err(format!("{n} scenes presented, want more than 2"));
    }
    let mean_ms = (scenes[n - 1] - scenes[0]) as f64 / (n - 1) as f64 / 1e6;
    Ok(format!(
        "{n} scenes presented; mean interval {mean_ms:.3} ms"
    ))
}

/// The widgets stage's sub-gates in [`SUB_NAMES`] order, from the probe's result, the app log and the frame callbacks after the marker.
pub fn checks(
    app: &str,
    after: &[&str],
    probe: &Result<Liveness, String>,
    display: DisplayModel,
) -> Vec<Result<String, String>> {
    vec![
        probe
            .as_ref()
            .map(|_| "every injection got its answer".to_owned())
            .map_err(Clone::clone),
        layout_check(app),
        text_check(app),
        header_check(app),
        click_check(app),
        focus_check(app),
        window_check(app),
        list_check(app),
        readback_check(app),
        presents_check(app),
        cadence_check(after, display.refresh_hz),
        pacing_check(app, display.refresh_hz),
        window_focus_check(app),
    ]
}

fn press(pressed: bool) -> PointerAction {
    PointerAction::Button {
        button: BTN_LEFT,
        pressed,
    }
}

fn inject(proxy: &Proxy, actions: Vec<PointerAction>) -> Result<(), String> {
    for action in actions {
        proxy.inject_pointer(action).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Whether the app log satisfies `want` within `within` (polled: the app process writes the log).
fn wait_until(app_log: &Path, within: Duration, want: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if std::fs::read_to_string(app_log).is_ok_and(|text| want(&text)) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

fn wait_line(app_log: &Path, needle: &str) -> Result<(), String> {
    wait_for_line(app_log, needle, LINE_WAIT)
        .then_some(())
        .ok_or_else(|| format!("no `{needle}` line within {LINE_WAIT:?}"))
}

/// The widgets probe: after the software frame and the first readback (so both show the picture before the input), presses the header, taps the Button, taps the EditText, waits for the second window to close and taps the Button again, drags the ListView up, and waits for the readback of the final frame.
/// Each injection waits for its answer: the proxy's report for the header, the fixture's line for the rest. The points are the fixture's rectangles' centres, shifted by the content origin. No liveness probe runs here.
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, SOFTWARE_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never logged its software frame")?;
    wait_for_line(app_log, CAPTURE_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the window never read back")?;
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = content_origin(&text)?;
    let centre = |name: &str| -> Result<(f64, f64), String> {
        let r = widget(&text, name)?;
        Ok((
            (r.left + r.width / 2) as f64 + f64::from(ox),
            (r.top + r.height / 2) as f64 + f64::from(oy),
        ))
    };
    let tap = |(x, y): (f64, f64)| {
        inject(
            &proxy,
            vec![PointerAction::Enter { x, y }, press(true), press(false)],
        )
    };
    tap(HEADER_POINT)?;
    for n in 1..=HEADER_ACTIONS {
        await_event(&proxy.events, LINE_WAIT, |e| {
            *e == ProxyEvent::PointerInjected
        })
        .ok_or_else(|| {
            format!(
                "the proxy reported {} of the header press's {HEADER_ACTIONS} actions",
                n - 1
            )
        })?;
    }
    tap(centre("button")?)?;
    wait_line(app_log, "AtlWindow: click count=1")?;
    tap(centre("input")?)?;
    wait_line(app_log, "AtlWindow: focus input=true")?;
    // the focus opened the second window; once it is closed, a tap must reach Main again
    wait_line(app_log, SECOND_CLOSED)?;
    tap(centre("button")?)?;
    wait_line(app_log, "AtlWindow: click count=2")?;
    let (x, y) = centre("list")?;
    let mut drag = vec![PointerAction::Enter { x, y }, press(true)];
    for step in 1..=SCROLL_STEPS {
        drag.push(PointerAction::Motion {
            x,
            y: y - DRAG_PX * f64::from(step) / f64::from(SCROLL_STEPS),
        });
    }
    drag.push(press(false));
    inject(&proxy, drag)?;
    if !wait_until(app_log, LINE_WAIT, |t| list_first(t) > 0) {
        return Err("the list did not scroll after the drag".to_owned());
    }
    wait_line(app_log, FINAL_LINE)?;
    // the readback of the final frame comes after the fixture's line, once the scenes stop
    if !wait_until(app_log, LINE_WAIT, |t| {
        t.rfind(CAPTURE_LINE) > t.find(FINAL_LINE)
    }) {
        return Err("no readback after the final frame".to_owned());
    }
    Ok(Liveness {
        answered: false,
        silent: false,
        resumed_ms: None,
        configured: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Button at left 16, top 49, 88x48: centre (60, 73), as a real run logs it.
    const BUTTON: &str = "AtlWindow: layout button left=16 top=49 width=88 height=48\n";

    #[test]
    fn header_check_wants_the_buttons_tap_first() {
        let tap = format!("{BUTTON}AtlWindow: touch action=0 x=60.0 y=73.0\n");
        assert!(header_check(&tap).is_ok());
        // a header press that leaked: content y is above the content's origin
        let leak = format!("{BUTTON}AtlWindow: touch action=0 x=86.0 y=-31.0\n{tap}");
        assert!(header_check(&leak).is_err());
        assert!(header_check(BUTTON).is_err());
    }

    #[test]
    fn readback_check_wants_the_final_frame_and_a_change() {
        let log = |first: &str, last: &str, want: &str| {
            format!(
                "atl-composer: ui: capture presented=5 size=640x400 fnv1a64={first}\nAtlWindow: final frame fnv1a64={want}\natl-composer: ui: capture presented=9 size=640x400 fnv1a64={last}\n"
            )
        };
        assert!(readback_check(&log("aa", "bb", "bb")).is_ok());
        assert!(readback_check(&log("aa", "cc", "bb")).is_err());
        assert!(readback_check(&log("bb", "bb", "bb")).is_err());
    }
}

/// The platform frame clock's stats line (atl-platform-ui-linux window.rs, every ~2 s of ticking).
const PACING_LINE: &str = "atl-platform-ui: frame clock paced=";

/// Sub-gate pacing: over the stats windows that had idle ticks (a tick after one that presented nothing), ticks come at the display rate within CADENCE_TOLERANCE, not at GDK's 60 Hz fallback.
/// On a 60 Hz host the two agree, so there this proves the idle path runs and keeps the rate; telling them apart needs a faster monitor.
fn pacing_check(app: &str, refresh_hz: f32) -> Result<String, String> {
    let field = |l: &str, key: &str| -> Option<f64> {
        l.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .and_then(|v| v.parse().ok())
    };
    let (mut ticks, mut us, mut windows) = (0.0, 0.0, 0);
    for line in app.lines().filter(|l| l.contains(PACING_LINE)) {
        let (Some(paced), Some(idle), Some(over)) = (
            field(line, "paced="),
            field(line, "idle="),
            field(line, "over_us="),
        ) else {
            return Err(format!("unreadable stats line: {line}"));
        };
        if idle > 0.0 {
            ticks += paced + idle;
            us += over;
            windows += 1;
        }
    }
    if windows == 0 {
        return Err("no frame clock window with idle ticks".to_owned());
    }
    let rate = ticks / us * 1e6;
    let want = f64::from(refresh_hz);
    if (rate - want).abs() > super::CADENCE_TOLERANCE * want {
        return Err(format!(
            "{rate:.1} ticks/s over {windows} idle windows, display {want} Hz"
        ));
    }
    Ok(format!(
        "{rate:.1} ticks/s over {windows} idle windows, display {want} Hz"
    ))
}

#[cfg(test)]
mod pacing_tests {
    use super::pacing_check;

    /// 120 Hz display: 240 ticks in 2 s pass; GDK's 60 Hz fallback (121 ticks in 2 s) fails; no idle window fails.
    #[test]
    fn idle_ticks_at_the_display_rate() {
        let line =
            |p, i| format!("x atl-platform-ui: frame clock paced={p} idle={i} over_us=2000000\n");
        assert!(pacing_check(&line(40, 200), 120.0).is_ok());
        assert!(pacing_check(&line(1, 120), 120.0).is_err());
        assert!(pacing_check(&line(240, 0), 120.0).is_err());
    }
}

/// Sub-gate window-focus: the desktop's activation of the toplevel reached the app as window focus (GTK is-active -> InputEvent::Focus -> publishFocusEvent -> onWindowFocusChanged).
fn window_focus_check(app: &str) -> Result<String, String> {
    match rest_of(app, "AtlWindow: window focus=") {
        Some("true") => Ok("the active toplevel gave the window focus".to_owned()),
        other => Err(format!("window focus line {other:?}, want true")),
    }
}
