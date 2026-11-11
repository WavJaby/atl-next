// SPDX-License-Identifier: MIT
//! Stage `ui-block` of `xtask test window` (composer threading S2): a SurfaceView whose thread keeps posting frames (xtask-harness/testdata/ui-block, Main.java) while the app's UI thread, which is the window's
//! GTK thread, sleeps 2 s. The compositor latches, releases and sends vsync on its own clock thread (SurfaceFlinger `commit`), so the producer must keep getting its buffers back during the sleep.
//! Expected from the display, not from the code under test: one latch per vsync, so the frames posted inside the sleep are at least floor(block / period) - [`SLACK`], and no dequeue timed out (`-110`).
//! A compositor that latches only when the window's thread runs leaves the producer blocked in lockCanvas for the whole sleep: a handful of frames.

use super::{ACK_WAIT, Liveness, Sub, await_event, wait_for_line};
use crate::composer::word;
use crate::nested_wayland::GATE_OUTPUT;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};
use std::path::Path;
use std::time::Duration;

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/ui-block";
/// The fixture's result line (Main.java draw).
const RESULT: &str = "AtlWindow: ub result ";
/// Grid points lost at the block's two edges (the post in flight when it starts, the first latch after it): the design's `- 2`.
const SLACK: u64 = 2;
/// Warm-up (60 frames, 1 s at 60 Hz) + block 2 s + tail 1 s, with room for a slow start.
const RESULT_WAIT: Duration = Duration::from_secs(20);
/// A dequeue that timed out (HWUI / libgui log `dequeueBuffer failed ... -110`, TIMED_OUT).
const TIMED_OUT: &str = "-110";

/// The frame period of the gate's output, ms.
fn period_ms() -> f64 {
    1_000_000.0 / f64::from(GATE_OUTPUT.refresh_mhz)
}

/// The frames the producer posted inside the block against the vsync grid.
fn posts_check(app: &str) -> Result<String, String> {
    let line = app
        .lines()
        .find(|l| l.contains(RESULT))
        .ok_or("no `ub result` line: the producer never got past the block")?;
    let n = |k: &str| {
        word(line, k)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| format!("no {k}= in {line}"))
    };
    let (during, gap, block) = (n("posts_during")?, n("max_gap_ms")?, n("block_ms")?);
    let want = ((block as f64 / period_ms()).floor() as u64).saturating_sub(SLACK);
    let summary = format!(
        "{during} frames posted during the {block} ms block (want >= {want}), longest gap {gap} ms"
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

/// The sub-gates.
pub fn checks(app: &str) -> Vec<Sub> {
    vec![
        ("posts", posts_check(app)),
        ("timeouts", timeouts_check(app)),
    ]
}

/// The probe: the window's configure is acked, then the fixture's result line (the block is the app's own doing).
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

    /// At 60 Hz (period 16.667 ms) a 2000 ms block holds floor(119.99) = 119 grid points, minus SLACK 2: 117 posts pass, 116 fail; a missing line fails.
    #[test]
    fn posts_against_the_grid() {
        let line = |d| {
            format!(
                "1.0 I AtlWindow: ub result posts_during={d} max_gap_ms=20 block_ms=2000 posted=300"
            )
        };
        assert!(posts_check(&line(117)).is_ok());
        assert!(posts_check(&line(116)).is_err());
        assert!(posts_check("").is_err());
    }
}
