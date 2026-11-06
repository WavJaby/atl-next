// SPDX-License-Identifier: MIT
//! Stage `ime` (IME plan step 5): the widgets fixture in UI mode on the gate's labwc, a fake desktop input method (nested_wayland `Ime`) on its seat. The probe taps the EditText; once the focus's second
//! window has closed, Main's EditText holds the focus and the in-app input method (atl-ime) has started input for it, so the desktop input method is activated. The fake IME then sends a
//! preedit, a commit and a delete-surrounding; the fixture logs each edit (`text input=<text> composing=<start>,<end>`, its TextWatcher).
//! Sub-gates: activate (the IME was activated), preedit (the text holds the preedit as a composing span), commit (the commit replaced it, no composing span left), surrounding (after the commit the IME
//! holds the editor's text with the cursor at its end, in text-input v3 bytes: the getSurroundingText path, IME plan step 6), delete (the last char of the commit is gone: GTK turns the IME's byte
//! count into chars against that surrounding).
//! `--break-ime-commit` leaves the commit_string out: the commit sub-gate must fail. `--break-ime-delete` sends the delete's byte count as after_length (after the cursor, where nothing is): the delete
//! sub-gate must fail.
//! Then death: the probe SIGKILLs the app mid-session; the services must count one client death and drop the uid's entry (IMMS-lite's death link). `--break-ime-death` runs the services without
//! the link (`--no-ime-death-link`): the death sub-gate must fail.

use std::path::Path;
use std::sync::Mutex;
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

/// The services' client-death counter (counters::count's line: `services: unimplemented <interface>.<name> <n> at=...`) and removal line (input_method.rs State::client_died).
const DEATH_COUNTER: &str = "unimplemented input_method.client died";
const DEATH_LINE: &str = "services: input_method client uid=";
const NONE_LEFT: &str = "entry removed, 0 clients left";

/// The surrounding text (text, cursor byte, anchor byte) the fake IME held after the commit: the probe thread's, for `checks` (Liveness is shared by every stage).
static SURROUNDING: Mutex<Option<(String, u32, u32)>> = Mutex::new(None);

/// The deliberate breaks.
#[derive(Clone, Copy, Default)]
pub struct Breaks {
    pub commit: bool,
    pub delete: bool,
}

/// Pumps until the IME's surrounding text is `text` (it follows the editor asynchronously) or the wait ends; the last one seen is kept for `checks`.
fn wait_surrounding(ime: &mut Ime, text: &str) {
    let deadline = Instant::now() + IME_WAIT;
    while Instant::now() < deadline && ime.surrounding.as_ref().is_none_or(|s| s.0 != text) {
        let _ = ime.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    *SURROUNDING.lock().unwrap_or_else(|e| e.into_inner()) = ime.surrounding.clone();
}

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
    breaks: Breaks,
    atld: u32,
    services_log: &Path,
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
    if !breaks.commit {
        ime.send(im_op::COMMIT_STRING, &[Arg::S(COMMIT)]);
    }
    ime.send(
        im_op::SET_PREEDIT_STRING,
        &[Arg::S(""), Arg::I(0), Arg::I(0)],
    );
    ime.commit().map_err(|e| e.to_string())?;
    wait_text(app_log, &mut ime, &after_commit());
    wait_surrounding(&mut ime, &after_commit());
    ime.settle().map_err(|e| e.to_string())?;
    // one char before the cursor, in bytes (text-input v3 counts bytes): the commit's last char
    let last = u32::try_from(COMMIT.chars().last().map_or(0, char::len_utf8)).unwrap_or(0);
    let (before, after) = if breaks.delete { (0, last) } else { (last, 0) };
    ime.send(
        im_op::DELETE_SURROUNDING_TEXT,
        &[Arg::U(before), Arg::U(after)],
    );
    ime.commit().map_err(|e| e.to_string())?;
    wait_text(app_log, &mut ime, &after_delete());
    // mid-session, input bound: SIGKILL the app (no Java-side teardown, so only the binder death can tell the services), then give the death notification time to arrive
    let apps = super::app_pids(atld);
    if apps.is_empty() {
        return Err("no app process to kill".to_owned());
    }
    for pid in &apps {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    wait_for_line(services_log, DEATH_LINE, IME_WAIT);
    Ok(Liveness {
        answered: true,
        silent: true,
        resumed_ms: None,
        configured: Vec::new(),
    })
}

/// The stage's sub-gates over the fixture log; `activate` is the probe's (it fails when the desktop input method never activated).
pub fn checks(app: &str, services: &str, probe: &Result<Liveness, String>) -> Vec<Sub> {
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
        ("surrounding", surrounding_check()),
        (
            "delete",
            edit(after_delete(), &|c| c == "-1,-1", "no composing span"),
        ),
        ("death", death_check(services)),
    ]
}

/// After the kill: the services counted exactly one client death, and its line names the app's uid with its entry removed and no client left (the fixture is the only app).
fn death_check(services: &str) -> Result<String, String> {
    let counted: Vec<&str> = services
        .lines()
        .filter(|l| l.contains(DEATH_COUNTER))
        .collect();
    let removed: Vec<&str> = services
        .lines()
        .filter(|l| l.contains(DEATH_LINE))
        .collect();
    let want = format!("{DEATH_LINE}{} ", super::APP_UID);
    match (counted.as_slice(), removed.as_slice()) {
        ([c], [r])
            if c.contains(&format!("{DEATH_COUNTER} 1 "))
                && r.contains(&want)
                && r.ends_with(NONE_LEFT) =>
        {
            Ok(format!(
                "one client death counted; uid {} entry removed, 0 clients left",
                super::APP_UID
            ))
        }
        _ => Err(format!(
            "want one `{DEATH_COUNTER} 1` and one `{want}... {NONE_LEFT}`; counter lines {counted:?}, removal lines {removed:?}"
        )),
    }
}

/// After the commit the IME holds the editor's whole text, cursor and anchor at its end: byte offset = the UTF-8 length (INITIAL 15 ASCII bytes + 仮名 2 x 3 bytes = 21).
fn surrounding_check() -> Result<String, String> {
    let want = after_commit();
    let end = u32::try_from(want.len()).unwrap_or(u32::MAX);
    match SURROUNDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        Some((text, cursor, anchor)) if text == want && cursor == end && anchor == end => {
            Ok(format!("`{text}` cursor = anchor = byte {end}"))
        }
        Some((text, cursor, anchor)) => Err(format!(
            "`{text}` cursor {cursor} anchor {anchor}, want `{want}` at byte {end}"
        )),
        None => Err("the IME never received a surrounding text".to_owned()),
    }
}
