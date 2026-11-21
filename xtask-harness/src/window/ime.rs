// SPDX-License-Identifier: MIT
//! Stage `ime` (IME plan step 5): the widgets fixture in UI mode on the gate's labwc, a fake desktop input method (nested_wayland `Ime`) on its seat. The probe taps the EditText; once the focus's second
//! window has closed, Main's EditText holds the focus and the in-app input method (atl-ime) has started input for it, so the desktop input method is activated. The fake IME then sends a
//! preedit, a commit and a delete-surrounding; the fixture logs each edit (`text input=<text> composing=<start>,<end>`, its TextWatcher).
//! Sub-gates: activate (the IME was activated), preedit (the text holds the preedit as a composing span), commit (the commit replaced it, no composing span left), surrounding (after the commit the IME
//! holds the editor's text with the cursor at its end, in text-input v3 bytes: GTK reads it inside the commit's `done`, from the display thread's copy of the editor's text), delete (the last char of
//! the commit is gone: GTK turns the IME's byte count into chars against that surrounding), copy (the editor's getSurroundingText answers confirm that copy, none diverges).
//! `--break-ime-commit` leaves the commit_string out: the commit sub-gate must fail. `--break-ime-delete` sends the delete's byte count as after_length (after the cursor, where nothing is): the delete
//! sub-gate must fail.
//! Then death: the probe SIGKILLs the app mid-session; the services must count one client death and drop the uid's entry (IMMS-lite's death link). `--break-ime-death` runs the services without
//! the link (`--no-ime-death-link`): the death sub-gate must fail. Then relaunch: the probe starts the fixture again through atld's control `start` (startActivity with no caller app,
//! a process started anew); the relaunch sub-gate wants a fresh addClient and bind after the death.
//! The run ends when this probe is done (window.rs `probe_ends_run`), and the composer sub-checks judge the first process's part of the shared app log ([`first_process`]): the relaunched one
//! is stopped wherever its start has got to.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::nested_wayland::{Arg, Ime, im_op};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

use super::{
    ACK_WAIT, CONTENT_ORIGIN_LINE, Liveness, Sub, await_event, content_origin, wait_for_line,
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
/// How long the first process's composer may take, before the kill, to log what the post-draw and release sub-checks judge (a paint that released a retired shown buffer). Paints took up to
/// ~1.3 s with the GPU held by another job (2026-11-20, against 3-9 ms idle); the bound only runs out when that paint never comes (`--break-ignore-hold`), and the check then judges what was logged.
const SETTLE_WAIT: Duration = Duration::from_secs(30);
/// How long the services may take to tell the SIGKILL as the client's binder death. Generous: it bounds only a lost death (`--break-ime-death`); a death told after the relaunch's addClient is
/// dropped by the pid check and would read as a lost one.
const DEATH_WAIT: Duration = Duration::from_secs(30);
/// How long the relaunch may take to bind again: a cold start, bounded as the first one ([`super::LIMIT`]). Under 5-9 builders addClient alone came 8 s after `start` (2026-11-21).
const RELAUNCH_WAIT: Duration = super::LIMIT;

/// The services' client-death counter (atl_diag::answered: `diag: first|count answered <interface>.<name> <n> at=...`) and removal line (input_method.rs State::client_died).
const DEATH_COUNTER: &str = "answered IInputMethodManager.client died";
const DEATH_LINE: &str = "services: input_method client uid=";
const NONE_LEFT: &str = "entry removed, 0 clients left";
/// atl_diag's exit summary line prefix (`diag: total <outcome> <api> <n> first=<stamp>`).
const DIAG_TOTAL: &str = "diag: total ";
/// The relaunched process's client: the second addClient, then the second onBindMethod (atl-diag power-of-two lines).
const READDED_LINE: &str = "answered IInputMethodManager.addClient 2 ";
const REBOUND_LINE: &str = "sent IInputMethodManager.IInputMethodClient.onBindMethod 2 ";

/// The surrounding text (text, cursor byte, anchor byte) the fake IME held after the commit: the probe thread's, for `checks` (Liveness is shared by every stage).
static SURROUNDING: Mutex<Option<(String, u32, u32)>> = Mutex::new(None);
/// The app log's length once the first process was dead (its death told, or the wait over), before the relaunch: its lines end there. The probe thread's, for [`first_process`].
static FIRST_END: Mutex<Option<usize>> = Mutex::new(None);

/// The first process's part of the app log: the relaunched process writes the same log, and the run stops it wherever its start has got to, so the composer sub-checks (post-draw, release,
/// dmabuf) judge the process the stage drove. The whole log when the probe never got to the kill.
pub fn first_process(app: &str) -> &str {
    first_part(app, *FIRST_END.lock().unwrap_or_else(|e| e.into_inner()))
}

/// `app` up to its last whole line inside `end` bytes; all of it for `None`.
fn first_part(app: &str, end: Option<usize>) -> &str {
    match end {
        Some(end) => app.as_bytes()[..end.min(app.len())]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or("", |at| &app[..=at]),
        None => app,
    }
}

/// The composer has logged what the post-draw and release sub-checks judge: a post-draw received, and a shown buffer retired for a paint whose release went out with its read fence (or a fence
/// that rode no release: red, and final). Read on the counters line, which the composer logs at most once a second after the work.
fn composer_settled(app: &str) -> bool {
    let Some(line) = crate::composer::last_counters(app) else {
        return false;
    };
    let n = |k: &str| crate::composer::field(line, k).unwrap_or(0);
    n("post_draw_received") > 0
        && n("releases_held") > 0
        && (n("releases_fenced") > 0 || n("releases_fenced_unmatched") > 0)
}

fn wait_settled(app_log: &Path) {
    let deadline = Instant::now() + SETTLE_WAIT;
    while Instant::now() < deadline
        && !composer_settled(&std::fs::read_to_string(app_log).unwrap_or_default())
    {
        std::thread::sleep(Duration::from_millis(50));
    }
}

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
    relaunch: (PathBuf, String),
) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    // the content origin is logged at the window's first present: the window has shown a frame
    wait_for_line(app_log, CONTENT_ORIGIN_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the window never presented")?;
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
    // the composer sub-checks judge this process's last counters: let its paints catch up first
    wait_settled(app_log);
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
    wait_for_line(services_log, DEATH_LINE, DEATH_WAIT);
    *FIRST_END.lock().unwrap_or_else(|e| e.into_inner()) = std::fs::metadata(app_log)
        .ok()
        .and_then(|m| usize::try_from(m.len()).ok());
    // then the relaunch, as a launcher tap: startActivity with no caller app (atld `start` -> atl-am), whose process is started anew; its window's focus binds again (fresh addClient)
    let (root, component) = relaunch;
    let reply = crate::atld_run::control_at(&root, &format!("start {component}"))
        .map_err(|e| format!("relaunch: {e:#}"))?;
    if !reply.starts_with("ok") {
        return Err(format!("relaunch refused: {reply}"));
    }
    wait_for_line(services_log, REBOUND_LINE, RELAUNCH_WAIT);
    Ok(Liveness {
        answered: true,
        silent: true,
        resumed_ms: None,
        configured: Vec::new(),
        idle: None,
        minimized: false,
    })
}

/// The stage's sub-gates over the fixture log; `activate` is the probe's (it fails when the desktop input method never activated); `app_uid` is the fixture's (Runtime::first_app_uid).
pub fn checks(
    app: &str,
    services: &str,
    probe: &Result<Liveness, String>,
    app_uid: u32,
) -> Vec<Sub> {
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
        ("death", death_check(services, app_uid)),
        ("relaunch", relaunch_check(services)),
        // the relaunch reports too; judge the process the stage drove, as the composer sub-checks do
        ("outbox", outbox_check(first_process(app))),
        ("copy", copy_check(first_process(app))),
    ]
}

/// The input method's lines when an editor answer meets a copy holding its own edits (atl-ime field.rs `surrounding`).
const COPY_CONFIRMED: &str = "atl-ime: editor text confirmed n=";
const COPY_DIVERGED: &str = "atl-ime: editor text diverged n=";

/// The display thread answers GTK from its copy of the editor's text, which applies each edit before the editor does (atl-ime editor_text.rs): the editor's later answers must agree with it.
/// Want >= 1 confirmed (the comparison ran: the preedit's and the commit's answers) and 0 diverged.
fn copy_check(app: &str) -> Result<String, String> {
    let count = |marker: &str| app.lines().filter(|l| l.contains(marker)).count();
    match (count(COPY_CONFIRMED), count(COPY_DIVERGED)) {
        (0, d) => Err(format!(
            "no `{}` line ({d} diverged)",
            COPY_CONFIRMED.trim_end_matches(" n=")
        )),
        (c, 0) => Ok(format!("{c} editor answers confirmed the copy, 0 diverged")),
        (c, d) => Err(format!(
            "{d} editor answers diverged from the copy ({c} confirmed)"
        )),
    }
}

/// The composer's line for each Task report it sent, then `<code> thread=<name>` (atl-composer task_reports.rs `send`).
const SENT_LINE: &str = "task reports: sent ";
/// The composer's outbox thread (atl-composer outbox.rs `THREAD`).
const OUTBOX_THREAD: &str = "atl-outbox";

/// The input method's line for each call it sent on the editor's connection, then `<call> thread=<name>` (atl-ime field.rs `send`).
const IME_SENT_LINE: &str = "atl-ime: sent ";

/// Display-thread plan rule B1: binder sends the window's thread starts run on the outbox, so every Task report line and every input method send names the outbox thread: want >= 1 of each
/// (the window's first display and size; the stage's preedit, commit and delete) and 0 on any other thread.
fn outbox_check(app: &str) -> Result<String, String> {
    let reports = on_outbox(app, SENT_LINE, "task reports")?;
    let ime = on_outbox(app, IME_SENT_LINE, "input method sends")?;
    Ok(format!("{reports}; {ime}"))
}

/// The `<marker>... thread=<name>` lines of `app`: >= 1, all on the outbox.
fn on_outbox(app: &str, marker: &str, what: &str) -> Result<String, String> {
    let threads: Vec<&str> = app
        .lines()
        .filter_map(|l| l.split_once(marker).map(|(_, rest)| rest))
        .map(|rest| rest.rsplit_once(" thread=").map_or("", |(_, t)| t.trim()))
        .collect();
    let elsewhere: Vec<&str> = threads
        .iter()
        .copied()
        .filter(|t| *t != OUTBOX_THREAD)
        .collect();
    match (threads.len(), elsewhere.is_empty()) {
        (0, _) => Err(format!("no `{}` line", marker.trim())),
        (n, true) => Ok(format!("{n} {what}, all on {OUTBOX_THREAD}")),
        (n, false) => Err(format!(
            "{} of {n} {what} off {OUTBOX_THREAD}: {elsewhere:?}",
            elsewhere.len()
        )),
    }
}

/// After the death line: the relaunched process added a fresh client (addClient 2) and was bound again (onBindMethod 2), in that order.
fn relaunch_check(services: &str) -> Result<String, String> {
    let at = |needle: &str| services.find(needle);
    match (at(DEATH_LINE), at(READDED_LINE), at(REBOUND_LINE)) {
        (Some(d), Some(a), Some(b)) if d < a && a < b => {
            Ok("after the death: a fresh addClient, then a fresh bind (onBindMethod 2)".to_owned())
        }
        (d, a, b) => Err(format!(
            "want the death line, then `{READDED_LINE}`, then `{REBOUND_LINE}`; offsets {d:?}, {a:?}, {b:?}"
        )),
    }
}

/// After the kill, before the relaunch's client is added: the services counted exactly one client death, and its line names the app's uid with its entry removed and no client left
/// (the fixture is the only app). Lines after the relaunch are not this death: the run's stop kills the relaunched app too.
fn death_check(services: &str, app_uid: u32) -> Result<String, String> {
    let before = services
        .find(READDED_LINE)
        .map_or(services, |at| &services[..at]);
    // the counter's own lines, not the exit summary (`diag: total ...`), which the services write at the stop and which lands here when no relaunch added a client
    let counted: Vec<&str> = before
        .lines()
        .filter(|l| l.contains(DEATH_COUNTER) && !l.starts_with(DIAG_TOTAL))
        .collect();
    let removed: Vec<&str> = before.lines().filter(|l| l.contains(DEATH_LINE)).collect();
    let want = format!("{DEATH_LINE}{app_uid} ");
    match (counted.as_slice(), removed.as_slice()) {
        ([c], [r])
            if c.contains(&format!("{DEATH_COUNTER} 1 "))
                && r.contains(&want)
                && r.ends_with(NONE_LEFT) =>
        {
            Ok(format!(
                "one client death counted; uid {app_uid} entry removed, 0 clients left"
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

#[cfg(test)]
mod outbox_tests {
    use super::*;

    /// The line shape is task_reports.rs `send`'s; a report from another thread (the break: the window's, unnamed = `?`; before S3: `atl-task-sender`) or a line without a thread fails.
    /// An input method send, as field.rs `send` logs it.
    const IME: &str = "INFO atl-ime: sent Commit(\"x\") thread=atl-outbox\n";

    #[test]
    fn every_sent_line_must_name_the_outbox() {
        let good = "INFO atl-composer: task reports: sent 2 thread=atl-outbox\nINFO atl-composer: task reports: sent 1 thread=atl-outbox\n";
        assert_eq!(
            outbox_check(&format!("{good}{IME}")),
            Ok(
                "2 task reports, all on atl-outbox; 1 input method sends, all on atl-outbox"
                    .to_owned()
            )
        );
        let bad = format!("{good}{IME}INFO atl-composer: task reports: sent 1 thread=?\n");
        assert!(outbox_check(&bad).is_err());
        assert!(outbox_check(&format!("INFO atl-composer: task reports: sent 1\n{IME}")).is_err());
        assert!(outbox_check("").is_err());
        // an input method send made on the display thread, or none at all
        assert!(
            outbox_check(&format!(
                "{good}INFO atl-ime: sent Commit(\"x\") thread=atl-display\n"
            ))
            .is_err()
        );
        assert!(outbox_check(good).is_err());
    }

    /// field.rs `surrounding`'s line shapes: confirmed lines alone pass; any diverged line, or no confirmed line (the comparison never ran), fails.
    #[test]
    fn the_copy_must_be_confirmed_and_never_diverge() {
        let ok = "INFO atl-ime: editor text confirmed n=1: 17 units at 0 selection 17..17 composing Some((15, 17))\n";
        let bad = "WARN atl-ime: editor text diverged n=1: copy 17 units at 0 ... editor 15 units at 0 ...\n";
        assert!(copy_check(ok).is_ok());
        assert!(copy_check(&format!("{ok}{bad}")).is_err());
        assert!(copy_check(bad).is_err());
        assert!(
            copy_check("INFO atl-ime: editor text 15 units at 0 selection 15..15 composing None\n")
                .is_err()
        );
    }

    /// The run goes on through the relaunch, whose reports also land in the app log: the gate counts the first process's part only, so a red there stays red and the relaunch's lines are not counted.
    #[test]
    fn the_outbox_judges_the_first_process() {
        let first = format!(
            "INFO atl-composer: task reports: sent 2 thread=atl-outbox\n{IME}services: died\n"
        );
        let relaunch = "INFO atl-composer: task reports: sent 2 thread=atl-outbox\nINFO atl-composer: task reports: sent 1 thread=?\n";
        let app = format!("{first}{relaunch}");
        assert_eq!(
            outbox_check(first_part(&app, Some(first.len()))),
            Ok(
                "1 task reports, all on atl-outbox; 1 input method sends, all on atl-outbox"
                    .to_owned()
            )
        );
        let red = format!("INFO atl-composer: task reports: sent 2 thread=?\n{IME}");
        let broken = format!("{red}{relaunch}");
        assert!(outbox_check(first_part(&broken, Some(red.len()))).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEATH: &str = "diag: first answered IInputMethodManager.client died 1 at=100.000\n\
                         services: input_method client uid=10002 pid=7 died; entry removed, 0 clients left\n";

    /// A run stopped before the relaunch added a client: the services' exit summary (`diag: total`, atl_diag's format) follows, and is not a second death.
    #[test]
    fn the_exit_summary_is_not_a_death() {
        let services = format!(
            "{DEATH}diag: total answered IInputMethodManager.client died 1 first=100.000\n"
        );
        assert!(death_check(&services, 10002).is_ok());
    }

    /// Two deaths told before the relaunch's client stay red.
    #[test]
    fn two_deaths_are_red() {
        let services = format!(
            "{DEATH}diag: count answered IInputMethodManager.client died 2 at=101.000\n\
             services: input_method client uid=10002 pid=8 died; entry removed, 0 clients left\n"
        );
        assert!(death_check(&services, 10002).is_err());
    }

    /// The first process's part ends at its last whole line before the recorded length; a length inside a multi-byte char still cuts at a line end.
    #[test]
    fn the_first_process_ends_at_its_last_whole_line() {
        let app = "a\nかな\nrelaunched\n";
        // "a\n" is 2 bytes, "かな\n" 7: a length of 4 falls inside か
        *FIRST_END.lock().unwrap_or_else(|e| e.into_inner()) = Some(4);
        assert_eq!(first_process(app), "a\n");
        *FIRST_END.lock().unwrap_or_else(|e| e.into_inner()) = Some(9);
        assert_eq!(first_process(app), "a\nかな\n");
        *FIRST_END.lock().unwrap_or_else(|e| e.into_inner()) = None;
        assert_eq!(first_process(app), app);
    }

    /// Settled only once a retired shown buffer's release went out (or a fence rode none), with a post-draw received.
    #[test]
    fn settled_waits_for_the_paints_release() {
        let line = |held: u64, fenced: u64, unmatched: u64, post: u64| {
            format!(
                "1.0 INFO composer: counters releases_fenced={fenced} releases_fenced_unmatched={unmatched} releases_held={held} post_draw_received={post}\n"
            )
        };
        assert!(!composer_settled(""));
        assert!(!composer_settled(&line(2, 0, 0, 1)));
        assert!(!composer_settled(&line(0, 0, 0, 1)));
        assert!(!composer_settled(&line(1, 1, 0, 0)));
        assert!(composer_settled(&line(1, 1, 0, 1)));
        assert!(composer_settled(&line(1, 0, 1, 1)));
    }
}
