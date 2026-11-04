// SPDX-License-Identifier: MIT
//! The timer frame clock the compositor ticks on until the platform layer supplies the desktop's (M3 slice 6: the GTK frame clock drives `EventThread::on_frame_tick` and `Compositor::on_frame` instead).
//! Ticks sit on a grid of one refresh interval from an origin on CLOCK_MONOTONIC, the clock Choreographer reads vsync times on; after an idle stretch the clock resumes on the next grid point.

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
}
