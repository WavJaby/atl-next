// SPDX-License-Identifier: MIT
//! The compositor's frame clock, one thread in UI and headless mode alike (SurfaceFlinger's main thread: `commit` latches each vsync whatever the display's toolkit is doing).
//! Ticks sit on a grid of one refresh interval from an origin on CLOCK_MONOTONIC, the clock Choreographer reads vsync times on; after an idle stretch the clock resumes on the next grid point.
//! In UI mode the grid is the window's presented frames' (their presentation time and period, [`tick_grid`]), so a tick lands where the display's vsync is; the monitor's rate from the start serves until the first frame.

use std::sync::{Condvar, Mutex};

use atl_platform_ui::PresentTiming;

/// What wakes the parked clock: a generation counter under its own mutex, bumped by every waker. The clock reads the generation before it checks its conditions (under other locks) and parks only while
/// it is unchanged, so a wake between check and park is never lost, and a waker (the toolkit thread) takes no lock but this one, held for an increment.
#[derive(Debug, Default)]
pub struct Wake {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl Wake {
    /// The generation now: pass it to [`Wake::park`] after checking the conditions.
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Something may have changed.
    pub fn kick(&self) {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.changed.notify_all();
    }

    /// Parks while the generation is still `seen`.
    pub fn park(&self, seen: u64) {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        while *g == seen {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// [`Wake::park`] for at most `limit`.
    pub fn park_for(&self, seen: u64, limit: std::time::Duration) {
        let g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(g, limit, |g| *g == seen)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// The interval of `refresh_hz`, in ns, rounded to the nearest.
pub fn interval_ns(refresh_hz: f32) -> i64 {
    (1e9 / f64::from(refresh_hz)).round() as i64
}

/// The first grid point `origin + k * interval` (k >= 0) at or after `now`.
pub fn next_tick(origin_ns: i64, interval_ns: i64, now_ns: i64) -> i64 {
    if now_ns <= origin_ns {
        return origin_ns;
    }
    let behind = now_ns - origin_ns;
    origin_ns + (behind + interval_ns - 1) / interval_ns * interval_ns
}

/// SurfaceFlinger's `kSnapToSameVsyncWithin` (VsyncSchedule.cpp `createDispatch`): two vsync times this close are the same vsync.
pub const SNAP_TO_SAME_VSYNC_NS: i64 = 3_000_000;

/// The next tick at or after `now` on the grid, never closer to the last one dispatched than a period allows: AOSP `VSyncDispatchTimerQueueEntry::adjustVsyncIfNeeded`'s `nextVsyncTooClose`
/// (`next - last + snap <= period` -> the first grid point a period after `last`). That test also covers AOSP's `alreadyDispatchedForVsync` (within the snap of `last`): AOSP retries from `last + snap` on its
/// stable model, which lands a period on; our grid can move its phase between ticks, so the retry is the too-close one, which cannot land closer. Without it a grid that moves onto a new presented sample
/// dispatches two vsyncs a few ms apart, a timeline no display has.
pub fn next_dispatch(origin_ns: i64, interval_ns: i64, now_ns: i64, last_ns: Option<i64>) -> i64 {
    let at = next_tick(origin_ns, interval_ns, now_ns);
    match last_ns {
        Some(last) if at - last + SNAP_TO_SAME_VSYNC_NS <= interval_ns => {
            next_tick(origin_ns, interval_ns, last + interval_ns)
        }
        _ => at,
    }
}

/// The grid `(origin_ns, interval_ns)` of the ticks: the last presented frame's time and period, else the start time on the monitor's rate (`refresh_hz`).
pub fn tick_grid(presented: Option<PresentTiming>, start_ns: i64, refresh_hz: f32) -> (i64, i64) {
    match presented {
        Some(p) => (p.time_ns, p.refresh_ns),
        None => (start_ns, interval_ns(refresh_hz)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: i64 = 16_666_667; // 1e9 / 60, rounded up
    const START: i64 = 1_000_000_000;
    /// The first frame is shown 5 ms after the start: a phase the start time's grid does not have.
    const OFFSET: i64 = 5_000_000;
    /// A frame's sample reaches the clock 2 ms after the frame is shown.
    const LATENCY: i64 = 2_000_000;
    /// Per-frame jitter of the shown time, cycling 0 / 0.2 / 0.4 ms.
    const JITTER: [i64; 3] = [0, 200_000, 400_000];
    /// The parent's gate: a tick within 1 ms of the phase of the last frame shown.
    const TOLERANCE: i64 = 1_000_000;
    const TICKS: u32 = 100;
    const MONITOR_HZ: f32 = 60.0;

    /// Frame `k` is shown at `OFFSET + k x PERIOD + JITTER[k]` after the start.
    fn shown(k: i64) -> i64 {
        START + OFFSET + k * PERIOD + JITTER[k as usize % JITTER.len()]
    }

    /// Over 100 ticks from one period after the first sample arrives, each tick's phase is within `TOLERANCE` of the last frame shown before it. The fake feed delivers each sample `LATENCY` after its
    /// frame is shown, and the clock computes each grid from the samples delivered at that moment. Feed off: the grid stays on the start time, 5 ms off the frames, and the worst phase error exceeds the tolerance.
    fn worst_phase_error(feed: bool) -> i64 {
        let window_start = shown(0) + LATENCY + PERIOD;
        let (mut now, mut delivered, mut next_frame, mut counted, mut worst) =
            (START, None, 0i64, 0u32, 0i64);
        while counted < TICKS {
            if feed {
                while shown(next_frame) + LATENCY <= now {
                    delivered = Some(PresentTiming {
                        time_ns: shown(next_frame),
                        refresh_ns: PERIOD,
                    });
                    next_frame += 1;
                }
            }
            let (origin, interval) = tick_grid(delivered, START, MONITOR_HZ);
            let at = next_tick(origin, interval, now);
            if at >= window_start {
                let last = (0..)
                    .map(shown)
                    .take_while(|&t| t <= at)
                    .last()
                    .expect("a frame is shown before the window");
                let phase = (at - last).rem_euclid(PERIOD);
                worst = worst.max(phase.min(PERIOD - phase));
                counted += 1;
            }
            now = at + 1;
        }
        worst
    }

    /// The fake presentation feed (`PresentTiming` samples, as the window reports them) puts the ticks on the presented phase within 1 ms; with no feed the same ticks are 5 ms off and the check fails.
    #[test]
    fn ticks_follow_presentation_phase() {
        assert!(worst_phase_error(true) <= TOLERANCE);
        assert!(worst_phase_error(false) > TOLERANCE);
    }

    /// 1e9 / 60 = 16 666 666.67 ns, which rounds up; 1e9 / 50 is exact.
    #[test]
    fn intervals() {
        assert_eq!(interval_ns(60.0), 16_666_667);
        assert_eq!(interval_ns(50.0), 20_000_000);
    }

    /// On the grid 100, 110, 120 ...: a time on a point stays, between two points goes up, before the origin goes to the origin.
    #[test]
    fn next_grid_point() {
        assert_eq!(next_tick(100, 10, 100), 100);
        assert_eq!(next_tick(100, 10, 101), 110);
        assert_eq!(next_tick(100, 10, 110), 110);
        assert_eq!(next_tick(100, 10, 95), 100);
        assert_eq!(next_tick(100, 10, 1_000_001), 1_000_010);
    }

    /// On a 16.67 ms grid: last tick at 0, the grid moved its phase 5 ms back (origin 11.67 ms). The plain next point after now = 1 ms is
    /// 11.67 ms, 11.67 ms after the last (< period - 3 ms = 13.67 ms): too close, so the first point at or after 16.67 ms is taken, 28.33 ms. A point 14 ms after (> 13.67 ms) stays; no last tick, no change.
    #[test]
    fn dispatch_not_too_close() {
        let p = 16_666_667;
        let origin = p - 5_000_000;
        assert_eq!(next_dispatch(origin, p, 1_000_000, Some(0)), origin + p);
        assert_eq!(next_dispatch(origin, p, 1_000_000, None), origin);
        let origin14 = 14_000_000;
        assert_eq!(next_dispatch(origin14, p, 1_000_000, Some(0)), origin14);
        // same vsync again (now == last on an unchanged grid): one period on
        assert_eq!(next_dispatch(0, p, 0, Some(0)), p);
    }

    /// A kick between reading the generation and parking is not lost: park returns at once.
    #[test]
    fn kick_before_park_is_kept() {
        let w = Wake::default();
        let seen = w.generation();
        w.kick();
        w.park(seen);
        assert_eq!(w.generation(), seen + 1);
    }
}
