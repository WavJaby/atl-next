// SPDX-License-Identifier: MIT
//! The window's presented frame count, aggregated per second: the `composer: fps ...` line, so a run tells its frame rate and the sizes it ran at without parsing the toolkit's stderr.
//! One line per completed window while frames flow, none while idle (diagnostics F4: no per-frame flood). Toolkit-neutral: the caller passes times and sizes.

/// Aggregation window. 1 s: the unit fps is stated in, so a line's `presented` is the fps directly with no scaling; long enough that one late frame moves it by one count, short enough to follow a size change.
pub const PERIOD_NS: u64 = 1_000_000_000;

/// A width x height in pixels.
pub type Size = (u32, u32);

/// One completed window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Second {
    /// Frames presented in the window.
    pub presented: u64,
    /// Scenes the window took from the mailbox in it (presented + refused).
    pub taken: u64,
    /// The app buffer of the last frame in the window (the largest buffer on screen).
    pub buffer: Size,
    /// The window's size at that frame.
    pub window: Size,
}

impl Second {
    /// `composer: fps presented=<n> taken=<n> buffer=<W>x<H> window=<W>x<H>` (run-apk's summary reads it).
    pub fn line(&self) -> String {
        format!(
            "composer: fps presented={} taken={} buffer={}x{} window={}x{}",
            self.presented, self.taken, self.buffer.0, self.buffer.1, self.window.0, self.window.1
        )
    }
}

/// The meter: frames since the open window started at the first frame after the last one closed.
#[derive(Debug, Default)]
pub struct FrameRate {
    /// Start of the open window, and the `taken` total then.
    start: Option<(u64, u64)>,
    presented: u64,
    /// Frames presented since the start (the first-frame signal reads it).
    total: u64,
    last: Option<(Size, Size)>,
}

impl FrameRate {
    /// Frames presented so far.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// A take at `time_ns` (monotonic); `taken` is the mailbox's running total, `shown` the frame's (buffer, window) sizes when it was presented, None when refused.
    /// Returns the window this call closed: the first event at or past start + [`PERIOD_NS`] closes it and opens the next at its own time, so an idle gap is never counted as a window.
    pub fn record(
        &mut self,
        time_ns: u64,
        taken: u64,
        shown: Option<(Size, Size)>,
    ) -> Option<Second> {
        let mut closed = None;
        if let Some((start, taken_then)) = self.start
            && time_ns >= start + PERIOD_NS
        {
            // the closing event belongs to the next window: `taken` minus this one
            if let Some((buffer, window)) = self.last {
                closed = Some(Second {
                    presented: self.presented,
                    taken: taken.saturating_sub(1).saturating_sub(taken_then),
                    buffer,
                    window,
                });
            }
            self.start = None;
        }
        if self.start.is_none() {
            self.start = Some((time_ns, taken.saturating_sub(1)));
            self.presented = 0;
            self.last = None;
        }
        if let Some(sizes) = shown {
            self.presented += 1;
            self.total += 1;
            self.last = Some(sizes);
        }
        closed
    }
}

/// The largest buffer in a scene: the app's main surface (several surfaces: the one with the most pixels).
pub fn largest_buffer(scene: &atl_compositor::scene::Scene) -> Size {
    scene
        .quads
        .iter()
        .filter_map(atl_compositor::scene::Quad::buffer)
        .map(|b| (b.buffer.width.max(0) as u32, b.buffer.height.max(0) as u32))
        .max_by_key(|&(w, h)| u64::from(w) * u64::from(h))
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const B: Size = (447, 745);
    const W: Size = (480, 800);

    /// Frames every 100 ms from t=0: the frame at t=1000 ms closes [0, 1000) holding t=0..900 = 10 frames, 10 takes; nothing closes before it.
    #[test]
    fn ten_frames_a_second() {
        let mut m = FrameRate::default();
        for i in 0..10 {
            assert_eq!(
                m.record(i * 100 * MS, i + 1, Some((B, W))),
                None,
                "frame {i}"
            );
        }
        let s = m.record(1000 * MS, 11, Some((B, W))).expect("closed");
        assert_eq!(
            s,
            Second {
                presented: 10,
                taken: 10,
                buffer: B,
                window: W
            }
        );
        assert_eq!(
            s.line(),
            "composer: fps presented=10 taken=10 buffer=447x745 window=480x800"
        );
        assert_eq!(m.total(), 11);
    }

    /// A refused take counts in `taken`, not `presented`; after a 5 s gap the next frame closes only the window before it (2 presented, 3 taken) and opens a fresh one.
    #[test]
    fn refused_and_idle_gap() {
        let mut m = FrameRate::default();
        assert_eq!(m.record(0, 1, Some((B, W))), None);
        assert_eq!(m.record(100 * MS, 2, None), None);
        assert_eq!(m.record(200 * MS, 3, Some(((121, 175), W))), None);
        let s = m.record(5200 * MS, 4, Some((B, W))).expect("closed");
        assert_eq!(
            s,
            Second {
                presented: 2,
                taken: 3,
                buffer: (121, 175),
                window: W
            }
        );
        assert_eq!(
            m.record(6100 * MS, 5, Some((B, W))),
            None,
            "window opened at 5200 ms"
        );
    }
}
