// SPDX-License-Identifier: MIT
//! The harness's pictures of the app's window: atld's control `capture` (atl-screencap as the shell uid: GET_COMPOSER, captureDisplayById, the reply's fence and GraphicBuffer, the RGBA rows on stdout).
//! The picture is the window's content, not the display: 1440x900 on the 1920x1200 gate display (the opening size). Every UI stage that judges pixels takes them through [`Probe`]: a probe records a
//! settled picture, and each sub-gate judges the recorded picture against its own expectation ([`judged`], or its own pixels).
//! Stage `capture` (capture-API S3): (a) the picture is the quadrants at the opening size (UI mode, the gtk fixture draws them); (b) the fixture itself calls GET_COMPOSER from its own process
//! (Main.java `logComposer`) and logs the status: an app uid must be refused with SECURITY (AOSP: captureDisplay needs READ_FRAME_BUFFER). `--break-capture-caller` expects `ok` instead,
//! so (b) fails on the real refusal: a harness-side break, the registry is not changed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    ACK_WAIT, DRAW_LINE, INPUT_SETTLE, Liveness, PONG_WAIT, Proxy, ProxyEvent, Sub, await_event,
    parse_wxh, quadrant_pixels, wait_for_line,
};
use crate::atld_run::capture_at;
use crate::composer::{fnv1a64, word};

/// The fixture's log line for its GET_COMPOSER (xtask/testdata/window Main.java): `composer GET_COMPOSER status=<ok|SECURITY|error> uid=<uid>`.
const COMPOSER_LINE: &str = "composer GET_COMPOSER status=";
/// How long a probe polls for two captures that agree. One attempt is a spawn, a GET_COMPOSER and a readback of the frame the window shows. It ends inside the gtk and hwui windows
/// (GTK_WINDOW_MS after the marker), so a window that never settles is reported as the picture's own failure, not as a cut-off control socket.
const CAPTURE_WAIT: Duration = Duration::from_secs(6);
/// The pause between two attempts.
const ATTEMPT_GAP: Duration = Duration::from_millis(250);
/// The stage's watch after the marker: the first draw, then the probe's waits (at most [`CAPTURE_WAIT`] for (a), then (b) and the ping).
pub(super) const CAPTURE_WINDOW_MS: u64 = 40_000;

/// One picture of the window: the size and RGBA rows of atld's reply and file, which [`Probe::shoot`] checks against each other. `digest` is FNV-1a 64 over `rgba`.
#[derive(Clone, Debug)]
pub(super) struct Shot {
    pub size: (u32, u32),
    pub digest: u64,
    pub rgba: Vec<u8>,
}

/// What a probe recorded, read by the sub-gates once it is joined: each picture in order (Ok, or why none settled), and the refused caller (capture (b)).
#[derive(Debug, Default)]
pub(super) struct Seen {
    pub(super) pictures: Vec<Result<Shot, String>>,
    pub(super) refusal: Option<Result<String, String>>,
}

/// What a probe needs from the run: atld's root and the app's package (the capture route), where atld writes the bytes, and where its pictures go.
pub(super) struct Probe {
    pub root: PathBuf,
    pub package: String,
    pub out_dir: PathBuf,
    /// `--break-capture-caller`: sub-gate (b) expects the fixture served (`ok`), so it fails on the registry's SECURITY.
    pub break_caller: bool,
    pub seen: Arc<Mutex<Seen>>,
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
        let picture = if wait_for_line(app_log, DRAW_LINE, ACK_WAIT) {
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

/// The capture stage's probe, from the proxy's side: (a) the picture, then (b) the fixture's refusal, then a ping must be answered.
pub(super) fn drive(proxy: Proxy, app_log: &Path, probe: &Probe) -> Result<Liveness, String> {
    probe.record_after_draw(app_log);
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

/// The capture stage's sub-gates: (a) the picture is the quadrants at the window's `opening` size, (b) the refusal, then the liveness the probe's ping showed.
pub(super) fn checks(
    seen: &Mutex<Seen>,
    opening: (u32, u32),
    liveness: &Result<Liveness, String>,
) -> Vec<Sub> {
    let refusal = seen
        .lock()
        .ok()
        .and_then(|s| s.refusal.clone())
        .unwrap_or_else(|| Err("the probe never sent the refused caller".to_owned()));
    vec![
        ("capture", judged(seen, 0, opening, false)),
        ("refused", refusal),
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
