// SPDX-License-Identifier: MIT
//! Stage `widgets` of `xtask test window`: the widget fixture (xtask/testdata/widgets, real resources under the Material theme) in UI mode, checked from the fixture's log lines.
//! The probe presses the header bar, then taps the Button and the EditText, hovers, turns the wheel and swipes the touchpad over the ListView, and drags it, at the fixture's own rectangles shifted by the content origin the platform logs (the proxy injects surface points).
//! Sub-gates ([`checks`]): probe (every injection got its answer), layout (geometry from the XML), text (the fixture's settled software frame is the first readback's digest, so the text is in the capture),
//! header (the press reached the client, no touch reached Android), click, focus, window (the second activity the focus starts and finishes leaves input on Main), hover, wheel, swipe (fake finger + fling), list (the drag), readback (the last readback is the fixture's final frame and differs from the first),
//! presents (more than 2 scenes), cadence (the frame callbacks while the list redraws, as the frames stage), on-demand (the window clock stops once nothing asks for a frame).

use std::path::Path;
use std::time::{Duration, Instant};

use atl_services::display_model::DisplayModel;
use atl_services::input_devices;

use super::{
    ACK_WAIT, BTN_LEFT, Liveness, POLL, Sub, await_event, cadence_check, capture_line,
    content_origin, wait_for_line,
};
use crate::composer::word;
use crate::nested_wayland::{GATE_OUTPUT, Output};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy, TouchAction};
use atl_cpu_pressure::Samples;

/// The fixture's dir.
pub const FIXTURE: &str = "xtask/testdata/widgets";
/// The fixture's first-frame line, after which it takes taps (`first` exists for `changed`).
const SOFTWARE_LINE: &str = "AtlWindow: software frame";
/// The fixture's line after the cadence run: the picture the last readback must show.
const FINAL_LINE: &str = "AtlWindow: final frame";
pub(super) const CAPTURE_LINE: &str = "atl-composer: ui: capture ";
/// The fixture's lines around its second window (activity .Second, started by the EditText's focus; `closed` once Main is back and Second's window is gone).
const SECOND_SHOWN: &str = "AtlWindow: window second shown";
pub(super) const SECOND_CLOSED: &str = "AtlWindow: window second closed";
/// A surface point in the header bar: below the shadow's 12 px, above the content's origin (as the gtk stage's chrome press).
const HEADER_POINT: (f64, f64) = (100.0, 20.0);
/// How long a fixture line may take after the injection that should cause it.
const LINE_WAIT: Duration = Duration::from_secs(5);
/// The run's nested output, smaller than the gates' (as Android's display: NestedCompositor::display, like every nested stage): the cadence gate needs a compositor that keeps the display rate, and a large window costs it per frame (WSLg paced a full-size window at about half the rate: vkcube 31 ms frame callbacks at 1308x875, 17 ms at 640x400; labwc headless composites with pixman, also per pixel).
/// It holds the opening window (3/4 of each side, 480x300) plus its client-side chrome.
pub const OUTPUT: Output = Output {
    width: 640,
    height: 400,
    ..GATE_OUTPUT
};

/// A tap's injections: enter, press, release.
const HEADER_ACTIONS: u32 = 3;
/// The list drag: DRAG_PX up in SCROLL_STEPS motions.
const DRAG_PX: f64 = 200.0;
const SCROLL_STEPS: u32 = 10;
/// The expected geometry from the layout XML: `android:padding="16dp"`, the list's `android:layout_height="200dp"`.
const PADDING_DP: f32 = 16.0;
const LIST_DP: f32 = 200.0;
/// The hover over the list: moved this far from the list's centre, in surface pixels.
const HOVER_PX: f64 = 4.0;
/// The wheel: notches down. 5 notches x AbsListView's 64dp vertical scroll factor (ViewConfiguration config_verticalScrollFactor) scrolls several rows, far enough that the swipe back up and its fling stay below the top.
const WHEEL_NOTCHES: i32 = 5;
/// The touchpad swipe toward the top: SWIPE_STEPS axis events of SWIPE_STEP_PX each, SWIPE_STEP_MS apart (600 px/s: above Android's minimum fling velocity of 50dp/s, ViewConfiguration MINIMUM_FLING_VELOCITY), then the fingers lift.
const SWIPE_STEPS: u32 = 6;
const SWIPE_STEP_PX: f64 = 6.0;
const SWIPE_STEP_MS: u64 = 10;
/// How long the swipe's fling may run before the list is idle again.
const FLING_WAIT: Duration = Duration::from_secs(5);
/// A list position's row weight: larger than any row height, so (first row, its offset) orders as one number.
const LIST_ROW_SCALE: i64 = 100_000;
/// MotionEvent actions and the touchpad swipe's classification (NDK input.h).
const ACTION_DOWN: u32 = 0;
/// The fixture logs MotionEvent.getSource() / getDeviceId(); the values the composer stamps (atl-services input_devices).
const SOURCE_MOUSE: i64 = input_devices::SOURCE_MOUSE as i64;
const SOURCE_TOUCHSCREEN: i64 = input_devices::SOURCE_TOUCHSCREEN as i64;
const TOUCHSCREEN_DEVICE: i64 = input_devices::TOUCHSCREEN as i64;
/// The touch: two fingers TOUCH_GAP_PX apart across the list, moved up DRAG_PX in SCROLL_STEPS motions each, the second lifting first.
const TOUCH_GAP_PX: f64 = 40.0;
const ACTION_UP: u32 = 1;
const ACTION_MOVE: u32 = 2;
const ACTION_HOVER_MOVE: u32 = 7;
const ACTION_SCROLL: u32 = 8;
const ACTION_HOVER_ENTER: u32 = 9;
const ACTION_HOVER_EXIT: u32 = 10;
const CLASS_SWIPE: u32 = 3;
/// The keys the probe types (linux input codes, input-event-codes.h): Shift+A, a, then A held past the desktop's repeat delay.
const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
/// How long A is held: past labwc's default repeat delay (600 ms, wlroots keyboard default) plus a few repeats at its 25 Hz.
const KEY_HOLD: Duration = Duration::from_millis(1000);
/// linux/input-event-codes.h BTN_SIDE and BTN_EXTRA: a mouse's back and forward side buttons (GDK 8 and 9, X11 numbering).
pub(super) const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// keycodes.h AKEYCODE_BACK and AKEYCODE_FORWARD: the KeyEvents the side buttons send.
const KEYCODE_BACK: i64 = 4;
const KEYCODE_FORWARD: i64 = 125;
/// Main's OnBackInvokedCallback line: BACK reached the app's back dispatch.
const BACK_CALLBACK_LINE: &str = "AtlWindow: back callback";
/// KeyEvent.FLAG_CANCELED: ViewRootImpl.doOnBackKeyEvent cancels a back UP after its dispatch (ViewRootImpl.java:7750-7753), so the UP reaches Main canceled. Forward has no such dispatch: flags 0.
const FLAG_CANCELED: i64 = 0x20;

/// A widget's rectangle in the content, from its `AtlWindow: layout <name>` line.
#[derive(Debug, Clone, Copy)]
pub(super) struct Rect {
    pub(super) left: i64,
    pub(super) top: i64,
    pub(super) width: i64,
    pub(super) height: i64,
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

pub(super) fn widget(app: &str, name: &str) -> Result<Rect, String> {
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

/// The list's scroll position on a `list first=N top=T` line: rows above plus the first row's offset, ordered like the scroll offset (first row index major, then how far it is scrolled out).
fn list_position(line: &str) -> Option<i64> {
    let (_, rest) = line.split_once("AtlWindow: list first=")?;
    let first: i64 = rest.split_whitespace().next()?.parse().ok()?;
    let top: i64 = word(rest, "top")?.parse().ok()?;
    Some(first * LIST_ROW_SCALE - top)
}

/// The list position last logged before line `i`, 0 (the top) when none was.
fn position_before(lines: &[&str], i: usize) -> i64 {
    lines[..i]
        .iter()
        .rev()
        .find_map(|l| list_position(l))
        .unwrap_or(0)
}

/// A fixture `touch` line's action and classification.
fn touch(line: &str) -> Option<(u32, u32)> {
    let (_, rest) = line.split_once("AtlWindow: touch ")?;
    Some((
        word(rest, "action")?.parse().ok()?,
        word(rest, "class")?.parse().ok()?,
    ))
}

/// A fixture `generic` line's action.
fn generic(line: &str) -> Option<u32> {
    let (_, rest) = line.split_once("AtlWindow: generic ")?;
    word(rest, "action")?.parse().ok()
}

/// The hover Button -> EditText -> Button: the views' resolved pointer icons reached the window as cursors (composer line) and the platform layer set them (CSS names). EditText (editable TextView): TEXT 1008; Button: none resolved -> NOT_SPECIFIED 1 (ViewRootImpl.updatePointerIcon), the arrow
/// (Button.onResolvePointerIcon gives HAND only without flag enableArrowIconOnHoverWhenClickable, on in this build). Last lines count: the earlier mouse taps resolve icons too.
fn cursor_check(app: &str) -> Result<String, String> {
    let at = |needle: &str| {
        app.rfind(needle)
            .ok_or_else(|| format!("no `{needle}` line"))
    };
    let text = at("atl-composer: ui: cursor type=1008 shape=Text")?;
    let arrow = at("atl-composer: ui: cursor type=1 shape=Default")?;
    let (set_text, set_arrow) = (
        at("atl-platform-ui: cursor text")?,
        at("atl-platform-ui: cursor default")?,
    );
    if !(text < arrow && text < set_text && arrow < set_arrow) {
        return Err("cursor lines out of order: want text then arrow, each then set".to_owned());
    }
    Ok("EditText -> text, back on the Button -> arrow, both set on the window".to_owned())
}

/// One `key action=` line of the fixture (Main.dispatchKeyEvent) as (action, code, scan, meta, repeat, flags, device, source, char, kbtype).
fn key_line(line: &str) -> Option<[i64; 10]> {
    let (_, rest) = line.split_once("AtlWindow: key ")?;
    let mut out = [0; 10];
    for (slot, name) in out.iter_mut().zip([
        "action", "code", "scan", "meta", "repeat", "flags", "device", "source", "char", "kbtype",
    ]) {
        *slot = word(rest, name)?.parse().ok()?;
    }
    Some(out)
}

/// The keys the probe typed reached Main as AOSP would make them, and the device list has the physical keyboard and the touchpad.
/// Expected values, all from AOSP sources: Generic.kl `key 30 A`, `key 42 SHIFT_LEFT`; KeyEvent KEYCODE_A 29, KEYCODE_SHIFT_LEFT 59, META_SHIFT_ON | META_SHIFT_LEFT_ON = 0x41, FLAG_LONG_PRESS 0x80;
/// Generic.kcm `key A { base: 'a' shift, capslock: 'A' }` -> char 97 / 65, `type FULL` -> kbtype 4 (KeyCharacterMap.FULL); InputDevice SOURCE_KEYBOARD 0x101, SOURCE_MOUSE | SOURCE_TOUCHPAD = 0x2002 | 0x100008, KEYBOARD_TYPE_ALPHABETIC 2.
/// Device ids are atl-services input_devices' (keyboard 2, touchpad 4).
fn keys_check(app: &str) -> Result<String, String> {
    let (keyboard, touchpad) = (
        i64::from(input_devices::KEYBOARD),
        i64::from(input_devices::TOUCHPAD),
    );
    for want in [
        format!("AtlWindow: device id={keyboard} sources=257 kbtype=2"),
        format!(
            "AtlWindow: device id={touchpad} sources={}",
            0x2002 | 0x10_0008
        ),
    ] {
        if !app.contains(&want) {
            return Err(format!("no `{want}` line"));
        }
    }
    // the mouse's button keys (source mouse) are buttons_check's, not the probe's typed keys
    let keys: Vec<[i64; 10]> = app
        .lines()
        .filter_map(key_line)
        .filter(|k| k[7] != SOURCE_MOUSE)
        .collect();
    if let Some(k) = keys.iter().find(|k| k[6] != keyboard || k[7] != 0x101) {
        return Err(format!(
            "key from device {} source {:#x}, want {keyboard} 0x101",
            k[6], k[7]
        ));
    }
    // (action, code, scan, meta, char) of the downs, repeats aside
    let downs: Vec<(i64, i64, i64, i64)> = keys
        .iter()
        .filter(|k| k[0] == 0 && k[4] == 0)
        .map(|k| (k[1], k[2], k[3], k[8]))
        .collect();
    let want_downs = [
        (59, 42, 0x41, 0),
        (29, 30, 0x41, 65),
        (29, 30, 0, 97),
        (29, 30, 0, 97),
    ];
    if downs != want_downs {
        return Err(format!(
            "first downs (code, scan, meta, char) {downs:?}, want {want_downs:?}"
        ));
    }
    if let Some(k) = keys.iter().find(|k| k[1] == 29 && k[9] != 4) {
        return Err(format!(
            "A read through a map of type {}, want FULL 4 (Generic.kcm)",
            k[9]
        ));
    }
    let repeats: Vec<(i64, i64)> = keys
        .iter()
        .filter(|k| k[4] > 0)
        .map(|k| (k[4], k[5] & 0x80))
        .collect();
    match repeats.first() {
        Some(&(1, 0x80)) => {}
        _ => {
            return Err(format!(
                "repeats (count, long-press flag) {repeats:?}, want the first (1, 0x80)"
            ));
        }
    }
    let ups = keys.iter().filter(|k| k[0] == 1).count();
    if ups != want_downs.len() {
        return Err(format!("{ups} key ups, want {}", want_downs.len()));
    }
    Ok(format!(
        "Shift+A -> 'A' meta 0x41, a -> 'a', held A repeated {} times (first long-press); keyboard {keyboard} and touchpad {touchpad} listed",
        repeats.len()
    ))
}

/// The mouse's side buttons (drive's last step): BTN_SIDE reached Main as KEYCODE_BACK down then up from the mouse (source mouse, the pointer's device, no repeat; the up FLAG_CANCELED by the back dispatch), BTN_EXTRA as KEYCODE_FORWARD the same way (flags 0), and the back callback ran once.
/// Values: keycodes.h, atl-services input_devices. AOSP: CursorInputMapper's synthesizeButtonKeys, a key on each button-state change.
fn buttons_check(app: &str) -> Result<String, String> {
    let pointer = i64::from(input_devices::POINTER);
    let got: Vec<(i64, i64, i64, i64, i64)> = app
        .lines()
        .filter_map(key_line)
        .filter(|k| k[7] == SOURCE_MOUSE)
        .map(|k| (k[0], k[1], k[6], k[4], k[5]))
        .collect();
    let want = [
        (0, KEYCODE_BACK, pointer, 0, 0),
        (1, KEYCODE_BACK, pointer, 0, FLAG_CANCELED),
        (0, KEYCODE_FORWARD, pointer, 0, 0),
        (1, KEYCODE_FORWARD, pointer, 0, 0),
    ];
    if got != want {
        return Err(format!(
            "mouse keys (action, code, device, repeat, flags) {got:?}, want {want:?}"
        ));
    }
    let callbacks = app
        .lines()
        .filter(|l| l.contains(BACK_CALLBACK_LINE))
        .count();
    if callbacks != 1 {
        return Err(format!("{callbacks} back callbacks, want 1"));
    }
    Ok(format!(
        "BTN_SIDE -> KEYCODE_BACK down and up from the mouse ({pointer}), the back callback ran once; BTN_EXTRA -> KEYCODE_FORWARD the same way"
    ))
}

/// The hover the probe makes over the list: HOVER_ENTER (9), HOVER_MOVE (7), HOVER_EXIT (10) in that order with no touch in between (input.h codes). A tap's enter is exited by its press, with no move: only the hover makes a 7.
fn hover_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if generic(l) != Some(ACTION_HOVER_ENTER) {
            continue;
        }
        let mut moved = false;
        for l in &lines[i + 1..] {
            if touch(l).is_some() {
                break;
            }
            match generic(l) {
                Some(ACTION_HOVER_MOVE) => moved = true,
                Some(ACTION_HOVER_EXIT) if moved => {
                    return Ok(
                        "hover enter, move, exit reached the window, no touch between".to_owned(),
                    );
                }
                _ => {}
            }
        }
    }
    Err("no hover enter -> move -> exit sequence (generic action=9, 7, 10)".to_owned())
}

/// The wheel turned down over the list: ACTION_SCROLL (8) with a negative AXIS_VSCROLL (Android's positive is up), and the list then scrolled toward its end.
fn wheel_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let i = lines
        .iter()
        .position(|l| generic(l) == Some(ACTION_SCROLL))
        .ok_or("no ACTION_SCROLL reached the window (generic action=8)")?;
    let vscroll: f32 = number(word(lines[i], "vscroll"), "vscroll")?;
    if vscroll >= 0.0 {
        return Err(format!(
            "a wheel turned down gave AXIS_VSCROLL {vscroll}, want < 0"
        ));
    }
    let before = position_before(&lines, i);
    let end = lines
        .iter()
        .position(|l| touch(l).is_some_and(|(_, c)| c == CLASS_SWIPE))
        .unwrap_or(lines.len());
    let after = lines[i..end]
        .iter()
        .filter_map(|l| list_position(l))
        .max()
        .unwrap_or(before);
    if after <= before {
        return Err(format!(
            "the wheel did not scroll the list: position {before} -> {after}"
        ));
    }
    Ok(format!(
        "AXIS_VSCROLL {vscroll}; list position {before} -> {after}"
    ))
}

/// The touchpad swipe toward the top: its fake finger (class 3) goes DOWN, MOVEs, goes UP; the list moved toward its top while the fingers were down and flung after (`list state=2` after the UP).
fn swipe_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let swipe = |action: u32, from: usize| {
        lines[from..]
            .iter()
            .position(|l| touch(l) == Some((action, CLASS_SWIPE)))
            .map(|p| p + from)
    };
    let down = swipe(ACTION_DOWN, 0).ok_or("no swipe DOWN (touch action=0 class=3)")?;
    let up = swipe(ACTION_UP, down).ok_or("no swipe UP after its DOWN")?;
    if swipe(ACTION_MOVE, down).is_none_or(|m| m > up) {
        return Err("no swipe MOVE between its DOWN and UP".to_owned());
    }
    let (at_down, at_up) = (position_before(&lines, down), position_before(&lines, up));
    if at_up >= at_down {
        return Err(format!(
            "the swipe toward the top did not scroll up: position {at_down} -> {at_up}"
        ));
    }
    if !lines[up..]
        .iter()
        .any(|l| l.contains("AtlWindow: list state=2"))
    {
        return Err(format!(
            "no fling after the swipe (list state=2); position {at_down} -> {at_up}"
        ));
    }
    Ok(format!(
        "fake finger DOWN, MOVE, UP; list position {at_down} -> {at_up}, then a fling"
    ))
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

/// Main's window focus (onWindowFocusChanged, fed by the compositor's per-window focus publish): gained when the GTK window became active, lost to the second activity's window, regained when it closed.
fn window_focus_check(app: &str) -> Result<String, String> {
    let seen: Vec<&str> = app
        .lines()
        .filter_map(|l| {
            l.split_once("AtlWindow: window focus=")
                .map(|(_, v)| v.trim())
        })
        .collect();
    if seen != ["true", "false", "true"] {
        return Err(format!(
            "Main window focus {seen:?}, want [true, false, true]"
        ));
    }
    Ok("Main focused, lost focus to the second window, focused again".to_owned())
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

/// A fixture `touch` line's field `key` (source, pointers, device).
fn touch_field(line: &str, key: &str) -> Option<i64> {
    let (_, rest) = line.split_once("AtlWindow: touch ")?;
    word(rest, key)?.parse().ok()
}

/// The mouse drag (the last plain mouse DOWN: class 0, source mouse) scrolled the list toward its end.
fn list_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let down = lines
        .iter()
        .rposition(|l| {
            touch(l) == Some((ACTION_DOWN, 0)) && touch_field(l, "source") == Some(SOURCE_MOUSE)
        })
        .ok_or("no mouse DOWN at all")?;
    let before = position_before(&lines, down);
    let after = lines[down..]
        .iter()
        .filter_map(|l| list_position(l))
        .max()
        .unwrap_or(before);
    if after <= before {
        return Err(format!(
            "the drag did not scroll the list: position {before} -> {after}"
        ));
    }
    Ok(format!(
        "drag scrolled the list: position {before} -> {after}"
    ))
}

/// The two-finger touch on the list: from the touchscreen (source, device), as DOWN(0) 1 pointer, POINTER_DOWN(5) 2, MOVE(2) 2, POINTER_UP(6) 2, UP(1) 1 (input.h codes; TouchInputMapper's sequence), and the list scrolled after its DOWN.
fn touch_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let from_screen = |l: &str| touch_field(l, "source") == Some(SOURCE_TOUCHSCREEN);
    let first = lines
        .iter()
        .position(|l| from_screen(l))
        .ok_or("no touchscreen event at all")?;
    let mut seq: Vec<(i64, i64)> = Vec::new();
    for l in lines[first..].iter().filter(|l| from_screen(l)) {
        if touch_field(l, "device") != Some(TOUCHSCREEN_DEVICE) {
            return Err(format!("a touchscreen event from another device: {l}"));
        }
        let (action, _) = touch(l).ok_or("unparsed touch line")?;
        let step = (i64::from(action), touch_field(l, "pointers").unwrap_or(0));
        // repeated MOVEs are one step
        if seq.last() != Some(&step) {
            seq.push(step);
        }
    }
    let want = [(0, 1), (5, 2), (2, 2), (6, 2), (1, 1)];
    if seq != want {
        return Err(format!(
            "touchscreen (action, pointers) {seq:?}, want {want:?}"
        ));
    }
    let before = position_before(&lines, first);
    let after = lines[first..]
        .iter()
        .filter_map(|l| list_position(l))
        .max()
        .unwrap_or(before);
    if after <= before {
        return Err(format!(
            "the touch did not scroll the list: position {before} -> {after}"
        ));
    }
    Ok(format!(
        "two fingers from the touchscreen {seq:?}; list position {before} -> {after}"
    ))
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

/// The widgets stage's sub-gates, in print order, from the probe's result, the app log and the frame callbacks after the marker.
pub fn checks(
    app: &str,
    after: &[&str],
    probe: &Result<Liveness, String>,
    display: DisplayModel,
    pressure: &Samples,
) -> Vec<Sub> {
    vec![
        (
            "probe",
            probe
                .as_ref()
                .map(|_| "every injection got its answer".to_owned())
                .map_err(Clone::clone),
        ),
        ("layout", layout_check(app)),
        ("text", text_check(app)),
        ("header", header_check(app)),
        ("click", click_check(app)),
        ("focus", focus_check(app)),
        ("window", window_check(app)),
        ("window-focus", window_focus_check(app)),
        ("cursor", cursor_check(app)),
        ("keys", keys_check(app)),
        ("mouse-buttons", buttons_check(app)),
        ("hover", hover_check(app)),
        ("wheel", wheel_check(app)),
        ("swipe", swipe_check(app)),
        ("list", list_check(app)),
        ("touch", touch_check(app)),
        ("readback", readback_check(app)),
        ("presents", presents_check(app)),
        (
            "cadence",
            cadence_check(after, display.refresh_hz, pressure),
        ),
        ("on-demand", on_demand_check(app)),
    ]
}

pub(super) fn press(pressed: bool) -> PointerAction {
    PointerAction::Button {
        button: BTN_LEFT,
        pressed,
    }
}

pub(super) fn inject(proxy: &Proxy, actions: Vec<PointerAction>) -> Result<(), String> {
    for action in actions {
        proxy.inject_pointer(action).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Whether the app log satisfies `want` within `within` (polled: the app process writes the log).
pub(super) fn wait_until(app_log: &Path, within: Duration, want: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if std::fs::read_to_string(app_log).is_ok_and(|text| want(&text)) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

pub(super) fn wait_line(app_log: &Path, needle: &str) -> Result<(), String> {
    wait_for_line(app_log, needle, LINE_WAIT)
        .then_some(())
        .ok_or_else(|| format!("no `{needle}` line within {LINE_WAIT:?}"))
}

/// The widgets probe: after the software frame and the first readback (so both show the picture before the input), presses the header, taps the Button, taps the EditText, waits for the second window to close and taps the Button again, hovers over the ListView, turns the wheel down and swipes up over it, drags it up, waits for the readback of the final frame, and presses the mouse's back and forward side buttons over the label.
/// Each injection waits for its answer: the proxy's report for the header, the fixture's line for the rest. The points are the fixture's rectangles' centres, shifted by the content origin. No liveness probe runs here.
pub fn drive(proxy: Proxy, app_log: &Path, break_mouse_back: bool) -> Result<Liveness, String> {
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
    // the cursor follows the views under the pointer: hover the Button, the EditText, the Button again
    let ((bx, by), (ix, iy)) = (centre("button")?, centre("input")?);
    inject(
        &proxy,
        vec![
            PointerAction::Enter { x: bx, y: by },
            PointerAction::Motion {
                x: bx + HOVER_PX,
                y: by,
            },
            PointerAction::Motion { x: ix, y: iy },
            PointerAction::Motion {
                x: ix + HOVER_PX,
                y: iy,
            },
            PointerAction::Motion { x: bx, y: by },
            PointerAction::Leave,
        ],
    )?;
    wait_until(app_log, LINE_WAIT, |t| cursor_check(t).is_ok());
    // keys: Shift+A, a, then a held A (the client's own repeat runs while it is down)
    let key = |code: u32, pressed: bool| proxy.inject_key(code, pressed).map_err(|e| e.to_string());
    for (code, pressed) in [
        (KEY_LEFTSHIFT, true),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, false),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_A, true),
    ] {
        key(code, pressed)?;
    }
    std::thread::sleep(KEY_HOLD);
    key(KEY_A, false)?;
    wait_until(app_log, LINE_WAIT, |t| keys_check(t).is_ok());
    let (x, y) = centre("list")?;
    // hover, wheel and swipe over the list: each waits for its own check, and a miss leaves that check to fail alone (the drag and the cadence run after it still run)
    inject(
        &proxy,
        vec![
            PointerAction::Enter { x, y },
            PointerAction::Motion {
                x: x + HOVER_PX,
                y: y + HOVER_PX,
            },
            PointerAction::Leave,
        ],
    )?;
    wait_until(app_log, LINE_WAIT, |t| hover_check(t).is_ok());
    inject(
        &proxy,
        vec![
            PointerAction::Enter { x, y },
            PointerAction::Wheel {
                notches: WHEEL_NOTCHES,
            },
        ],
    )?;
    wait_until(app_log, LINE_WAIT, |t| wheel_check(t).is_ok());
    for _ in 0..SWIPE_STEPS {
        inject(
            &proxy,
            vec![PointerAction::Finger {
                dx: 0.0,
                dy: -SWIPE_STEP_PX,
            }],
        )?;
        std::thread::sleep(Duration::from_millis(SWIPE_STEP_MS));
    }
    inject(&proxy, vec![PointerAction::FingerStop])?;
    // the fling ends idle (state 0) before the drag
    wait_until(app_log, FLING_WAIT, |t| {
        swipe_check(t).is_ok()
            && t.rfind("AtlWindow: list state=0") > t.rfind("AtlWindow: list state=2")
    });
    let drag_from = std::fs::read_to_string(app_log)
        .map_err(|e| e.to_string())?
        .len();
    let mut drag = vec![PointerAction::Enter { x, y }, press(true)];
    for step in 1..=SCROLL_STEPS {
        drag.push(PointerAction::Motion {
            x,
            y: y - DRAG_PX * f64::from(step) / f64::from(SCROLL_STEPS),
        });
    }
    drag.push(press(false));
    inject(&proxy, drag)?;
    if !wait_until(app_log, LINE_WAIT, |t| {
        t.get(drag_from..)
            .is_some_and(|after| after.contains("AtlWindow: list first="))
    }) {
        return Err("the list did not scroll after the drag".to_owned());
    }
    // two fingers on the touchscreen, once the drag's fling is idle; a miss leaves the touch check to fail alone
    wait_until(app_log, FLING_WAIT, |t| {
        t.rfind("AtlWindow: list state=0") > t.rfind("AtlWindow: list state=2")
    });
    let (x2, step) = (x + TOUCH_GAP_PX, DRAG_PX / f64::from(SCROLL_STEPS));
    let mut fingers = vec![
        TouchAction::Down { id: 0, x, y },
        TouchAction::Down { id: 1, x: x2, y },
    ];
    for n in 1..=SCROLL_STEPS {
        let y = y - step * f64::from(n);
        fingers.push(TouchAction::Motion { id: 0, x, y });
        fingers.push(TouchAction::Motion { id: 1, x: x2, y });
    }
    fingers.extend([TouchAction::Up { id: 1 }, TouchAction::Up { id: 0 }]);
    for action in fingers {
        proxy.inject_touch(action).map_err(|e| e.to_string())?;
    }
    wait_until(app_log, LINE_WAIT, |t| touch_check(t).is_ok());
    wait_line(app_log, FINAL_LINE)?;
    // the readback of the final frame comes after the fixture's line, once the scenes stop
    if !wait_until(app_log, LINE_WAIT, |t| {
        t.rfind(CAPTURE_LINE) > t.find(FINAL_LINE)
    }) {
        return Err("no readback after the final frame".to_owned());
    }
    // the mouse's side buttons last, over the label (not clickable): BTN_SIDE is back, BTN_EXTRA forward; the break sends BTN_LEFT for the back press, so no back key comes
    let (x, y) = centre("label")?;
    let back = if break_mouse_back { BTN_LEFT } else { BTN_SIDE };
    let button = |button: u32, pressed: bool| PointerAction::Button { button, pressed };
    inject(
        &proxy,
        vec![
            PointerAction::Enter { x, y },
            button(back, true),
            button(back, false),
            button(BTN_EXTRA, true),
            button(BTN_EXTRA, false),
        ],
    )?;
    wait_until(app_log, LINE_WAIT, |t| buttons_check(t).is_ok());
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

    /// One mouse key line as Main logs it (`key` format, source mouse).
    fn mouse_key(action: u32, code: i64, device: i64, flags: i64) -> String {
        format!(
            "AtlWindow: key action={action} code={code} scan=0 meta=0 repeat=0 flags={flags} device={device} source={SOURCE_MOUSE} char=0 kbtype=0\n"
        )
    }

    /// Back and forward down and up from the pointer, and the back callback: the probe's last step passes; a missing back key, an uncanceled back up or a missing callback fails it.
    #[test]
    fn buttons_check_wants_back_and_forward_from_the_mouse() {
        let pointer = i64::from(input_devices::POINTER);
        let back_up = mouse_key(1, KEYCODE_BACK, pointer, FLAG_CANCELED);
        let good = [
            mouse_key(0, KEYCODE_BACK, pointer, 0),
            back_up.clone(),
            mouse_key(0, KEYCODE_FORWARD, pointer, 0),
            mouse_key(1, KEYCODE_FORWARD, pointer, 0),
        ]
        .concat()
            + "AtlWindow: back callback\n";
        assert!(buttons_check(&good).is_ok());
        assert!(buttons_check(&good.replace(&mouse_key(0, KEYCODE_BACK, pointer, 0), "")).is_err());
        assert!(
            buttons_check(&good.replace(&back_up, &mouse_key(1, KEYCODE_BACK, pointer, 0)))
                .is_err()
        );
        assert!(buttons_check(&good.replace("AtlWindow: back callback\n", "")).is_err());
        // logcat prefixes every line of a real run
        let logcat = good.replace(
            "AtlWindow: back callback\n",
            "1793995016.872 5227.437     2     2 I liblog/AtlWindow: back callback\n",
        );
        assert!(buttons_check(&logcat).is_ok());
    }

    #[test]
    fn header_check_wants_the_buttons_tap_first() {
        let tap = format!("{BUTTON}AtlWindow: touch action=0 x=60.0 y=73.0\n");
        assert!(header_check(&tap).is_ok());
        // a header press that leaked: content y is above the content's origin
        let leak = format!("{BUTTON}AtlWindow: touch action=0 x=86.0 y=-31.0\n{tap}");
        assert!(header_check(&leak).is_err());
        assert!(header_check(BUTTON).is_err());
    }

    /// Hover needs 9, 7, 10 with no touch between: a tap's 9, 10 alone fails, a touch between fails.
    #[test]
    fn hover_check_wants_enter_move_exit() {
        let g = |a: u32| {
            format!(
                "AtlWindow: generic action={a} vscroll=0.0 x=1.0 y=2.0
"
            )
        };
        let tap = "AtlWindow: touch action=0 x=1.0 y=2.0 class=0
";
        assert!(hover_check(&[g(9), g(7), g(10)].concat()).is_ok());
        assert!(hover_check(&[g(9), g(10)].concat()).is_err());
        assert!(hover_check(&[g(9), g(7), tap.to_owned(), g(10)].concat()).is_err());
    }

    /// Wheel down: negative VSCROLL and the list position grows after it; swipe: class-3 DOWN, MOVE, UP, the position falls, then a fling.
    #[test]
    fn wheel_and_swipe_checks_follow_the_list() {
        let pos = |first: u32, top: i32| {
            format!(
                "AtlWindow: list first={first} top={top}
"
            )
        };
        let scroll = |v: f32| {
            format!(
                "AtlWindow: generic action=8 vscroll={v} x=1.0 y=2.0
"
            )
        };
        let t = |a: u32| {
            format!(
                "AtlWindow: touch action={a} x=1.0 y=2.0 class=3
"
            )
        };
        assert!(wheel_check(&[pos(0, 0), scroll(-5.0), pos(6, -10)].concat()).is_ok());
        assert!(wheel_check(&[pos(0, 0), scroll(5.0), pos(6, -10)].concat()).is_err());
        assert!(wheel_check(&[pos(0, 0), scroll(-5.0)].concat()).is_err());
        let fling = "AtlWindow: list state=2
"
        .to_owned();
        let swipe = [pos(6, -10), t(0), t(2), pos(5, -3), t(1), fling.clone()].concat();
        assert!(swipe_check(&swipe).is_ok());
        assert!(swipe_check(&[pos(6, -10), t(0), t(2), pos(5, -3), t(1)].concat()).is_err());
        assert!(swipe_check(&[pos(6, -10), t(0), t(2), pos(7, -3), t(1), fling].concat()).is_err());
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

/// The composer's line at each stop of the on-demand window clock (ui_window.rs; gates only).
const CLOCK_STOPPED_LINE: &str = "atl-composer: ui: clock stopped ";

/// Sub-gate on-demand: the window clock ticks only while asked (EventThread sends an app vsync only after requestNextVsync). It stopped at least once, no tick ran with nothing wanting it (`unwanted=0`),
/// and it stopped after the last readback: the idle fixture leaves the clock stopped for the rest of the watch, where a free-running clock would tick at the display rate.
fn on_demand_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let stops: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].contains(CLOCK_STOPPED_LINE))
        .collect();
    let &last = stops.last().ok_or("the window clock never stopped")?;
    let unwanted = word(lines[last], "unwanted").ok_or("no unwanted= in the stop line")?;
    if unwanted != "0" {
        return Err(format!("{unwanted} ticks ran with nothing wanting them"));
    }
    let last_capture = lines.iter().rposition(|l| l.contains(CAPTURE_LINE));
    if last_capture.is_some_and(|c| c > last) {
        return Err("no stop after the last readback: the clock still ran at the end".to_owned());
    }
    let ticks = word(lines[last], "ticks").unwrap_or("?");
    Ok(format!(
        "{} stops, the last after the final readback at tick {ticks}; 0 unwanted ticks",
        stops.len()
    ))
}

#[cfg(test)]
mod on_demand_tests {
    use super::on_demand_check;

    /// A stop after the last readback with 0 unwanted passes; no stop, an unwanted tick, or a readback after the last stop (the clock still running) fails.
    #[test]
    fn on_demand_needs_a_final_stop_with_no_unwanted_tick() {
        let stop = |u| format!("atl-composer: ui: clock stopped ticks=90 unwanted={u}");
        let capture = "atl-composer: ui: capture presented=3 size=3x1 fnv1a64=0";
        assert!(on_demand_check(&[capture, &stop(0)].join("\n")).is_ok());
        assert!(on_demand_check(capture).is_err());
        assert!(on_demand_check(&[capture, &stop(2)].join("\n")).is_err());
        assert!(on_demand_check(&[&stop(0), capture].join("\n")).is_err());
    }
}
