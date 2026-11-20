// SPDX-License-Identifier: MIT
//! Stage `ui-block` of `xtask test window` (composer threading S2, composer backpressure): a SurfaceView whose thread keeps posting frames (xtask-harness/testdata/ui-block, Main.java) while the app's UI
//! thread sleeps 2 s (block). The fixture's steady phase before it is stage slow-display's subject (slow_display.rs), not judged here.
//! - Block: the window takes nothing, so the composer holds until its bound (`bound_ms` on its start line), then latches every vsync as a display that presents: the frames posted inside the sleep are at
//!   least floor((block - bound) / period) - [`SLACK`], and no dequeue timed out (`-110`). A compositor that latches only when the window's thread runs leaves the producer blocked for the whole sleep.

use super::{ACK_WAIT, Liveness, Sub, await_event, wait_for_line};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};
use std::path::Path;
use std::time::Duration;

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/ui-block";
/// The fixture's result line (Main.java draw).
const RESULT: &str = "AtlWindow: ub result ";
/// The composer's start line in UI mode (atl-composer host.rs `start`): whether backpressure is on and its bound.
const BACKPRESSURE_LINE: &str = "atl-composer: ui: backpressure=";
/// Grid points lost at the block's two edges (the post in flight when it starts, the first latch after it): the design's `- 2`.
const SLACK: u64 = 2;
/// Warm-up (60 frames, 1 s at 60 Hz) + steady phase 4 s + block 2 s + tail 1 s, with room for a slow start.
const RESULT_WAIT: Duration = Duration::from_secs(25);
/// A dequeue that timed out (HWUI / libgui log `dequeueBuffer failed ... -110`, TIMED_OUT).
const TIMED_OUT: &str = "-110";

/// The frame period of the run's display (the gate's output locally, the target's display remotely), ms.
pub(super) fn period_ms(refresh_hz: f32) -> f64 {
    1_000.0 / f64::from(refresh_hz)
}

pub(super) fn result_line(app: &str) -> Result<&str, String> {
    app.lines()
        .find(|l| l.contains(RESULT))
        .ok_or_else(|| "no `ub result` line: the producer never got past the block".to_owned())
}

pub(super) fn number(line: &str, k: &str) -> Result<u64, String> {
    word(line, k)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| format!("no {k}= in {line}"))
}

/// The composer's backpressure bound, ms: 0 when backpressure is off (the gate's break).
pub(super) fn bound_ms(app: &str) -> Result<u64, String> {
    let line = app
        .lines()
        .find(|l| l.contains(BACKPRESSURE_LINE))
        .ok_or("no `atl-composer: ui: backpressure=` line")?;
    if word(line, "backpressure") == Some("false") {
        return Ok(0);
    }
    number(line, "bound_ms")
}

/// The frames the producer posted inside the block against the vsync grid, less the composer's bound.
fn posts_check(app: &str, refresh_hz: f32) -> Result<String, String> {
    let line = result_line(app)?;
    let (during, gap, block) = (
        number(line, "posts_during")?,
        number(line, "max_gap_ms")?,
        number(line, "block_ms")?,
    );
    let bound = bound_ms(app)?;
    let want = ((block.saturating_sub(bound) as f64 / period_ms(refresh_hz)).floor() as u64)
        .saturating_sub(SLACK);
    let summary = format!(
        "{during} frames posted during the {block} ms block (want >= {want}: bound {bound} ms), longest gap {gap} ms"
    );
    if during >= want {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// No dequeue timed out.
fn timeouts_check(app: &str) -> Result<String, String> {
    let n = app
        .lines()
        .filter(|l| l.contains("dequeueBuffer") && l.contains(TIMED_OUT))
        .count();
    if n == 0 {
        Ok("no dequeueBuffer -110".to_owned())
    } else {
        Err(format!("{n} dequeueBuffer -110 lines"))
    }
}

/// The sub-gates; `refresh_hz`: the run's display (the block's grid).
pub fn checks(app: &str, refresh_hz: f32) -> Vec<Sub> {
    vec![
        ("posts", posts_check(app, refresh_hz)),
        ("timeouts", timeouts_check(app)),
    ]
}

/// The probe: the window's configure is acked, then the fixture's result line (the phases are the app's own doing).
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, RESULT, RESULT_WAIT)
        .then_some(())
        .ok_or("no `ub result` line in time")?;
    Ok(Liveness::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: &str = "0.5 INFO atl-composer: ui: backpressure=true bound_ms=100\n";

    fn result(during: u64) -> String {
        format!(
            "9.0 I AtlWindow: ub result posts_during={during} max_gap_ms=20 block_ms=2000 steady_posts=240 steady_ms=4000 posted=500\n"
        )
    }

    /// At 60 Hz (period 16.666...67 ms in f64) with a 100 ms bound a 2000 ms block holds floor(113.99...) = 113 grid points, minus SLACK 2: 111 posts pass, 110 fail. Backpressure off (bound 0): floor(119.99...)
    /// = 119, minus 2: 117. No start line or no result line fails.
    /// A 120 Hz target display (period 8.333...34 ms in f64): 1900 ms holds floor(227.99...) = 227 grid points, minus 2: 225 pass, 224 fail.
    #[test]
    fn posts_against_the_grid_less_the_bound() {
        assert!(posts_check(&format!("{ON}{}", result(111)), 60.0).is_ok());
        assert!(posts_check(&format!("{ON}{}", result(110)), 60.0).is_err());
        let off = "0.5 INFO atl-composer: ui: backpressure=false bound_ms=100\n";
        assert!(posts_check(&format!("{off}{}", result(117)), 60.0).is_ok());
        assert!(posts_check(&format!("{off}{}", result(116)), 60.0).is_err());
        assert!(posts_check(&result(117), 60.0).is_err());
        assert!(posts_check(ON, 60.0).is_err());
        assert!(posts_check(&format!("{ON}{}", result(225)), 120.0).is_ok());
        assert!(posts_check(&format!("{ON}{}", result(224)), 120.0).is_err());
    }
}
