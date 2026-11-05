// SPDX-License-Identifier: MIT
//! Stage `widgets` of `xtask test window`: the widget fixture (xtask/testdata/widgets, real resources under the Material theme) in UI mode, checked from the fixture's log lines.
//! The probe taps the Button and the EditText and drags the ListView, at the fixture's own rectangles shifted by the content origin the platform logs (the proxy injects surface points).
//! Sub-gates ([`SUB_NAMES`]): layout (geometry from the XML), text (the software frame logged at first layout is the readback's digest, so the text is in the capture),
//! click, focus, list, presents (more than 2 scenes), cadence (the frame callbacks, as the frames stage).

use std::path::Path;
use std::time::{Duration, Instant};

use atl_services::display_model::DisplayModel;

use super::{
    ACK_WAIT, BTN_LEFT, DRAW_LINE, Liveness, POLL, await_event, cadence_check, capture_line,
    content_origin, wait_for_line,
};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

/// The fixture's dir.
pub const FIXTURE: &str = "xtask/testdata/widgets";
/// The sub-gates, in print order (each prints `window: <name>: pass|FAIL`).
pub const SUB_NAMES: &[&str] = &[
    "layout", "text", "click", "focus", "list", "presents", "cadence",
];
/// How long a fixture line may take after the injection that should cause it.
const LINE_WAIT: Duration = Duration::from_secs(5);
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

/// The widgets stage's sub-gates in [`SUB_NAMES`] order, from the app log and the frame callbacks after the marker.
pub fn checks(app: &str, after: &[&str]) -> Vec<Result<String, String>> {
    vec![
        layout_check(app),
        text_check(app),
        click_check(app),
        focus_check(app),
        list_check(app),
        presents_check(app),
        cadence_check(after, DisplayModel::default().refresh_hz),
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

/// The widgets probe: after the first draw and the readback (so the readback shows the frame before the taps), taps the Button, taps the EditText, drags the ListView up.
/// Each injection waits for the fixture's line for it. The points are the fixture's rectangles' centres, shifted by the content origin. No liveness probe runs here.
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    wait_for_line(app_log, "atl-composer: ui: capture ", ACK_WAIT)
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
    tap(centre("button")?)?;
    wait_line(app_log, "AtlWindow: click count=1")?;
    tap(centre("input")?)?;
    wait_line(app_log, "AtlWindow: focus input=true")?;
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
    Ok(Liveness {
        answered: false,
        silent: false,
        resumed_ms: None,
    })
}
