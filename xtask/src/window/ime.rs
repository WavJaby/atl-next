// SPDX-License-Identifier: MIT
//! Stage `ime` (IME plan step 5): the widgets fixture in UI mode on the gate's labwc, a fake desktop input method (nested_wayland `Ime`) on its seat. The probe taps the EditText; once the focus's second
//! window has closed, Main's EditText holds the focus and the in-app input method (atl-ime) has started input for it, so the desktop input method is activated. The fake IME then sends a
//! preedit, a commit and a delete-surrounding; the fixture logs each edit (`text input=<text> composing=<start>,<end>`, its TextWatcher).
//! Sub-gates: activate (the IME was activated), preedit (the text holds the preedit as a composing span), commit (the commit replaced it, no composing span left), delete (the last char of the commit is gone).
//! `--break-ime-commit` leaves the commit_string out: the commit sub-gate must fail.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::nested_wayland::{Arg, Ime, im_op};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

use super::{
    ACK_WAIT, Liveness, Sub, await_event, content_origin, wait_for_line,
    widgets::{self},
};

/// The fixture's EditText text before the IME (Main.java setText).
const INITIAL: &str = "typed from code";
const PREEDIT: &str = "かな";
const COMMIT: &str = "仮名";
/// The fixture's edit line.
const TEXT_LINE: &str = "AtlWindow: text input=";
/// How long the desktop input method may take to activate after the focus, and each edit to reach the fixture.
const IME_WAIT: Duration = Duration::from_secs(10);

/// The fixture's text after the preedit: the cursor sits at the end of the setText text (TextView.setText puts the selection at the end), so the preedit is appended there.
fn after_preedit() -> String {
    format!("{INITIAL}{PREEDIT}")
}

fn after_commit() -> String {
    format!("{INITIAL}{COMMIT}")
}

/// After deleting one char before the cursor: the commit's last char goes.
fn after_delete() -> String {
    let mut s = after_commit();
    s.pop();
    s
}

fn edits(app: &str) -> Vec<&str> {
    app.lines()
        .filter_map(|l| l.split_once(TEXT_LINE).map(|(_, rest)| rest))
        .collect()
}

/// The edit line whose text is `text`: its composing span.
fn composing_of<'a>(app: &'a str, text: &str) -> Option<&'a str> {
    edits(app)
        .into_iter()
        .filter_map(|e| e.rsplit_once(" composing="))
        .find(|(t, _)| *t == text)
        .map(|(_, c)| c)
}

fn wait_text(app_log: &Path, ime: &mut Ime, text: &str) -> bool {
    let deadline = Instant::now() + IME_WAIT;
    while Instant::now() < deadline {
        let app = std::fs::read_to_string(app_log).unwrap_or_default();
        if composing_of(&app, text).is_some() {
            return true;
        }
        let _ = ime.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The probe: tap the EditText, wait for the second window to close and the IME to activate, then preedit, commit, delete.
pub fn drive(
    proxy: Proxy,
    app_log: &Path,
    mut ime: Ime,
    break_commit: bool,
) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, widgets::CAPTURE_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the window never read back")?;
    let text = std::fs::read_to_string(app_log).map_err(|e| e.to_string())?;
    let (ox, oy) = content_origin(&text)?;
    let r = widgets::widget(&text, "input")?;
    let (x, y) = (
        (r.left + r.width / 2) as f64 + f64::from(ox),
        (r.top + r.height / 2) as f64 + f64::from(oy),
    );
    widgets::inject(
        &proxy,
        vec![
            PointerAction::Enter { x, y },
            widgets::press(true),
            widgets::press(false),
        ],
    )?;
    widgets::wait_line(app_log, "AtlWindow: focus input=true")?;
    widgets::wait_line(app_log, widgets::SECOND_CLOSED)?;
    let deadline = Instant::now() + IME_WAIT;
    while !(ime.active && ime.dones > 0) && Instant::now() < deadline {
        ime.pump().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(20));
    }
    if !ime.active {
        return Err("the desktop input method was never activated (no startInput reached the window's TextInput?)".to_owned());
    }
    let byte_len = |s: &str| i32::try_from(s.len()).unwrap_or(i32::MAX);
    ime.settle().map_err(|e| e.to_string())?;
    ime.send(
        im_op::SET_PREEDIT_STRING,
        &[
            Arg::S(PREEDIT),
            Arg::I(byte_len(PREEDIT)),
            Arg::I(byte_len(PREEDIT)),
        ],
    );
    ime.commit().map_err(|e| e.to_string())?;
    wait_text(app_log, &mut ime, &after_preedit());
    ime.settle().map_err(|e| e.to_string())?;
    if !break_commit {
        ime.send(im_op::COMMIT_STRING, &[Arg::S(COMMIT)]);
    }
    ime.send(
        im_op::SET_PREEDIT_STRING,
        &[Arg::S(""), Arg::I(0), Arg::I(0)],
    );
    ime.commit().map_err(|e| e.to_string())?;
    wait_text(app_log, &mut ime, &after_commit());
    ime.settle().map_err(|e| e.to_string())?;
    // one char before the cursor, in bytes (text-input v3 counts bytes): the commit's last char
    let last = COMMIT.chars().last().map_or(0, char::len_utf8);
    ime.send(
        im_op::DELETE_SURROUNDING_TEXT,
        &[Arg::U(u32::try_from(last).unwrap_or(0)), Arg::U(0)],
    );
    ime.commit().map_err(|e| e.to_string())?;
    wait_text(app_log, &mut ime, &after_delete());
    Ok(Liveness {
        answered: true,
        silent: true,
        resumed_ms: None,
        configured: Vec::new(),
    })
}

/// The stage's sub-gates over the fixture log; `activate` is the probe's (it fails when the desktop input method never activated).
pub fn checks(app: &str, probe: &Result<Liveness, String>) -> Vec<Sub> {
    let activate = probe
        .as_ref()
        .map(|_| "the desktop input method was activated for the focused EditText".to_owned())
        .map_err(Clone::clone);
    let edit =
        |text: String, want: &dyn Fn(&str) -> bool, what: &str| match composing_of(app, &text) {
            Some(c) if want(c) => Ok(format!("text `{text}` composing={c}")),
            Some(c) => Err(format!("text `{text}` but composing={c} ({what})")),
            None => Err(format!("no edit to `{text}`; edits: {:?}", edits(app))),
        };
    // the preedit is the composing span: from the end of the initial text, in UTF-16 units (Android Spanned indices), to the end
    let start = INITIAL.encode_utf16().count();
    let end = start + PREEDIT.encode_utf16().count();
    let preedit_span = format!("{start},{end}");
    vec![
        ("activate", activate),
        (
            "preedit",
            edit(
                after_preedit(),
                &|c| c == preedit_span,
                "the preedit's span",
            ),
        ),
        (
            "commit",
            edit(
                after_commit(),
                &|c| c == "-1,-1",
                "no composing span after a commit",
            ),
        ),
        (
            "delete",
            edit(after_delete(), &|c| c == "-1,-1", "no composing span"),
        ),
    ]
}
