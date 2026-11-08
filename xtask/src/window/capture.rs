// SPDX-License-Identifier: MIT
//! Stage `capture` (capture-API S3): the window's display capture through the harness route, not the composer's log. UI mode, the gtk fixture draws the quadrants.
//! (a) atld's control `capture` runs atl-screencap as the shell uid: GET_COMPOSER, captureDisplayById, the reply's fence and GraphicBuffer, the RGBA rows on stdout. Polled until the bytes
//! are the quadrants at the opening size; the digest is recomputed here from the bytes, and the reply's own digest is only checked against them.
//! (b) `capture-as` runs it as the fixture's own uid: the registry refuses a caller that is not the shell or system with SECURITY (AOSP: captureDisplay needs READ_FRAME_BUFFER).
//! `--break-capture-caller` sends (b) as the shell uid instead: the registry serves that, so (b) must fail. The break is the gate's own caller, not a switch in product code.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    ACK_WAIT, DRAW_LINE, INPUT_SETTLE, Liveness, PONG_WAIT, Proxy, ProxyEvent, Sub, await_event,
    quadrant_pixels, wait_for_line,
};
use crate::atld_run::capture_at;
use crate::composer::fnv1a64;

/// The shell uid, the one the registry serves (permission.rs SHELL_UID): what `--break-capture-caller` sends (b) as.
pub(super) const SHELL_UID: u32 = 2000;
/// How long (a) polls for the quadrants after the first draw. One attempt is a spawn, a GET_COMPOSER and a readback of the frame the window shows.
const CAPTURE_WAIT: Duration = Duration::from_secs(15);
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
    /// The uid of the refused caller: the app's own uid, or [`SHELL_UID`] under `--break-capture-caller`.
    pub refused_as: u32,
    pub out_dir: PathBuf,
    pub seen: Arc<Mutex<Seen>>,
}

/// The capture stage's probe, from the proxy's side: once the fixture has drawn, (a) and (b) run through atld, then a ping must be answered.
pub(super) fn drive(proxy: Proxy, app_log: &Path, probe: &Probe) -> Result<Liveness, String> {
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    std::fs::create_dir_all(&probe.out_dir).map_err(|e| e.to_string())?;
    let picture = poll_picture(probe);
    let refusal = refused(probe);
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    *probe.seen.lock().map_err(|e| e.to_string())? = Seen {
        picture: Some(picture),
        refusal: Some(refusal),
    };
    Ok(Liveness {
        answered,
        ..Liveness::default()
    })
}

/// Sub-gate (a): atld's capture until its bytes are the quadrants at the opening size, or [`CAPTURE_WAIT`] passes.
fn poll_picture(probe: &Probe) -> Result<String, String> {
    let started = Instant::now();
    let out = probe.out_dir.join("picture.rgba");
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let (reply, bytes) =
            capture_at(&probe.root, None, &probe.package, &out).map_err(|e| format!("{e:#}"))?;
        match judge_picture(&reply, &bytes, probe.opening) {
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
fn judge_picture(reply: &str, bytes: &[u8], (width, height): (u32, u32)) -> Result<String, String> {
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
    let want = quadrant_pixels(width, height);
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

/// Sub-gate (b): atld's `capture-as` as `probe.refused_as` must be refused with SECURITY. A served capture, or any other failure, fails it.
fn refused(probe: &Probe) -> Result<String, String> {
    let out = probe.out_dir.join("refused.rgba");
    let (reply, _) = capture_at(&probe.root, Some(probe.refused_as), &probe.package, &out)
        .map_err(|e| format!("{e:#}"))?;
    if reply.starts_with("ok ") {
        return Err(format!(
            "uid {} was served the composer: {reply}",
            probe.refused_as
        ));
    }
    if reply.contains("refused: SECURITY") {
        return Ok(format!("uid {} refused with SECURITY", probe.refused_as));
    }
    Err(format!(
        "uid {} not refused with SECURITY: {reply}",
        probe.refused_as
    ))
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
