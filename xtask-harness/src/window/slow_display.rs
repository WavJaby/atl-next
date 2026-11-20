// SPDX-License-Identifier: MIT
//! Stage `slow-display` of `xtask test window` (display-thread S4): the ui-block fixture's SurfaceView producer against a display slower than it. The harness's Wayland proxy answers the window's frame
//! callbacks no faster than one per [`THROTTLE_K`] output periods (wayland_proxy.rs `Proxy::throttle_frames`), so the window's frame clock -- and with it the composer's takes -- runs at 1/K of the output
//! rate while the app's UI thread is idle (the fixture's steady phase). SurfaceFlinger does not latch while the last present is pending (SurfaceFlinger.cpp commit `wouldBackpressureHwc`): the producer
//! must be held to the display's rate and the composer must replace no frame unseen. A composer that latches every tick (`--break-no-backpressure`) lets the producer post at the output rate.
//! Judged against the display, not the app's UI thread: holds whichever thread the window runs on.
//! Host too slow: when the window's own measured render inside the steady phase exceeds the composer's hold bound, a held / replaced failure is `undecided` (still red, rerun); never decided from the
//! composer's counters, which broken backpressure moves the same way ([`gpu_starved`]).

use super::ui_block::{FIXTURE as UI_BLOCK_FIXTURE, bound_ms, number, period_ms, result_line};
use super::{ACK_WAIT, Liveness, Sub, await_event, line_stamp, wait_for_line};
use crate::composer::word;
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};
use std::path::Path;
use std::time::Duration;

/// The fixture: ui-block's (its steady phase is this stage's subject).
pub const FIXTURE: &str = UI_BLOCK_FIXTURE;
/// Output periods per answered frame callback. 4 at 60 Hz = 66.7 ms (15 Hz): a quarter of the producer's rate, so held (~64 posts in 4 s) and unheld (~240) are far apart, and under the composer's
/// 100 ms bound, so the display, not the bound, sets the rate ([`bound_check`]).
pub const THROTTLE_K: u32 = 4;
/// The fixture's steady-phase markers (Main.java steady).
const STEADY_START: &str = "AtlWindow: ub steady start";
const STEADY_END: &str = "AtlWindow: ub steady end";
/// The fixture's result line (Main.java draw).
const RESULT: &str = "AtlWindow: ub result ";
/// The composer's counters line (host.rs `counters_line`).
const COUNTERS: &str = "composer: counters ";
/// Posts that can go out with no latch for them: the SurfaceView's BufferQueue slots (3, triple buffering) can all be queued and waiting at once.
const IN_FLIGHT: u64 = 3;
/// Takes at the phase's edges beyond floor(steady / T): the frame callback already released when the phase starts.
const EDGE_TAKES: u64 = 1;
/// Frames the composer may replace in the steady phase: none while the window takes within the bound; two allow for a take that host load delayed past it.
const REPLACED_SLACK: u64 = 2;
/// Warm-up at the throttled rate (60 frames at 15 Hz, 4 s) + steady 4 s + block 2 s + tail 1 s = 11 s, with room for a slow start.
const RESULT_WAIT: Duration = Duration::from_secs(30);
/// The watch after the marker (window.rs `Stage::window_ms`): the 11 s of [`RESULT_WAIT`]'s phases plus 5 s for a loaded host; the gtk watch (10 s) ends before the result (measured: stopped in the tail).
pub const WINDOW_MS: u64 = 16_000;

/// The throttled frame period on a display of `refresh_hz`, ms.
pub fn throttle_ms(refresh_hz: f32) -> f64 {
    f64::from(THROTTLE_K) * period_ms(refresh_hz)
}

/// The phase's length and posts from the fixture's result line.
fn steady(app: &str) -> Result<(u64, u64), String> {
    let line = result_line(app)?;
    Ok((number(line, "steady_ms")?, number(line, "steady_posts")?))
}

/// The producer is held to the slowed display: one take, one latch, one buffer per released frame callback.
fn held_check(app: &str, refresh_hz: f32) -> Result<String, String> {
    let (ms, posts) = steady(app)?;
    let t = throttle_ms(refresh_hz);
    let takes = (ms as f64 / t).floor() as u64 + EDGE_TAKES;
    let most = takes + IN_FLIGHT;
    let summary = format!(
        "{posts} frames posted in the {ms} ms steady phase (want <= {most}: {takes} takes at one per {t:.1} ms + {IN_FLIGHT} queued slots)"
    );
    if posts <= most {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The composer's counters lines inside the steady phase, first and last, and how many: the interval the counter checks judge.
fn phase_counters(app: &str) -> Result<(&str, &str, usize), String> {
    let stamp = |marker: &str| {
        app.lines()
            .find(|l| l.contains(marker))
            .and_then(line_stamp)
            .ok_or_else(|| format!("no stamped `{marker}` line"))
    };
    let (start, end) = (stamp(STEADY_START)?, stamp(STEADY_END)?);
    let inside: Vec<&str> = app
        .lines()
        .filter(|l| l.contains(COUNTERS))
        .filter(|l| line_stamp(l).is_some_and(|t| t >= start && t <= end))
        .collect();
    match inside.as_slice() {
        [] => Err("no counters line inside the steady phase".to_owned()),
        [_] => Err(format!(
            "one counters line inside the {} ms steady phase: no interval to judge",
            end - start
        )),
        [first, .., last] => Ok((first, last, inside.len())),
    }
}

/// Counter `k`'s growth over the steady phase.
fn phase_delta(app: &str, k: &str) -> Result<u64, String> {
    let (first, last, _) = phase_counters(app)?;
    Ok(number(last, k)?.saturating_sub(number(first, k)?))
}

/// The platform's per-second paint line (atl-platform-ui-linux paint_stats.rs): `render_us=mean/max`, the GSK render + swap, measured by the window itself.
const PAINT: &str = "atl-platform-ui: paint ";

/// The longest window render (us) among the paint lines stamped inside the steady phase; None without stamped markers or a paint line between them.
fn steady_render_max_us(app: &str) -> Option<u64> {
    let stamp = |marker: &str| {
        app.lines()
            .find(|l| l.contains(marker))
            .and_then(line_stamp)
    };
    let (start, end) = (stamp(STEADY_START)?, stamp(STEADY_END)?);
    app.lines()
        .filter(|l| l.contains(PAINT))
        .filter(|l| line_stamp(l).is_some_and(|t| t >= start && t <= end))
        .filter_map(|l| word(l, "render_us")?.split_once('/')?.1.parse().ok())
        .max()
}

/// Host GPU starvation: a window render inside the steady phase longer than the composer's hold bound (a loaded software-GL host: 250-500 ms paints measured at load 10-13). Every hold then expires
/// before the window takes, so the bound, not the throttled display, sets the rate and held / replaced read like `--break-no-backpressure` whatever the code does. Keyed on the window's own measured
/// render time, never on the composer's counters: an early-expiring hold (broken backpressure) grows `latches_unpainted` too but cannot lengthen the window's render, so it stays judged (R6-8).
/// Backpressure off (bound 0, the red control) is never starved. Some(text) naming both times.
pub fn gpu_starved(app: &str) -> Option<String> {
    let bound = bound_ms(app).ok().filter(|b| *b > 0)?;
    let max = steady_render_max_us(app)?;
    (max > bound * 1000).then(|| {
        format!(
            "host GPU starved: window render max {} ms > hold bound {bound} ms",
            max / 1000
        )
    })
}

/// Sub-gate paint: the steady phase's longest window render, shown beside the verdict (judges nothing on its own).
fn paint_check(app: &str) -> Result<String, String> {
    Ok(match steady_render_max_us(app) {
        Some(us) => format!("steady phase window render max {} ms", us / 1000),
        None => "no paint line inside the steady phase".to_owned(),
    })
}

/// No frame replaced unseen in the steady phase.
fn replaced_check(app: &str) -> Result<String, String> {
    let (_, _, lines) = phase_counters(app)?;
    let delta = |k: &str| phase_delta(app, k);
    let (dropped, taken, latches, holds) = (
        delta("scenes_dropped")?,
        delta("scenes_taken")?,
        delta("latches")?,
        delta("backpressure_holds")?,
    );
    let summary = format!(
        "steady phase, {lines} counters lines: scenes_dropped +{dropped} (want <= {REPLACED_SLACK}), scenes_taken +{taken}, latches +{latches}, backpressure_holds +{holds}"
    );
    if dropped <= REPLACED_SLACK {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The probe responds: the proxy delayed at least one frame callback, so the display was slower than the window. How many it released depends on how fast the window paints, so only its count is shown.
fn throttled_check((held, released): (u64, u64)) -> Result<String, String> {
    let summary =
        format!("proxy held {held} and released {released} frame callbacks (want held > 0)");
    if held > 0 { Ok(summary) } else { Err(summary) }
}

/// The throttled period is under the composer's bound, so the display sets the rate; backpressure off (bound 0) has no bound to stay under.
fn bound_check(app: &str, refresh_hz: f32) -> Result<String, String> {
    let bound = bound_ms(app)?;
    let t = throttle_ms(refresh_hz);
    if bound == 0 {
        return Ok("backpressure off: no bound".to_owned());
    }
    let summary = format!("throttled period {t:.1} ms against the composer's bound {bound} ms");
    if t < bound as f64 {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The sub-gates; `refresh_hz`: the run's display; `throttle`: the proxy's counts at the end of the drive. Every sub-gate is always judged. A held / replaced failure on a GPU-starved host
/// ([`gpu_starved`]) is `undecided`: still a failure, so `check` and `land` need a rerun that decides, and a starved host never passes the stage unjudged.
pub fn checks(app: &str, refresh_hz: f32, throttle: (u64, u64)) -> Vec<Sub> {
    let starved = gpu_starved(app);
    let judged = |r: Result<String, String>| match (&starved, r) {
        (Some(why), Err(e)) => Err(format!("undecided: {why}: {e}")),
        (_, r) => r,
    };
    vec![
        ("bound", bound_check(app, refresh_hz)),
        ("throttled", throttled_check(throttle)),
        ("held", judged(held_check(app, refresh_hz))),
        ("replaced", judged(replaced_check(app))),
        ("paint", paint_check(app)),
    ]
}

/// The probe: once the window acks its configure, the proxy slows its frame callbacks; then the fixture's result line. The throttle's counts reach the checks through the live half (window.rs `live`).
pub fn drive(proxy: Proxy, app_log: &Path, refresh_hz: f32) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    proxy.throttle_frames(Duration::from_secs_f64(throttle_ms(refresh_hz) / 1_000.0));
    wait_for_line(app_log, RESULT, RESULT_WAIT)
        .then_some(())
        .ok_or("no `ub result` line in time")?;
    Ok(Liveness::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(ms: u64, posts: u64) -> String {
        format!(
            "9.0 I AtlWindow: ub result posts_during=100 max_gap_ms=20 block_ms=2000 steady_posts={posts} steady_ms={ms} posted=500\n"
        )
    }

    /// 60 Hz, K 4: T = 66.666...7 ms; 4000 ms holds floor(59.99...) = 59 periods + 1 edge take + 3 slots = 63 pass, 64 fail. 4001 ms: floor(60.01) = 60 -> 64 pass.
    #[test]
    fn held_to_the_throttled_display() {
        assert!(held_check(&result(4000, 63), 60.0).is_ok());
        assert!(held_check(&result(4000, 64), 60.0).is_err());
        assert!(held_check(&result(4001, 64), 60.0).is_ok());
        assert!(held_check("", 60.0).is_err());
    }

    /// At least one callback held; the released count is shown, not judged.
    #[test]
    fn throttle_must_respond() {
        assert!(throttled_check((1, 0)).is_ok());
        assert!(throttled_check((0, 60)).is_err());
    }

    /// Starvation keys on the window's measured render, not on the composer's counters: unpainted latches with fast paints (an early-expiring hold, R6-8) stay judged and red; a render over the
    /// 100 ms bound inside the phase makes a failure `undecided` (still Err) and leaves a pass a pass; 100 ms exactly, a long render outside the phase, or backpressure off (the red control) decide as usual.
    #[test]
    fn starvation_is_the_window_render_not_the_counters() {
        let counters = |t: &str, unpainted: u64, dropped: u64| {
            format!(
                "{t}  INFO composer: counters ticks=1 latches=1 backpressure_holds=0 scenes_taken=0 latches_unpainted={unpainted} scenes_dropped={dropped}\n"
            )
        };
        let paint = |t: &str, max_us: u64| {
            format!(
                "{t}  INFO atl-platform-ui: paint ms=1000 render_us=5000/{max_us} after_us=1/1\n"
            )
        };
        let log = |on: &str, inside: u64, outside: u64, dropped: u64| {
            format!(
                "{on}1.000 I AtlWindow: ub steady start\n{}{}{}5.000 I AtlWindow: ub steady end\n{}",
                counters("1.500", 7, 0),
                paint("2.000", inside),
                counters("4.500", 47, dropped),
                paint("6.000", outside),
            )
        };
        let on = "0.5 INFO atl-composer: ui: backpressure=true bound_ms=100\n";
        let off = "0.5 INFO atl-composer: ui: backpressure=false bound_ms=100\n";
        let replaced = |l: &str| checks(l, 60.0, (1, 1)).remove(3).1;
        // latches_unpainted +40 with 20 ms paints: judged, red
        let broken = replaced(&log(on, 20_000, 0, 135)).expect_err("135 replaced");
        assert!(!broken.starts_with("undecided"), "{broken}");
        assert!(gpu_starved(&log(on, 20_000, 0, 135)).is_none());
        let starved = replaced(&log(on, 100_001, 0, 135)).expect_err("still a failure");
        assert!(
            starved.starts_with(
                "undecided: host GPU starved: window render max 100 ms > hold bound 100 ms"
            ),
            "{starved}"
        );
        assert!(replaced(&log(on, 300_000, 0, 0)).is_ok());
        let at_bound = replaced(&log(on, 100_000, 0, 135)).expect_err("fail");
        assert!(!at_bound.starts_with("undecided"), "{at_bound}");
        let outside = replaced(&log(on, 0, 300_000, 135)).expect_err("fail");
        assert!(!outside.starts_with("undecided"), "{outside}");
        let control = replaced(&log(off, 300_000, 0, 135)).expect_err("fail");
        assert!(!control.starts_with("undecided"), "{control}");
        assert_eq!(checks(&log(on, 300_000, 0, 0), 60.0, (1, 1)).len(), 5);
    }

    /// 60 Hz K 4 = 66.7 ms < 100 passes; 30 Hz K 4 = 133.3 ms does not; backpressure off has no bound.
    #[test]
    fn throttle_under_the_bound() {
        let on = "0.5 INFO atl-composer: ui: backpressure=true bound_ms=100\n";
        let off = "0.5 INFO atl-composer: ui: backpressure=false bound_ms=100\n";
        assert!(bound_check(on, 60.0).is_ok());
        assert!(bound_check(on, 30.0).is_err());
        assert!(bound_check(off, 30.0).is_ok());
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
                "{}1.000 I AtlWindow: ub steady start\n{}{}5.000 I AtlWindow: ub steady end\n{}",
                counters("0.900", 0),
                counters("1.500", 10),
                counters("4.500", 10 + d2),
                counters("5.500", 500),
            )
        };
        assert!(replaced_check(&log(2)).is_ok());
        assert!(replaced_check(&log(3)).is_err());
        let one = format!(
            "1.000 I AtlWindow: ub steady start\n{}5.000 I AtlWindow: ub steady end\n",
            counters("2.000", 0)
        );
        assert!(replaced_check(&one).is_err());
    }
}
