// SPDX-License-Identifier: MIT
//! The compositor's frame clock, one thread in UI and headless mode alike (SurfaceFlinger's main thread: `commit` latches each vsync whatever the display's toolkit is doing).
//! Ticks sit on a grid of one refresh interval from an origin on CLOCK_MONOTONIC, the clock Choreographer reads vsync times on; after an idle stretch the clock resumes on the next grid point.

use std::sync::{Condvar, Mutex};

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

#[cfg(test)]
mod tests {
    use super::*;

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
