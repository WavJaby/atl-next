// SPDX-License-Identifier: MIT
//! `Readback` tier counter: frames copied, plus the cost of each copy, one line per >= 1 s of readback activity (never per frame) and one at the first frame, so a run shows the tier ran before its first period ends.
//! Line: `readback_frames=<total> ms=<period> frames=<n> sync_us=<mean>/<max> read_us=<mean>/<max>`; `read` = `glReadPixels` into the buffer's CPU mapping (returns once the GPU finished: GPU wait + transfer + the CPU write), `sync` = the dma-buf SYNC_START + SYNC_END pair around it.

use std::time::{Duration, Instant};

/// A line covers at least this much time: the rate limit.
const LINE_PERIOD: Duration = Duration::from_secs(1);

/// One stage's timings since the last line.
#[derive(Default, Clone, Copy)]
struct Stage {
    n: u64,
    sum: Duration,
    max: Duration,
}

impl Stage {
    fn add(&mut self, d: Duration) {
        self.n += 1;
        self.sum += d;
        self.max = self.max.max(d);
    }

    /// `mean/max` in us; `-` when the stage did not run.
    fn show(&self) -> String {
        match self.n {
            0 => "-".to_owned(),
            n => format!(
                "{}/{}",
                self.sum.as_micros() / u128::from(n),
                self.max.as_micros()
            ),
        }
    }
}

/// The counter of the `Readback` tier (one per process, behind the driver's lock).
#[derive(Default)]
pub struct ReadbackStats {
    total: u64,
    since: Option<Instant>,
    sync: Stage,
    read: Stage,
}

impl ReadbackStats {
    /// One frame copied at `now`: `read` in glReadPixels, `sync` in the dma-buf sync pair. Returns the line to log when one is due.
    pub fn frame(&mut self, now: Instant, read: Duration, sync: Duration) -> Option<String> {
        self.total += 1;
        self.read.add(read);
        self.sync.add(sync);
        let period = match self.since {
            // first frame: a line at once, the period starts here
            None => Duration::ZERO,
            Some(s) if now.duration_since(s) >= LINE_PERIOD => now.duration_since(s),
            Some(_) => return None,
        };
        let line = format!(
            "{}{} ms={} frames={} sync_us={} read_us={}",
            crate::surface::READBACK_FRAMES_LINE,
            self.total,
            period.as_millis(),
            self.read.n,
            self.sync.show(),
            self.read.show(),
        );
        self.since = Some(now);
        self.sync = Stage::default();
        self.read = Stage::default();
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-derived: reads of 2 + 4 ms in one period -> mean 3000 us, max 4000 us; the first frame logs alone, the frame before the period ends logs nothing.
    #[test]
    fn line_per_period() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut s = ReadbackStats::default();
        assert_eq!(
            s.frame(t0, ms(1), ms(0)).as_deref(),
            Some("readback_frames=1 ms=0 frames=1 sync_us=0/0 read_us=1000/1000")
        );
        assert_eq!(s.frame(t0 + ms(500), ms(2), ms(1)), None);
        assert_eq!(
            s.frame(t0 + ms(1000), ms(4), ms(1)).as_deref(),
            Some("readback_frames=3 ms=1000 frames=2 sync_us=1000/1000 read_us=3000/4000")
        );
        assert_eq!(s.frame(t0 + ms(1100), ms(1), ms(1)), None);
    }
}
