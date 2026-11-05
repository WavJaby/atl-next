// SPDX-License-Identifier: MIT
//! Frame clock pacing of requested ticks. The clock is on demand (no request, no tick: an app's vsync on Android comes only after it asks, EventThread::requestNextVsync), but a requested tick lands on the display's vsync grid
//! whether or not the last frame drew anything: SurfaceFlinger's VSyncDispatch runs off the hardware vsync model, not off client commits, and Choreographer's frame-interval estimate assumes that rate.
//! GDK's frame clock is paced by the compositor's frame callback only after a commit; with nothing drawn it falls back to its own timer at its refresh estimate (GTK default 16.667 ms), which on a 120 Hz monitor gives ~60 ticks/s.
//! So: after a tick that presented, GDK paces the next one (frame callback); after one that did not, a timer ticks on the grid `last tick + k x monitor interval`. Times are CLOCK_MONOTONIC microseconds, GDK's frame-time unit.

/// A monitor's refresh interval in µs from GDK's milli-Hz; `None` for 0 (unknown) or negative.
pub fn interval_us(refresh_millihz: i32) -> Option<i64> {
    (refresh_millihz > 0).then(|| (1e9 / f64::from(refresh_millihz)).round() as i64)
}

/// The first grid point `last + k x interval` (k >= 1) at or after `now`: the next idle tick. A tick late by more than an interval skips the missed points instead of firing them in a burst.
pub fn next_grid_us(last_us: i64, interval_us: i64, now_us: i64) -> i64 {
    let interval = interval_us.max(1);
    let k = if now_us <= last_us {
        1
    } else {
        ((now_us - last_us) + interval - 1) / interval
    };
    last_us + k.max(1) * interval
}

/// Ticks per kind over a window of time: the line a laptop run reads for ticks/s idle vs animating.
#[derive(Debug, Default)]
pub struct TickStats {
    since_us: Option<i64>,
    /// Ticks the compositor's frame callback paced (the previous tick presented).
    pub paced: u32,
    /// Ticks the idle timer made (the previous tick presented nothing).
    pub idle: u32,
}

/// How often the stats line is written while the clock ticks.
pub const STATS_WINDOW_US: i64 = 2_000_000;

impl TickStats {
    /// Counts one tick at `now_us`; once a window has passed, returns `(paced, idle, elapsed_us)` and starts the next.
    pub fn count(&mut self, idle: bool, now_us: i64) -> Option<(u32, u32, i64)> {
        let since = *self.since_us.get_or_insert(now_us);
        if idle {
            self.idle += 1;
        } else {
            self.paced += 1;
        }
        let elapsed = now_us - since;
        (elapsed >= STATS_WINDOW_US).then(|| {
            let out = (self.paced, self.idle, elapsed);
            *self = TickStats {
                since_us: Some(now_us),
                ..TickStats::default()
            };
            out
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 Hz is 16 667 µs, 120 Hz 8 333 µs (1e9 / mHz, rounded); unknown is none.
    #[test]
    fn intervals() {
        assert_eq!(interval_us(60_000), Some(16_667));
        assert_eq!(interval_us(120_000), Some(8_333));
        assert_eq!(interval_us(0), None);
    }

    /// From a tick at 1000 on a 100 µs grid: now before or on the tick -> 1100; between points -> the next one up; on a point -> that point; late by 2.5 intervals -> skips to 1300.
    #[test]
    fn next_grid_point() {
        assert_eq!(next_grid_us(1000, 100, 900), 1100);
        assert_eq!(next_grid_us(1000, 100, 1000), 1100);
        assert_eq!(next_grid_us(1000, 100, 1050), 1100);
        assert_eq!(next_grid_us(1000, 100, 1100), 1100);
        assert_eq!(next_grid_us(1000, 100, 1250), 1300);
    }

    /// Ticks are counted per kind and reported once the 2 s window has passed, then counting restarts.
    #[test]
    fn stats_window() {
        let mut s = TickStats::default();
        assert_eq!(s.count(false, 0), None);
        assert_eq!(s.count(true, 1_000_000), None);
        assert_eq!(s.count(true, 2_000_000), Some((1, 2, 2_000_000)));
        assert_eq!((s.paced, s.idle), (0, 0));
        assert_eq!(s.count(false, 2_500_000), None);
    }
}
