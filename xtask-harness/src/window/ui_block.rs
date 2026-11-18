// SPDX-License-Identifier: MIT
//! Stage `ui-block` of `xtask test window` (composer threading S2, composer backpressure): a SurfaceView whose thread keeps posting frames (xtask-harness/testdata/ui-block, Main.java) while the app's UI
//! thread, which is the window's GTK thread, first takes frames slower than the producer (slow phase: busy 40 ms / idle 10 ms), then sleeps 2 s (block).
//! - Slow phase (SurfaceFlinger does not latch while the last present is pending, SurfaceFlinger.cpp commit `wouldBackpressureHwc`): the producer is held to the window's rate. Expected from the fixture's
//!   own counts, not from the code under test: the window takes at most [`TAKES_PER_GAP`] frames per idle gap, each take lets one latch through, and a latch takes one buffer per layer
//!   (eEnableBackpressure), so the posts inside the phase are at most TAKES_PER_GAP x (cycles + 1) + [`IN_FLIGHT`]; the composer replaces no frame unseen (its `scenes_dropped` stays put). A composer
//!   that latches every tick lets a 60 Hz producer post one frame per vsync (~240 in 4 s against ~150 allowed) and replaces most of them (`--break-no-backpressure`).
//! - Block: the window takes nothing, so the composer holds until its bound (`bound_ms` on its start line), then latches every vsync as a display that presents: the frames posted inside the sleep are at
//!   least floor((block - bound) / period) - [`SLACK`], and no dequeue timed out (`-110`). A compositor that latches only when the window's thread runs leaves the producer blocked for the whole sleep.

use super::{ACK_WAIT, Liveness, Sub, await_event, line_stamp, wait_for_line};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};
use std::path::Path;
use std::time::Duration;

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/ui-block";
/// The fixture's result line (Main.java draw).
const RESULT: &str = "AtlWindow: ub result ";
/// The fixture's slow-phase markers (Main.java slow, busy).
const SLOW_START: &str = "AtlWindow: ub slow start";
const SLOW_END: &str = "AtlWindow: ub slow end";
/// The composer's start line in UI mode (atl-composer host.rs `start`): whether backpressure is on and its bound.
const BACKPRESSURE_LINE: &str = "atl-composer: ui: backpressure=";
/// The composer's counters line (host.rs `counters_line`).
const COUNTERS: &str = "composer: counters ";
/// Grid points lost at the block's two edges (the post in flight when it starts, the first latch after it): the design's `- 2`.
const SLACK: u64 = 2;
/// Posts that can go out in the slow phase with no latch for them: the SurfaceView's BufferQueue slots (3, triple buffering) can all be queued and waiting at once.
const IN_FLIGHT: u64 = 3;
/// Frame-clock ticks (takes) one idle gap shorter than two periods can hold: the one for the frame callback that arrived during the busy part, at the gap's start, and the next output frame's.
/// One per gap was measured too tight: 90 posts against 74 cycles (the Looper's 10 ms delay runs late under load, and a gap past one period fits a second callback).
const TAKES_PER_GAP: u64 = 2;
/// The slow phase's idle part, ms (Main.java slow: busy 40 / idle 10): the gap [`takes_per_gap`] counts grid points in.
const IDLE_GAP_MS: f64 = 10.0;
/// Frames the composer may replace in the slow phase: none while the window takes within the bound; two allow for a take that host load delayed past it.
const REPLACED_SLACK: u64 = 2;
/// Warm-up (60 frames, 1 s at 60 Hz) + slow phase 4 s + block 2 s + tail 1 s, with room for a slow start.
const RESULT_WAIT: Duration = Duration::from_secs(25);
/// A dequeue that timed out (HWUI / libgui log `dequeueBuffer failed ... -110`, TIMED_OUT).
const TIMED_OUT: &str = "-110";

/// The frame period of the run's display (the gate's output locally, the target's display remotely), ms.
fn period_ms(refresh_hz: f32) -> f64 {
    1_000.0 / f64::from(refresh_hz)
}

fn result_line(app: &str) -> Result<&str, String> {
    app.lines()
        .find(|l| l.contains(RESULT))
        .ok_or_else(|| "no `ub result` line: the producer never got past the block".to_owned())
}

fn number(line: &str, k: &str) -> Result<u64, String> {
    word(line, k)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| format!("no {k}= in {line}"))
}

/// The composer's backpressure bound, ms: 0 when backpressure is off (the gate's break).
fn bound_ms(app: &str) -> Result<u64, String> {
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

/// Frame-clock ticks (takes) one idle gap can hold on a display of period `period_ms`: the callback that arrived during the busy part, at the gap's start, plus the output frames inside the gap
/// (ceil([`IDLE_GAP_MS`] / period)). 60 Hz: 1 + 1 = 2, the measured [`TAKES_PER_GAP`]; 120 Hz (the laptop, 2026-11-18): 1 + 2 = 3 (173 posts in 80 cycles measured, over 2 x 81 + 3).
fn takes_per_gap(period_ms: f64) -> u64 {
    (TAKES_PER_GAP - 1) + (IDLE_GAP_MS / period_ms).ceil() as u64
}

/// The producer is held to the window's takes in the slow phase.
fn held_check(app: &str, refresh_hz: f32) -> Result<String, String> {
    let line = result_line(app)?;
    let (posts, cycles, ms) = (
        number(line, "slow_posts")?,
        number(line, "slow_cycles")?,
        number(line, "slow_ms")?,
    );
    let takes = takes_per_gap(period_ms(refresh_hz));
    let most = takes * (cycles + 1) + IN_FLIGHT;
    let summary = format!(
        "{posts} frames posted in the {ms} ms slow phase, {cycles} busy cycles (want <= {most}: {takes} takes per idle gap at {refresh_hz} Hz)"
    );
    if posts <= most {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The composer's counters between the first and the last of its lines inside the slow phase: no frame replaced unseen.
fn replaced_check(app: &str) -> Result<String, String> {
    let stamp = |marker: &str| {
        app.lines()
            .find(|l| l.contains(marker))
            .and_then(line_stamp)
            .ok_or_else(|| format!("no stamped `{marker}` line"))
    };
    let (start, end) = (stamp(SLOW_START)?, stamp(SLOW_END)?);
    let inside: Vec<&str> = app
        .lines()
        .filter(|l| l.contains(COUNTERS))
        .filter(|l| line_stamp(l).is_some_and(|t| t >= start && t <= end))
        .collect();
    let (Some(first), Some(last)) = (inside.first(), inside.last()) else {
        return Err("no counters line inside the slow phase".to_owned());
    };
    if inside.len() < 2 {
        return Err(format!(
            "one counters line inside the {} ms slow phase: no interval to judge",
            end - start
        ));
    }
    let delta =
        |k: &str| -> Result<u64, String> { Ok(number(last, k)?.saturating_sub(number(first, k)?)) };
    let (dropped, taken, latches, holds) = (
        delta("scenes_dropped")?,
        delta("scenes_taken")?,
        delta("latches")?,
        delta("backpressure_holds")?,
    );
    let summary = format!(
        "slow phase, {} counters lines: scenes_dropped +{dropped} (want <= {REPLACED_SLACK}), scenes_taken +{taken}, latches +{latches}, backpressure_holds +{holds}",
        inside.len()
    );
    if dropped <= REPLACED_SLACK {
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
        ("held", held_check(app, refresh_hz)),
        ("replaced", replaced_check(app)),
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

    fn result(during: u64, slow_posts: u64) -> String {
        format!(
            "9.0 I AtlWindow: ub result posts_during={during} max_gap_ms=20 block_ms=2000 slow_posts={slow_posts} slow_cycles=80 slow_ms=4000 posted=500\n"
        )
    }

    /// At 60 Hz (period 16.666...67 ms in f64) with a 100 ms bound a 2000 ms block holds floor(113.99...) = 113 grid points, minus SLACK 2: 111 posts pass, 110 fail. Backpressure off (bound 0): floor(119.99...)
    /// = 119, minus 2: 117. No start line or no result line fails.
    /// A 120 Hz target display (period 8.333...34 ms in f64): 1900 ms holds floor(227.99...) = 227 grid points, minus 2: 225 pass, 224 fail.
    #[test]
    fn posts_against_the_grid_less_the_bound() {
        assert!(posts_check(&format!("{ON}{}", result(111, 0)), 60.0).is_ok());
        assert!(posts_check(&format!("{ON}{}", result(110, 0)), 60.0).is_err());
        let off = "0.5 INFO atl-composer: ui: backpressure=false bound_ms=100\n";
        assert!(posts_check(&format!("{off}{}", result(117, 0)), 60.0).is_ok());
        assert!(posts_check(&format!("{off}{}", result(116, 0)), 60.0).is_err());
        assert!(posts_check(&result(117, 0), 60.0).is_err());
        assert!(posts_check(ON, 60.0).is_err());
        assert!(posts_check(&format!("{ON}{}", result(225, 0)), 120.0).is_ok());
        assert!(posts_check(&format!("{ON}{}", result(224, 0)), 120.0).is_err());
    }

    /// 80 busy cycles: at most 2 x 81 = 162 takes, plus 3 queued slots = 165 posts pass, 166 fail.
    #[test]
    fn held_to_the_takes() {
        assert!(held_check(&result(0, 165), 60.0).is_ok());
        assert!(held_check(&result(0, 166), 60.0).is_err());
        // 120 Hz: the 10 ms gap spans two grid points, 3 takes: 3 x 81 + 3 = 246
        assert!(held_check(&result(0, 246), 120.0).is_ok());
        assert!(held_check(&result(0, 247), 120.0).is_err());
    }

    /// Two counters lines inside the phase: their scenes_dropped delta is judged; lines outside the phase do not count, and one line alone is no interval.
    #[test]
    fn replaced_between_lines_inside_the_phase() {
        let counters = |t: &str, dropped: u64| {
            format!(
                "{t}  INFO composer: counters ticks=1 latches=1 backpressure_holds=0 scenes_taken=0 scenes_dropped={dropped}\n"
            )
        };
        let log = |d2: u64| {
            format!(
                "{}1.000 I AtlWindow: ub slow start\n{}{}5.000 I AtlWindow: ub slow end\n{}",
                counters("0.900", 0),
                counters("1.500", 10),
                counters("4.500", 10 + d2),
                counters("5.500", 500),
            )
        };
        assert!(replaced_check(&log(2)).is_ok());
        assert!(replaced_check(&log(3)).is_err());
        let one = format!(
            "1.000 I AtlWindow: ub slow start\n{}5.000 I AtlWindow: ub slow end\n",
            counters("2.000", 0)
        );
        assert!(replaced_check(&one).is_err());
    }
}
