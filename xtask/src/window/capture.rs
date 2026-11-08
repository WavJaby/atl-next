// SPDX-License-Identifier: MIT
//! Stage `capture` (capture-API S3): the window's display capture through the harness route, not the composer's log. UI mode, the gtk fixture draws the quadrants.
//! (a) atld's control `capture` runs atl-screencap as the shell uid: GET_COMPOSER, captureDisplayById, the reply's fence and GraphicBuffer, the RGBA rows on stdout. Polled until the bytes
//! are the quadrants at the opening size; the digest is recomputed here from the bytes, and the reply's own digest is only checked against them.
//! (b) the fixture itself calls GET_COMPOSER from its own process (Main.java `logComposer`) and logs the status: an app uid must be refused with SECURITY (AOSP: captureDisplay
//! needs READ_FRAME_BUFFER). `--break-capture-caller` expects `ok` instead, so (b) fails on the real refusal: a harness-side break, the registry is not changed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    ACK_WAIT, DRAW_LINE, INPUT_SETTLE, Liveness, PONG_WAIT, Proxy, ProxyEvent, Sub, await_event,
    quadrant_pixels, wait_for_line,
};
use crate::atld_run::capture_at;
use crate::composer::{fnv1a64, word};

/// The fixture's log line for its GET_COMPOSER (xtask/testdata/window Main.java): `composer GET_COMPOSER status=<ok|SECURITY|error> uid=<uid>`.
const COMPOSER_LINE: &str = "composer GET_COMPOSER status=";
/// How long (a) polls for the quadrants after the first draw. One attempt is a spawn, a GET_COMPOSER and a readback of the frame the window shows. It ends inside the gtk and
/// hwui windows (GTK_WINDOW_MS after the marker), so a mismatch is reported as the picture's own failure, not as a cut-off control socket.
const CAPTURE_WAIT: Duration = Duration::from_secs(6);
/// The pause between two attempts of (a).
const ATTEMPT_GAP: Duration = Duration::from_millis(250);
/// The stage's watch after the marker: the first draw, then the probe's waits (at most [`CAPTURE_WAIT`] for (a), then (b) and the ping).
pub(super) const CAPTURE_WINDOW_MS: u64 = 40_000;

/// The probe's record, filled by [`drive`] and read by [`checks`]: sub-gates (a) and (b). None: the probe never reached it.
#[derive(Debug, Default)]
pub(super) struct Seen {
    picture: Option<Result<String, String>>,
    refusal: Option<Result<String, String>>,
}

/// What the probe needs from the run.
pub(super) struct Probe {
    pub root: PathBuf,
    pub package: String,
    /// The window's content size: the display's opening size (window.rs `opening_size`).
    pub opening: (u32, u32),
    /// `--break-capture-caller`: sub-gate (b) expects the fixture served (`ok`), so it fails on the registry's SECURITY.
    pub break_caller: bool,
    pub out_dir: PathBuf,
    pub seen: Arc<Mutex<Seen>>,
}

/// The capture stage's probe, from the proxy's side: (a) the picture, then (b) the fixture's refusal, then a ping must be answered.
pub(super) fn drive(proxy: Proxy, app_log: &Path, probe: &Probe) -> Result<Liveness, String> {
    record_picture(app_log, probe, false);
    let refusal = refused(app_log, probe);
    if let Ok(mut seen) = probe.seen.lock() {
        seen.refusal = Some(refusal);
    }
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    Ok(Liveness {
        answered,
        ..Liveness::default()
    })
}

/// Records sub-gate (a) in the probe: once the fixture has drawn, the picture is polled until it is the quadrants at the opening size. `swap_rb` is the hwui break's
/// expectation (R and B swapped), which the picture must then fail to match.
pub(super) fn record_picture(app_log: &Path, probe: &Probe, swap_rb: bool) {
    let picture = if wait_for_line(app_log, DRAW_LINE, ACK_WAIT) {
        std::thread::sleep(INPUT_SETTLE);
        std::fs::create_dir_all(&probe.out_dir)
            .map_err(|e| e.to_string())
            .and_then(|()| poll_picture(probe, swap_rb))
    } else {
        Err("the fixture never drew".to_owned())
    };
    if let Ok(mut seen) = probe.seen.lock() {
        seen.picture = Some(picture);
    }
}

/// Sub-gate (a) as the probe recorded it: gtk and hwui read their readback through here too.
pub(super) fn picture(seen: &Mutex<Seen>) -> Result<String, String> {
    seen.lock()
        .map_err(|e| e.to_string())?
        .picture
        .clone()
        .unwrap_or_else(|| Err("the probe never captured".to_owned()))
}

/// Sub-gate (a): atld's capture until its bytes are the quadrants at the opening size, or [`CAPTURE_WAIT`] passes.
fn poll_picture(probe: &Probe, swap_rb: bool) -> Result<String, String> {
    let started = Instant::now();
    let out = probe.out_dir.join("picture.rgba");
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let (reply, bytes) =
            capture_at(&probe.root, &probe.package, &out).map_err(|e| format!("{e:#}"))?;
        match judge_picture(&reply, &bytes, probe.opening, swap_rb) {
            Ok(pass) => return Ok(format!("{pass}, after {attempts} capture(s)")),
            Err(why) if started.elapsed() >= CAPTURE_WAIT => {
                return Err(format!("after {attempts} captures: {why}"));
            }
            Err(_) => std::thread::sleep(ATTEMPT_GAP),
        }
    }
}

/// Whether atld's reply and the bytes it wrote are the quadrants at `(width, height)`. Size and digest come from the fixture's draw (window.rs `quadrant_pixels`), never from the
/// reply; the reply's digest must agree with the bytes.
fn judge_picture(
    reply: &str,
    bytes: &[u8],
    (width, height): (u32, u32),
    swap_rb: bool,
) -> Result<String, String> {
    let summary = reply
        .strip_prefix("ok screencap: ")
        .ok_or_else(|| format!("no picture: {reply}"))?;
    let size = summary.split(' ').next().unwrap_or_default();
    let want_size = format!("{width}x{height}");
    if size != want_size {
        return Err(format!(
            "the capture is {size}, the opening size is {want_size}"
        ));
    }
    let mut want = quadrant_pixels(width, height);
    if swap_rb {
        want.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p.swap(0, 2));
    }
    if bytes.len() != want.len() {
        return Err(format!(
            "the file holds {} bytes, the picture is {}",
            bytes.len(),
            want.len()
        ));
    }
    let got = fnv1a64(bytes);
    let expected = fnv1a64(&want);
    if got != expected {
        return Err(format!("digest {got:016x}, the quadrants {expected:016x}"));
    }
    let reported = summary.split("fnv1a64=").nth(1).unwrap_or_default();
    if reported != format!("{got:016x}") {
        return Err(format!(
            "the reply says {reported}, the bytes are {got:016x}"
        ));
    }
    Ok(format!(
        "picture {want_size}, digest {got:016x} = the quadrants"
    ))
}

/// Sub-gate (b): the fixture's own GET_COMPOSER (its log line) must be refused with SECURITY, the status an app uid gets. Under the break, `ok` is the expected status,
/// so the real refusal fails the sub-gate. A missing line fails it too.
fn refused(app_log: &Path, probe: &Probe) -> Result<String, String> {
    if !wait_for_line(app_log, COMPOSER_LINE, ACK_WAIT) {
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

/// The stage's sub-gates: (a) the picture, (b) the refusal, then the liveness the probe's ping showed.
pub(super) fn checks(seen: &Seen, liveness: &Result<Liveness, String>) -> Vec<Sub> {
    vec![
        (
            "capture",
            seen.picture
                .clone()
                .unwrap_or_else(|| Err("the probe never captured".to_owned())),
        ),
        (
            "refused",
            seen.refusal
                .clone()
                .unwrap_or_else(|| Err("the probe never sent the refused caller".to_owned())),
        ),
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
    ]
}
