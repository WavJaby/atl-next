// SPDX-License-Identifier: MIT
//! The harness's pictures of the app's window: atld's control `capture` (atl-screencap as the shell uid: GET_COMPOSER, captureDisplayById, the reply's fence and GraphicBuffer, the RGBA rows on stdout).
//! The picture is the window's content, not the display: 1440x900 on the 1920x1200 gate display (the opening size). Every UI stage that judges pixels takes them through [`Probe`]: a probe records a
//! settled picture, and each sub-gate judges the recorded picture against its own expectation ([`judged`], or its own pixels).
//! Stage `capture` (capture-API S3): (a) the picture is the quadrants at the opening size (UI mode, the gtk fixture draws them); (b) the fixture itself calls GET_COMPOSER from its own process
//! (Main.java `logComposer`) and logs the status: an app uid must be refused with SECURITY (AOSP: captureDisplay needs READ_FRAME_BUFFER). `--break-capture-caller` expects `ok` instead,
//! so (b) fails on the real refusal: a harness-side break, the registry is not changed.
//! (c) the probe sends `xdg_toplevel.close`: the services must receive the composer's REMOVE_TASK for the fixture's Task (`close`), and must have refused and counted the fixture's own REMOVE_TASK
//! naming a Task its uid does not own (`foreign`, Main.java `removeForeignTask`): the ownership refusal is the row's break, built into the run. Then AOSP's removeTask: the fixture runs
//! onPause, onStop, onDestroy in that order (`destroy`), and the services kill its process once it reported the destroy, not by the 1000 ms timeout, and every pid they killed is gone (`ended`).
//! (d) `timeout`, without `--exit-with-last-app` only (that run ends at the close): the probe starts the fixture again (control `start`), F11 arms a delayed onDestroy (Main.java), and the second close must end the process
//! by the services' timer (`no destroy within`) before that destroy. The delay must outlast the timer's printed duration (the product's KILL_TASK_PROCESSES_TIMEOUT, activity.rs).
//! Last, `atld-end`: with `--exit-with-last-app` atld ends the run with status 0 and every process it ran is gone; without, the run goes on until the harness's stop ([`atld_end_judged`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    ACK_WAIT, DRAW_LINE, INPUT_SETTLE, Liveness, PONG_WAIT, Proxy, ProxyEvent, Sub, parse_wxh,
    quadrant_pixels,
};
use crate::atld_run::capture_at;
use crate::composer::{fnv1a64, word};
use crate::wait::{Miss, Wait, until};

/// The fixture's log line for its GET_COMPOSER (xtask-harness/testdata/window Main.java): `composer GET_COMPOSER status=<ok|SECURITY|error> uid=<uid>`.
const COMPOSER_LINE: &str = "composer GET_COMPOSER status=";
/// The composer's line for the REMOVE_TASK it sent (atl-composer task_reports.rs), then the Task's number.
const SENT_LINE: &str = "task reports: REMOVE_TASK task ";
/// The services' line for a REMOVE_TASK they took (registry_service.rs `remove_task`): `services: removeTask task <n> of uid <uid>`.
const REMOVED_LINE: &str = "services: removeTask task ";
/// The fixture's lifecycle lines after the close (Main.java), in AOSP's removeTask order: pause, stop, destroy.
const LIFECYCLE: [&str; 3] = ["onPause reached", "onStop reached", "onDestroy reached"];
/// The services' removeTask line (activity_task.rs `remove_task`): `activity remove task <n> wait=<bool> at=<s>`.
const REMOVE_LINE: &str = "activity remove task ";
/// The services' kill line (activity.rs `kill_task_processes`): `task <n> removed: killing <package> pids [<pid>, ..] at=<s>`.
const KILLED_LINE: &str = " removed: killing ";
/// The timer's line when no destroy came within KILL_TASK_PROCESSES_TIMEOUT (activity.rs `kill_task_processes_on_timeout`).
const TIMED_OUT: &str = ": no destroy within ";
/// How long a killed pid may take to disappear: atld SIGKILLs the sandbox and its reaper thread waits on it; generous for a loaded host.
const GONE_WAIT: Duration = Duration::from_secs(5);
/// atld's line when the services' `LastAppEnded` ends the run (atld main.rs `last_app_ended`), and when it does not because the adaptation is off.
const RUN_ENDS: &str = "the run ends";
const RUN_GOES_ON: &str = "the run goes on (--exit-with-last-app false)";
const LAST_APP: &str = "last app ended (";
/// The reply hook's count of a refused REMOVE_TASK, and the cause naming the fixture's foreign Task (Main.java FOREIGN_TASK = i32::MAX).
const FOREIGN_REFUSAL: &str = "status:PERMISSION_DENIED ICompositorRegistry.REMOVE_TASK";
const FOREIGN_CAUSE: &str = "does not own task 2147483647";
/// Linux input KEY_F12 (input-event-codes.h): the fixture's foreign-probe trigger, KEYCODE_F12 on the Android side.
const KEY_F12: u32 = 88;
/// Linux input KEY_F11: the timeout sub-gate's switch, KEYCODE_F11 on the Android side (Main.java onKeyUp arms the delayed onDestroy). It reaches the relaunched window
/// because the proxy hands the injections over to it (wayland_proxy.rs `WindowRelease`).
const KEY_F11: u32 = 87;
/// The fixture's line when F11 armed the delay, and the line its delayed onDestroy logs first (`ms=` the delay, Main.java DESTROY_DELAY_MS).
const ARMED_LINE: &str = "destroy delay armed";
const DELAYED_LINE: &str = "onDestroy delayed ms=";
/// How long the relaunched fixture may take to draw: a cold start under load (ime.rs `RELAUNCH_WAIT`: 8 s on a loaded host, 2026-11-21).
const DRAW_AFTER_START_WAIT: Duration = Duration::from_secs(20);
/// How long the close waits for the services' line: one GTK dispatch plus one binder transaction; generous for a loaded host.
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// How long a probe polls for two captures that agree. One attempt is a spawn, a GET_COMPOSER and a readback of the frame the window shows. It ends inside the gtk and hwui windows
/// (GTK_WINDOW_MS after the marker), so a window that never settles is reported as the picture's own failure, not as a cut-off control socket.
const CAPTURE_WAIT: Duration = Duration::from_secs(6);
/// The pause between two attempts.
const ATTEMPT_GAP: Duration = Duration::from_millis(250);
/// The stage's watch after the marker: the first draw, then the probe's waits (at most [`CAPTURE_WAIT`] for (a), then (b) and the ping, then (c), then (d) where it runs).
pub(super) const CAPTURE_WINDOW_MS: u64 = 60_000;

/// One picture of the window: the size and RGBA rows of atld's reply and file, which [`Probe::shoot`] checks against each other. `digest` is FNV-1a 64 over `rgba`.
/// Serialized for the target agent's `live.json` without `rgba`: the remote stages (window.rs `REMOTE_STAGES`) judge size and digest only.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct Shot {
    pub size: (u32, u32),
    pub digest: u64,
    #[serde(skip)]
    pub rgba: Vec<u8>,
}

/// What a probe recorded, read by the sub-gates once it is joined: each picture in order (Ok, or why none settled), and the refused caller (capture (b)).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Seen {
    pub(super) pictures: Vec<Result<Shot, String>>,
    pub(super) refusal: Option<Result<String, String>>,
    /// Capture (c): the desktop's close reached the services as REMOVE_TASK, and the foreign one was refused.
    pub(super) close: Option<Result<String, String>>,
    pub(super) foreign: Option<Result<String, String>>,
    /// Capture (c) after the removal: the lifecycle order, and the kill with its pids gone.
    pub(super) destroy: Option<Result<String, String>>,
    pub(super) ended: Option<Result<String, String>>,
    /// Capture (d), when it runs: the second Task's timer ended its process before the delayed destroy.
    pub(super) timeout: Option<Result<String, String>>,
    /// Every process under atld just before the close (services, binderd, gralloc, the app, sandbox helpers): none may outlive the run's end (atld-end).
    pub(super) ran: Vec<u32>,
}

/// What a probe needs from the run: atld's root and the app's package (the capture route), where atld writes the bytes, and where its pictures go.
pub(super) struct Probe {
    pub root: PathBuf,
    pub package: String,
    pub out_dir: PathBuf,
    /// `--break-capture-caller`: sub-gate (b) expects the fixture served (`ok`), so it fails on the registry's SECURITY.
    pub break_caller: bool,
    pub seen: Arc<Mutex<Seen>>,
    /// The component atld starts again for sub-gate (d): `<package>/<activity>`, as the ime stage's relaunch.
    pub component: String,
    /// Sub-gate (d) runs: false with `--exit-with-last-app`, where atld ends the run at the first close.
    pub timeout_close: bool,
}

impl Probe {
    /// One capture: atld's reply and the bytes it wrote. The reply's size and digest must agree with the bytes and the bytes must be size x 4.
    fn shoot(&self) -> Result<Shot, String> {
        std::fs::create_dir_all(&self.out_dir).map_err(|e| e.to_string())?;
        let out = self.out_dir.join("picture.rgba");
        let (reply, rgba) =
            capture_at(&self.root, &self.package, &out).map_err(|e| format!("{e:#}"))?;
        let summary = reply
            .strip_prefix("ok screencap: ")
            .ok_or_else(|| format!("no picture: {reply}"))?;
        let size = summary
            .split(' ')
            .next()
            .and_then(parse_wxh)
            .and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)))
            .ok_or_else(|| format!("no size in the reply: {reply}"))?;
        let digest = fnv1a64(&rgba);
        let reported = summary.split("fnv1a64=").nth(1).unwrap_or_default();
        if reported != format!("{digest:016x}") {
            return Err(format!(
                "the reply says {reported}, the bytes are {digest:016x}"
            ));
        }
        let want = size.0 as usize * size.1 as usize * 4;
        if rgba.len() != want {
            return Err(format!(
                "the file holds {} bytes, the {}x{} picture is {want}",
                rgba.len(),
                size.0,
                size.1
            ));
        }
        Ok(Shot { size, digest, rgba })
    }

    /// Records one settled picture after the fixture's first draw (and the input settle). Without the draw, the record is the error.
    pub(super) fn record_after_draw(&self, app_log: &Path) {
        let drawn = until(&Wait::log(
            "draw",
            "the fixture never drew",
            ACK_WAIT,
            app_log,
            &|t: &str| t.contains(DRAW_LINE),
        ));
        let picture = if drawn.is_ok() {
            std::thread::sleep(INPUT_SETTLE);
            self.settle()
        } else {
            Err("the fixture never drew".to_owned())
        };
        self.push(picture);
    }

    /// Records one settled picture now: see [`Probe::settle`].
    pub(super) fn record_settled(&self) {
        let picture = self.settle();
        self.push(picture);
    }

    /// Captures until two in a row agree in size and digest (the window has shown its frame), at most [`CAPTURE_WAIT`]. Past that, the last capture is the error:
    /// a sub-gate that judges the picture then names it, a window still changing is not a picture of anything.
    fn settle(&self) -> Result<Shot, String> {
        let started = Instant::now();
        let mut previous: Option<Shot> = None;
        let mut last_error = String::new();
        loop {
            match self.shoot() {
                Ok(shot) => {
                    if previous
                        .as_ref()
                        .is_some_and(|p| p.size == shot.size && p.digest == shot.digest)
                    {
                        return Ok(shot);
                    }
                    previous = Some(shot);
                }
                Err(e) => {
                    previous = None;
                    last_error = e;
                }
            }
            if started.elapsed() >= CAPTURE_WAIT {
                return Err(match previous {
                    Some(p) => format!(
                        "no two captures agreed within {CAPTURE_WAIT:?}: last {}x{} digest {:016x}",
                        p.size.0, p.size.1, p.digest
                    ),
                    None => last_error,
                });
            }
            std::thread::sleep(ATTEMPT_GAP);
        }
    }

    fn push(&self, picture: Result<Shot, String>) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.pictures.push(picture);
        }
    }
}

/// The picture the probe recorded at `index` (its order), or why there is none.
pub(super) fn picture(seen: &Mutex<Seen>, index: usize) -> Result<Shot, String> {
    let seen = seen.lock().map_err(|e| e.to_string())?;
    seen.pictures.get(index).cloned().unwrap_or_else(|| {
        Err(format!(
            "the probe recorded {} picture(s), none at {index}",
            seen.pictures.len()
        ))
    })
}

/// Judges the picture at `index` as the quadrants at `size` ([`quadrants`]). `swap_rb`: the hwui break's expectation (R and B swapped), which the picture must then fail to match.
pub(super) fn judged(
    seen: &Mutex<Seen>,
    index: usize,
    size: (u32, u32),
    swap_rb: bool,
) -> Result<String, String> {
    quadrants(&picture(seen, index)?, size, swap_rb)
}

/// Whether `shot` is the quadrants at `(width, height)`: its size, then its digest (the fixture's draw, window.rs `quadrant_pixels`).
pub(super) fn quadrants(
    shot: &Shot,
    (width, height): (u32, u32),
    swap_rb: bool,
) -> Result<String, String> {
    if shot.size != (width, height) {
        return Err(format!(
            "the picture is {}x{}, the window is {width}x{height}",
            shot.size.0, shot.size.1
        ));
    }
    let mut want = quadrant_pixels(width, height);
    if swap_rb {
        want.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p.swap(0, 2));
    }
    let expected = fnv1a64(&want);
    if shot.digest != expected {
        return Err(format!(
            "digest {:016x}, the quadrants {expected:016x}",
            shot.digest
        ));
    }
    Ok(format!(
        "picture {width}x{height}, digest {expected:016x} = the quadrants"
    ))
}

/// The capture stage's probe, from the proxy's side: (a) the picture, then (b) the fixture's refusal, then a ping must be answered, then (c) the desktop closes the window (last: it ends the window).
pub(super) fn drive(
    proxy: Proxy,
    app_log: &Path,
    services_log: &Path,
    probe: &Probe,
    atld: u32,
) -> Result<Liveness, Miss> {
    probe.record_after_draw(app_log);
    let refusal = refused(app_log, probe);
    if let Ok(mut seen) = probe.seen.lock() {
        seen.refusal = Some(refusal);
    }
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = until(&Wait::event(
        "pong",
        "no pong to the first ping",
        PONG_WAIT,
        &proxy.events,
        &|e: &ProxyEvent| *e == ProxyEvent::Pong { serial: 1 },
    ))
    .is_ok();
    // the foreign probe's trigger (Main.java onKeyUp): only this stage presses F12; its refusal is counted before the close
    for pressed in [true, false] {
        proxy
            .inject_key(KEY_F12, pressed)
            .map_err(|e| e.to_string())?;
    }
    // not a probe stop: the close sub-gate judges what the services logged (close_judged)
    let _ = until(&Wait::log(
        "foreign-cause",
        "no foreign refusal logged",
        CLOSE_WAIT,
        services_log,
        &|t: &str| t.contains(FOREIGN_CAUSE),
    ));
    let ran = descendants(atld);
    if let Ok(mut seen) = probe.seen.lock() {
        seen.ran = ran;
    }
    proxy.inject_toplevel_close().map_err(|e| e.to_string())?;
    // the line, not its arguments: a removal the registry refused never writes it, and close_judged names what is missing
    let _ = until(&Wait::log(
        "removed",
        "no removal logged",
        CLOSE_WAIT,
        services_log,
        &|t: &str| t.contains(REMOVED_LINE),
    ));
    let _ = until(&Wait::log(
        "killed",
        "no kill logged",
        CLOSE_WAIT,
        services_log,
        &|t: &str| t.contains(KILLED_LINE),
    ));
    let read = atl_diag::logfiles::read;
    let services = read(services_log);
    let still = killed_pids(&services).map(gone_within);
    let app = read(app_log);
    if let Ok(mut seen) = probe.seen.lock() {
        seen.close = Some(close_judged(&app, &services));
        seen.foreign = Some(foreign_judged(&services));
        seen.destroy = Some(destroy_judged(&app));
        seen.ended = Some(ended_judged(&services, still));
    }
    // after the rows above read their logs: the second close's lines must not reach them
    if probe.timeout_close {
        let timeout = timed_close(&proxy, app_log, services_log, probe);
        if let Ok(mut seen) = probe.seen.lock() {
            seen.timeout = Some(timeout);
        }
    }
    Ok(Liveness {
        answered,
        ..Liveness::default()
    })
}

/// Sub-gate (c) close: after `xdg_toplevel.close` the composer sent REMOVE_TASK (its line names the Task) and the services took it for that Task from the fixture's uid (their line), and counted it.
/// The services accept only a Task the caller's uid owns, so a matching line is the right Task; the uid is the one the fixture logged for its GET_COMPOSER.
fn close_judged(app: &str, services: &str) -> Result<String, String> {
    let sent = app
        .lines()
        .find_map(|l| l.split_once(SENT_LINE).map(|(_, t)| t.trim()))
        .ok_or("xdg_toplevel.close: the composer sent no REMOVE_TASK")?;
    let uid = app
        .lines()
        .find(|l| l.contains(COMPOSER_LINE))
        .and_then(|l| word(l, "uid"))
        .ok_or("the fixture never logged its uid (GET_COMPOSER line)")?;
    let want = format!("{REMOVED_LINE}{sent} of uid {uid}");
    if !services.lines().any(|l| l.contains(&want)) {
        return Err(format!(
            "the services never took REMOVE_TASK of task {sent} from uid {uid} (want `{want}`)"
        ));
    }
    Ok(format!("REMOVE_TASK task {sent} of uid {uid} taken"))
}

/// Sub-gate (c) destroy: after the composer sent REMOVE_TASK the fixture logged onPause, onStop, onDestroy, in that order (Task.removeActivities -> finishIfPossible on the resumed top).
fn destroy_judged(app: &str) -> Result<String, String> {
    let after = app
        .split_once(SENT_LINE)
        .map(|(_, rest)| rest)
        .ok_or("xdg_toplevel.close: the composer sent no REMOVE_TASK")?;
    let mut at = 0;
    for step in LIFECYCLE {
        at += after[at..]
            .find(step)
            .ok_or_else(|| format!("no `{step}` after REMOVE_TASK in the order {LIFECYCLE:?}"))?;
    }
    Ok("onPause, onStop, onDestroy after REMOVE_TASK".to_owned())
}

/// The pids of the services' kill line; None when there is none.
fn killed_pids(services: &str) -> Option<Vec<i32>> {
    let line = services.lines().find(|l| l.contains(KILLED_LINE))?;
    let list = line.split_once(" pids [")?.1.split_once(']')?.0;
    list.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().parse().ok())
        .collect()
}

/// The `at=` wall stamp of the first line holding `needle`, seconds.
fn stamp(text: &str, needle: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.contains(needle))
        .and_then(|l| word(l, "at"))
        .and_then(|s| s.parse().ok())
}

/// Sub-gate (c) ended: the services killed the Task's process (at least one pid) after its destroy report, not by the timeout, and every pid is gone. `still`: the pids alive
/// [`GONE_WAIT`] after the kill line, None when there is no kill line. Reports the remove -> kill latency.
fn ended_judged(services: &str, still: Option<Vec<i32>>) -> Result<String, String> {
    let still = still.ok_or_else(|| format!("no `{KILLED_LINE}` line in the services log"))?;
    let pids = killed_pids(services).unwrap_or_default();
    if pids.is_empty() {
        return Err("the kill line names no pid".to_owned());
    }
    if let Some(l) = services.lines().find(|l| l.contains(TIMED_OUT)) {
        return Err(format!(
            "killed by the timeout, not after onDestroy: {}",
            l.trim()
        ));
    }
    if !still.is_empty() {
        return Err(format!(
            "pids {still:?} still alive {GONE_WAIT:?} after the kill"
        ));
    }
    let latency = match (stamp(services, REMOVE_LINE), stamp(services, KILLED_LINE)) {
        (Some(a), Some(b)) => format!("{:.0} ms", (b - a) * 1000.0),
        _ => "unknown".to_owned(),
    };
    Ok(format!(
        "pids {pids:?} killed after onDestroy and gone; remove -> kill {latency}"
    ))
}

/// Sub-gate (d) timeout: the fixture starts again through atld's control `start` (ime.rs relaunch does the same), F11 arms its delayed onDestroy, then the window closes. Only the lines after
/// that close are judged ([`timeout_judged`]): the services' timer must end the new Task's process before the delayed destroy.
fn timed_close(
    proxy: &Proxy,
    app_log: &Path,
    services_log: &Path,
    probe: &Probe,
) -> Result<String, String> {
    let app_start = len_of(app_log);
    let reply = crate::atld_run::control_at(&probe.root, &format!("start {}", probe.component))
        .map_err(|e| format!("relaunch: {e:#}"))?;
    if !reply.starts_with("ok") {
        return Err(format!("relaunch refused: {reply}"));
    }
    if !wait_line_after(app_log, app_start, DRAW_LINE, DRAW_AFTER_START_WAIT) {
        return Err("the relaunched fixture never drew".to_owned());
    }
    std::thread::sleep(INPUT_SETTLE);
    for pressed in [true, false] {
        proxy
            .inject_key(KEY_F11, pressed)
            .map_err(|e| e.to_string())?;
    }
    if !wait_line_after(app_log, app_start, ARMED_LINE, CLOSE_WAIT) {
        return Err(format!(
            "no `{ARMED_LINE}` after F11: the fixture did not arm its delay"
        ));
    }
    let app_close = len_of(app_log);
    let services_close = len_of(services_log);
    proxy.inject_toplevel_close().map_err(|e| e.to_string())?;
    wait_line_after(app_log, app_close, SENT_LINE, CLOSE_WAIT);
    wait_line_after(services_log, services_close, TIMED_OUT, CLOSE_WAIT);
    wait_line_after(services_log, services_close, KILLED_LINE, CLOSE_WAIT);
    let app = text_after(app_log, app_close);
    let services = text_after(services_log, services_close);
    timeout_judged(&app, &services, killed_pids(&services).map(gone_within))
}

/// Sub-gate (d) timeout, on the lines after the second close: the composer sent REMOVE_TASK for a Task; the services' timer fired for it (`no destroy within <D>`, D as the product prints
/// KILL_TASK_PROCESSES_TIMEOUT); the fixture's delayed onDestroy had not reached `onDestroy reached` by the kill (the destroy did not come first); the kill line for the Task follows the timer
/// line and its pids are gone. `still`: the pids alive after the kill ([`gone_within`]), None when there is no kill line. The delay (`onDestroy delayed ms=`) must outlast D.
fn timeout_judged(app: &str, services: &str, still: Option<Vec<i32>>) -> Result<String, String> {
    let sent = app
        .lines()
        .find_map(|l| l.split_once(SENT_LINE).map(|(_, t)| t.trim()))
        .ok_or("the second close: the composer sent no REMOVE_TASK")?;
    let delayed = app
        .lines()
        .find(|l| l.contains(DELAYED_LINE))
        .ok_or_else(|| {
            format!("no `{DELAYED_LINE}` line: the fixture's onDestroy was not delayed")
        })?;
    let delay: f64 = word(delayed, "ms")
        .and_then(|ms| ms.parse().ok())
        .ok_or_else(|| format!("no delay in `{}`", delayed.trim()))?;
    let timer_text = format!("remove task {sent}{TIMED_OUT}");
    let timer = services
        .lines()
        .position(|l| l.contains(&timer_text))
        .ok_or_else(|| {
            format!("no `remove task {sent}: no destroy within <D>` line: the destroy came first, or the timer did not fire")
        })?;
    let timeout = services
        .lines()
        .nth(timer)
        .and_then(|l| l.split_once(TIMED_OUT))
        .and_then(|(_, d)| debug_ms(d.trim()))
        .ok_or("the timer's line names no duration")?;
    if delay <= timeout {
        return Err(format!(
            "the fixture's delay {delay} ms does not outlast the timeout {timeout} ms (KILL_TASK_PROCESSES_TIMEOUT, activity.rs): the destroy comes first"
        ));
    }
    if app.contains(LIFECYCLE[2]) {
        return Err(format!(
            "`{}` before the kill: the destroy came first",
            LIFECYCLE[2]
        ));
    }
    let kill_text = format!("task {sent}{KILLED_LINE}");
    let kill = services
        .lines()
        .position(|l| l.contains(&kill_text))
        .ok_or_else(|| format!("no `{kill_text}`: task {sent} was not killed"))?;
    if kill < timer {
        return Err(format!(
            "the kill of task {sent} came before its timer line"
        ));
    }
    let still = still.ok_or_else(|| format!("no `{KILLED_LINE}` line in the services log"))?;
    let pids = killed_pids(services).unwrap_or_default();
    if pids.is_empty() {
        return Err("the kill line names no pid".to_owned());
    }
    if !still.is_empty() {
        return Err(format!(
            "pids {still:?} still alive {GONE_WAIT:?} after the kill"
        ));
    }
    Ok(format!(
        "task {sent}: the timer ({timeout} ms) ended pids {pids:?} before the delayed onDestroy ({delay} ms); every pid gone"
    ))
}

/// A `Duration` as its `Debug` prints it (`1s`, `1.5s`, `250ms`, `250µs`), in ms.
fn debug_ms(text: &str) -> Option<f64> {
    let (number, per_ms) = if let Some(n) = text.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = text.strip_suffix("\u{b5}s") {
        (n, 1e-3)
    } else if let Some(n) = text.strip_suffix("ns") {
        (n, 1e-6)
    } else {
        (text.strip_suffix('s')?, 1000.0)
    };
    number.parse::<f64>().ok().map(|v| v * per_ms)
}

/// The pids of [`killed_pids`] still alive [`GONE_WAIT`] after the kill; none when they are all gone.
fn gone_within(pids: Vec<i32>) -> Vec<i32> {
    let alive = |pid: &i32| Path::new(&format!("/proc/{pid}")).exists();
    let deadline = Instant::now() + GONE_WAIT;
    while pids.iter().any(alive) && Instant::now() < deadline {
        std::thread::sleep(ATTEMPT_GAP);
    }
    pids.into_iter().filter(|p| alive(p)).collect()
}

/// The log's length in bytes (all its segments): the offset [`text_after`] and [`wait_line_after`] count from.
fn len_of(path: &Path) -> usize {
    atl_diag::logfiles::read(path).len()
}

/// The log from byte `at` on. A log read mid-line may end inside a multi-byte char: `at` moves back to a char boundary.
fn text_after(path: &Path, at: usize) -> String {
    let text = atl_diag::logfiles::read(path);
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[at..].to_owned()
}

/// Whether the log holds `needle` after byte `at` within `within`.
fn wait_line_after(path: &Path, at: usize, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !text_after(path, at).contains(needle) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ATTEMPT_GAP);
    }
    true
}

/// Every live process under `atld` (its /proc ppid chain reaches it).
fn descendants(atld: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| super::descends_from(pid, atld))
        .collect()
}

/// atld logged that the last app's end ends the run (`--exit-with-last-app` on).
pub(super) fn ended_with_last_app(atld_log: &str) -> bool {
    atld_log
        .lines()
        .any(|l| l.contains(LAST_APP) && l.trim_end().ends_with(RUN_ENDS))
}

/// Sub-gate (c) atld-end: with `expect_end` (atld `--exit-with-last-app true`) the close's end of the last app ended the run: atld logged it, exited 0 by itself (`ended`), and none of the
/// processes it ran before the close is left. Without it atld logged that the run goes on and was still running for the harness's `stop` (`ended` None): today's rule.
pub(super) fn atld_end_judged(
    seen: &Mutex<Seen>,
    atld_log: &str,
    ended: Option<std::process::ExitStatus>,
    expect_end: bool,
) -> Result<String, String> {
    if !expect_end {
        if !atld_log.contains(RUN_GOES_ON) {
            return Err(format!(
                "atld never logged `{RUN_GOES_ON}` after the last app ended"
            ));
        }
        return match ended {
            None => Ok(
                "adaptation off: atld still ran after the last app ended, until stop".to_owned(),
            ),
            Some(s) => Err(format!("adaptation off, yet atld exited by itself: {s}")),
        };
    }
    if !ended_with_last_app(atld_log) {
        return Err(format!("atld never logged `{LAST_APP}..: {RUN_ENDS}`"));
    }
    let status = ended.ok_or("atld logged the end but was still running at the window's end")?;
    if !status.success() {
        return Err(format!("atld ended the run with {status}, want status 0"));
    }
    let ran = seen.lock().map(|s| s.ran.clone()).unwrap_or_default();
    if ran.is_empty() {
        return Err("no process under atld was recorded before the close".to_owned());
    }
    let alive = |pid: &u32| Path::new(&format!("/proc/{pid}")).exists();
    let deadline = Instant::now() + GONE_WAIT;
    while ran.iter().any(alive) && Instant::now() < deadline {
        std::thread::sleep(ATTEMPT_GAP);
    }
    let left: Vec<u32> = ran.iter().copied().filter(alive).collect();
    if !left.is_empty() {
        return Err(format!(
            "pids {left:?} atld ran outlived it by {GONE_WAIT:?}"
        ));
    }
    Ok(format!(
        "atld exited 0 after the last app ended; the {} processes it ran are gone",
        ran.len()
    ))
}

/// Sub-gate (c) foreign: the fixture's REMOVE_TASK naming a Task its uid does not own (Main.java FOREIGN_TASK) was refused and counted, its cause naming that Task.
fn foreign_judged(services: &str) -> Result<String, String> {
    services
        .lines()
        .find(|l| l.contains(FOREIGN_REFUSAL) && l.contains(FOREIGN_CAUSE))
        .map(|l| format!("refused and counted: {}", l.trim()))
        .ok_or_else(|| {
            format!("no `{FOREIGN_REFUSAL}` count with `{FOREIGN_CAUSE}` in the services log")
        })
}

/// Sub-gate (b): the fixture's own GET_COMPOSER (its log line) must be refused with SECURITY, the status an app uid gets. Under the break, `ok` is the expected status,
/// so the real refusal fails the sub-gate. A missing line fails it too.
fn refused(app_log: &Path, probe: &Probe) -> Result<String, String> {
    if until(&Wait::log(
        "composer-line",
        "the fixture never logged its GET_COMPOSER",
        ACK_WAIT,
        app_log,
        &|t: &str| t.contains(COMPOSER_LINE),
    ))
    .is_err()
    {
        return Err("the fixture never logged its GET_COMPOSER".to_owned());
    }
    let text = std::fs::read_to_string(app_log).unwrap_or_default();
    let line = text
        .lines()
        .find(|l| l.contains(COMPOSER_LINE))
        .unwrap_or_default();
    let status = word(line, "status").unwrap_or_default();
    let uid = word(line, "uid").unwrap_or_default();
    let (want, what) = if probe.break_caller {
        ("ok", "served")
    } else {
        ("SECURITY", "refused with SECURITY")
    };
    if status != want {
        return Err(format!(
            "uid {uid} got status {status}, want {want}: {line}"
        ));
    }
    Ok(format!("uid {uid} {what}"))
}

/// The capture stage's sub-gates: (a) the picture is the quadrants at the window's `opening` size, (b) the refusal, then the liveness the probe's ping showed.
/// `timed`: sub-gate (d) ran (the run goes on after the first close), so its row is judged too.
pub(super) fn checks(
    seen: &Mutex<Seen>,
    opening: (u32, u32),
    liveness: &Result<Liveness, String>,
    timed: bool,
) -> Vec<Sub> {
    let refusal = seen
        .lock()
        .ok()
        .and_then(|s| s.refusal.clone())
        .unwrap_or_else(|| Err("the probe never sent the refused caller".to_owned()));
    let (close, foreign) = seen
        .lock()
        .ok()
        .map(|s| (s.close.clone(), s.foreign.clone()))
        .unwrap_or_default();
    let (destroy, ended) = seen
        .lock()
        .ok()
        .map(|s| (s.destroy.clone(), s.ended.clone()))
        .unwrap_or_default();
    let never = || Err("the probe never closed the window".to_owned());
    let timeout = seen.lock().ok().and_then(|s| s.timeout.clone());
    let mut subs = vec![
        ("capture", judged(seen, 0, opening, false)),
        ("refused", refusal),
        ("close", close.unwrap_or_else(never)),
        ("foreign", foreign.unwrap_or_else(never)),
        ("destroy", destroy.unwrap_or_else(never)),
        ("ended", ended.unwrap_or_else(never)),
        (
            "pong",
            match liveness {
                Ok(live) if live.answered => {
                    Ok("a ping after the captures was answered".to_owned())
                }
                Ok(_) => Err("no pong to the ping after the captures".to_owned()),
                Err(e) => Err(e.clone()),
            },
        ),
    ];
    if timed {
        subs.push((
            "timeout",
            timeout.unwrap_or_else(|| Err("the probe never ran the timed close".to_owned())),
        ));
    }
    subs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Log lines in the forms the composer, the fixture and the services write them (task_reports.rs, Main.java, registry_service.rs, atl-diag `line`).
    const APP: &str = "I AtlWindow: composer GET_COMPOSER status=SECURITY uid=10001\nI atl-composer: task reports: REMOVE_TASK task 1\n";
    const TAKEN: &str = "services: removeTask task 1 of uid 10001\n";
    const REFUSED: &str = "diag: first status:PERMISSION_DENIED ICompositorRegistry.REMOVE_TASK 1 at=1.000 cause=uid 10001 does not own task 2147483647\n";

    /// The close passes on the same Task and uid; another Task or another uid fails it.
    #[test]
    fn close_needs_the_sent_task_from_the_fixture_uid() {
        assert!(close_judged(APP, TAKEN).is_ok());
        assert!(close_judged(APP, &TAKEN.replace("task 1 ", "task 2 ")).is_err());
        assert!(close_judged(APP, &TAKEN.replace("uid 10001", "uid 10002")).is_err());
        assert!(
            close_judged(
                "I AtlWindow: composer GET_COMPOSER status=SECURITY uid=10001\n",
                TAKEN
            )
            .is_err()
        );
    }

    /// The foreign row needs the refusal count with the foreign Task's cause; an accepted removal is not one.
    #[test]
    fn foreign_needs_the_refusal_of_the_foreign_task() {
        assert!(foreign_judged(REFUSED).is_ok());
        assert!(foreign_judged(TAKEN).is_err());
        assert!(foreign_judged(&REFUSED.replace("2147483647", "1")).is_err());
    }

    /// The lifecycle must follow REMOVE_TASK in pause, stop, destroy order; a missing or reordered step fails, as does a lifecycle logged only before the close.
    #[test]
    fn destroy_needs_pause_stop_destroy_after_the_close() {
        let ordered = format!(
            "{APP}I AtlWindow: onPause reached\nI AtlWindow: onStop reached\nI AtlWindow: onDestroy reached\n"
        );
        assert!(destroy_judged(&ordered).is_ok());
        assert!(destroy_judged(&ordered.replace("I AtlWindow: onStop reached\n", "")).is_err());
        let swapped = format!(
            "{APP}I AtlWindow: onPause reached\nI AtlWindow: onDestroy reached\nI AtlWindow: onStop reached\n"
        );
        assert!(destroy_judged(&swapped).is_err());
        let before = "I AtlWindow: onPause reached\nI AtlWindow: onStop reached\nI AtlWindow: onDestroy reached\nI atl-composer: task reports: REMOVE_TASK task 1\n";
        assert!(destroy_judged(before).is_err());
    }

    /// The kill line in the form activity.rs writes it; a timeout kill, a survivor, no kill line or no pid fails the row. 250 ms = the two stamps' difference.
    #[test]
    fn ended_needs_a_kill_after_destroy_with_every_pid_gone() {
        let killed = "services: activity remove task 1 wait=true at=10.000\nservices: task 1 removed: killing com.example.atlwindow pids [4242] at=10.250\n";
        assert_eq!(killed_pids(killed), Some(vec![4242]));
        let ok = ended_judged(killed, Some(vec![])).expect("a kill after destroy, its pid gone");
        assert!(ok.contains("250 ms"), "{ok}");
        assert!(ended_judged(killed, Some(vec![4242])).is_err());
        assert!(ended_judged(killed, None).is_err());
        assert!(ended_judged(&killed.replace("[4242]", "[]"), Some(vec![])).is_err());
        let timed = format!("{killed}services: remove task 1: no destroy within 1s\n");
        assert!(ended_judged(&timed, Some(vec![])).is_err());
    }

    /// atld-end both ways, with atld's lines as main.rs `last_app_ended` writes them. The recorded pid is above pid_max (2^22), so it is gone; an empty record fails.
    #[test]
    fn atld_end_needs_status_0_and_no_survivor_or_the_run_going_on() {
        use std::os::unix::process::ExitStatusExt as _;
        let exit = |code: i32| Some(std::process::ExitStatus::from_raw(code << 8));
        let gone = Mutex::new(Seen {
            ran: vec![u32::MAX],
            ..Seen::default()
        });
        let ends = "atld: last app ended (pid 77, finished): the run ends\n";
        assert!(atld_end_judged(&gone, ends, exit(0), true).is_ok());
        assert!(atld_end_judged(&gone, ends, exit(1), true).is_err());
        assert!(atld_end_judged(&gone, ends, None, true).is_err());
        assert!(atld_end_judged(&gone, "", exit(0), true).is_err());
        assert!(atld_end_judged(&Mutex::new(Seen::default()), ends, exit(0), true).is_err());
        let goes_on =
            "atld: last app ended (pid 77): the run goes on (--exit-with-last-app false)\n";
        assert!(atld_end_judged(&gone, goes_on, None, false).is_ok());
        assert!(atld_end_judged(&gone, goes_on, exit(0), false).is_err());
        // the break: atld ran with the adaptation off while the check expects the end
        assert!(atld_end_judged(&gone, goes_on, None, true).is_err());
        assert!(ended_with_last_app(ends) && !ended_with_last_app(goes_on));
    }

    /// The timed close passes when the timer ended the new Task's process before its delayed destroy, the kill after the timer, every pid gone. A destroy first, a delay that does not
    /// outlast the timer, a kill before the timer, no kill, a pid alive, no delay or no timer line each fail. The 5000 ms delay is the fixture's; `1s` / `2500ms` are the product's printed timeouts.
    #[test]
    fn timeout_needs_the_timer_before_the_delayed_destroy() {
        let app = "I atl-composer: task reports: REMOVE_TASK task 2\nI AtlWindow: onPause reached\nI AtlWindow: onStop reached\nI AtlWindow: onDestroy delayed ms=5000\n";
        let timer = "services: remove task 2: no destroy within 1s\n";
        let kill =
            "services: task 2 removed: killing com.example.atlwindow pids [4242] at=12.000\n";
        let services = format!("{timer}{kill}");
        assert!(timeout_judged(app, &services, Some(vec![])).is_ok());
        assert!(timeout_judged(app, &services, Some(vec![4242])).is_err());
        assert!(timeout_judged(app, &services, None).is_err());
        assert!(timeout_judged(app, &format!("{kill}{timer}"), Some(vec![])).is_err());
        assert!(timeout_judged(app, kill, Some(vec![])).is_err());
        assert!(
            timeout_judged(
                app,
                &services.replace("task 2 removed", "task 3 removed"),
                Some(vec![])
            )
            .is_err()
        );
        assert!(
            timeout_judged(
                app,
                &services.replace("remove task 2:", "remove task 3:"),
                Some(vec![])
            )
            .is_err()
        );
        assert!(
            timeout_judged(&app.replace("ms=5000", "ms=1000"), &services, Some(vec![])).is_err()
        );
        assert!(
            timeout_judged(
                &app.replace("ms=5000", "ms=3000"),
                &services.replace("within 1s", "within 2500ms"),
                Some(vec![])
            )
            .is_ok()
        );
        let destroyed = format!("{app}I AtlWindow: onDestroy reached\n");
        assert!(timeout_judged(&destroyed, &services, Some(vec![])).is_err());
        assert!(
            timeout_judged(
                &app.replace("onDestroy delayed ms=5000\n", ""),
                &services,
                Some(vec![])
            )
            .is_err()
        );
    }

    /// `debug_ms` reads a `Duration`'s `Debug` text; the expected values are the durations' own (from_millis, from_micros), not the parser's.
    #[test]
    fn debug_ms_reads_the_duration_debug_text() {
        let ms = |d: Duration| debug_ms(&format!("{d:?}"));
        assert_eq!(ms(Duration::from_secs(1)), Some(1000.0));
        assert_eq!(ms(Duration::from_millis(1500)), Some(1500.0));
        assert_eq!(ms(Duration::from_millis(250)), Some(250.0));
        assert_eq!(ms(Duration::from_micros(250)), Some(0.25));
        assert_eq!(debug_ms("soon"), None);
    }
}
