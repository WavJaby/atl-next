// SPDX-License-Identifier: MIT
//! The app volume (behaviour decision 3: the host's per-application volume is the app's volume): [`AppVolume`] holds the one value, an AOSP music index and a mute flag, for the app's own host stream.
//! It is the only writer to that stream's volume (output.rs applies what it hands out) and follows what the host reports back (a desktop mixer, the session manager's restore). The host stays the source of truth: an outside value is kept as the host has it, only the index is quantised.
//! AOSP model: an absolute-volume device (AudioManager.DEVICE_VOLUME_BEHAVIOR_ABSOLUTE), whose controller this is.

use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

use atl_platform_ui::audio::StreamVolume;

/// Index `index` of `max` as the host stream's linear amplitude: (index / max)^3.
/// Derivation: AOSP forwards an absolute-volume device's index linearly into the device's own range (AudioService.rescaleIndex); the device here is the desktop's per-stream slider, whose position is the cube root of the linear amplitude PipeWire stores
/// (pulse-protocol volume = pa_sw_volume_from_linear = cbrt; measured: channelVolumes 0.064 / 0.216 / 1.0 read 40 / 60 / 100 %). So slider fraction = index / max, linear = fraction^3. The AOSP media curve (VolumeCurve) is not used: an absolute device bypasses local attenuation.
pub(crate) fn index_to_linear(index: u32, max: u32) -> f32 {
    if max == 0 {
        return 1.0;
    }
    let fraction = f64::from(index.min(max)) / f64::from(max);
    fraction.powi(3) as f32
}

/// The inverse of [`index_to_linear`]: round(max * cbrt(linear)), clamped to 0..=max. Exact for every step: cbrt's error (~1e-7) is far below half a step (0.5 / max).
pub(crate) fn linear_to_index(linear: f32, max: u32) -> u32 {
    let fraction = f64::from(linear.clamp(0.0, 1.0)).cbrt();
    (f64::from(max) * fraction).round() as u32
}

/// The app's volume: index in 0..=max of AOSP's music stream, and mute.
pub(crate) struct AppVolume {
    max: u32,
    state: Mutex<State>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct State {
    index: u32,
    mute: bool,
    /// Set by the app side and not yet written to a stream: applied when the stream appears (or at once when it exists). Kept apart from index/mute: until it is written, host reports are dropped (the write replaces the host's value).
    pending: Option<(u32, bool)>,
}

impl AppVolume {
    /// `max`: the music stream's maximum index (AudioService.MAX_STREAM_VOLUME). Starts at the host's default for a new stream (WirePlumber `node.stream.default-playback-volume` 1.0 = index max), unmuted, nothing pending.
    pub(crate) fn new(max: u32) -> Self {
        Self {
            max,
            state: Mutex::new(State {
                index: max,
                mute: false,
                pending: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The app side sets index (clamped to max) and mute; the write happens through [`AppVolume::take_pending`].
    pub(crate) fn set(&self, index: u32, mute: bool) {
        self.lock().pending = Some((index.min(self.max), mute));
    }

    /// The value to write to the stream, if the app side set one since the last write; it becomes the current value. Logs `app-volume: applied ...`.
    pub(crate) fn take_pending(&self) -> Option<StreamVolume> {
        let mut s = self.lock();
        let (index, mute) = s.pending.take()?;
        s.index = index;
        s.mute = mute;
        let linear = index_to_linear(index, self.max);
        let _ = writeln!(
            std::io::stderr(),
            "app-volume: applied index={index} max={} mute={mute} linear={linear:.4}",
            self.max
        );
        Some(StreamVolume { linear, mute })
    }

    /// The current (index, mute): the last value written or observed.
    pub(crate) fn current(&self) -> (u32, bool) {
        let s = self.lock();
        (s.index, s.mute)
    }

    /// The host reported the stream's volume. Returns the change when index or mute changed (an echo of our own write does not), and logs `app-volume: observed ...`.
    /// While an app-side value is pending the report is dropped: AudioService already holds the app's value and the pending write puts it on the host.
    pub(crate) fn observed(&self, volume: StreamVolume) -> Option<Report> {
        let index = linear_to_index(volume.linear, self.max);
        let mut s = self.lock();
        let before = (s.index, s.mute);
        if s.pending.is_some() || before == (index, volume.mute) {
            return None;
        }
        s.index = index;
        s.mute = volume.mute;
        let _ = writeln!(
            std::io::stderr(),
            "app-volume: observed index={index} max={} mute={} linear={:.4}",
            self.max,
            volume.mute,
            volume.linear
        );
        Some(Report {
            before,
            now: (index, volume.mute),
        })
    }
}

/// A host-side change of the app volume, (index, mute) before and now; AudioService holds `before`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Report {
    pub(crate) before: (u32, bool),
    pub(crate) now: (u32, bool),
}

impl Report {
    /// What AudioService is told, as (setStreamVolume index, adjustStreamVolume mute): the index when it changed; the mute when it differs from what AudioService then holds -- its own mute
    /// unless setStreamVolume reset it (onSetStreamVolume: `muteAliasStreams(index == 0)`).
    pub(crate) fn calls(&self) -> (Option<u32>, Option<bool>) {
        let (index, mute) = self.now;
        let set = (index != self.before.0).then_some(index);
        let held = if set.is_some() {
            index == 0
        } else {
            self.before.1
        };
        (set, (mute != held).then_some(mute))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-derived (i / 15)^3: 0, (6/15)^3 = 0.4^3 = 0.064, (9/15)^3 = 0.6^3 = 0.216, 1. 15 is AOSP's music max (AudioService.MAX_STREAM_VOLUME[STREAM_MUSIC]).
    const STEPS: [(u32, f32); 4] = [(0, 0.0), (6, 0.064), (9, 0.216), (15, 1.0)];

    #[test]
    fn index_to_linear_is_the_cube_of_the_fraction() {
        for (index, linear) in STEPS {
            assert!(
                (index_to_linear(index, 15) - linear).abs() < 1e-6,
                "{index}"
            );
        }
    }

    #[test]
    fn linear_to_index_takes_the_cube_root() {
        for (index, linear) in STEPS {
            assert_eq!(linear_to_index(linear, 15), index, "{linear}");
        }
        // half way between steps 6 and 7 in slider terms: 6.4 / 15 -> rounds to 6; 6.6 / 15 -> 7
        assert_eq!(linear_to_index((6.4f32 / 15.0).powi(3), 15), 6);
        assert_eq!(linear_to_index((6.6f32 / 15.0).powi(3), 15), 7);
    }

    #[test]
    fn pending_wins_over_a_host_report_and_echo_is_silent() {
        let v = AppVolume::new(15);
        assert_eq!(v.current(), (15, false));
        v.set(9, false);
        // the host restores a value before our write: not reported (AudioService already holds the app's value, which the write puts on the host), the pending write stays
        assert_eq!(
            v.observed(StreamVolume {
                linear: 0.064,
                mute: false
            }),
            None
        );
        let w = v.take_pending().expect("pending");
        assert!((w.linear - 0.216).abs() < 1e-6 && !w.mute);
        assert_eq!(v.take_pending(), None);
        assert_eq!(v.current(), (9, false));
        // the server echoes our own write: no change
        assert_eq!(
            v.observed(StreamVolume {
                linear: 0.216,
                mute: false
            }),
            None
        );
        assert_eq!(
            v.observed(StreamVolume {
                linear: 0.216,
                mute: true
            }),
            Some(Report {
                before: (9, false),
                now: (9, true)
            })
        );
    }

    /// The host value at stream creation (the session manager's restore, measured as a Props event at connect) with nothing pending is a change like any other.
    #[test]
    fn a_restore_with_nothing_pending_is_reported() {
        let v = AppVolume::new(15);
        assert_eq!(
            v.observed(StreamVolume {
                linear: 0.216,
                mute: false
            }),
            Some(Report {
                before: (15, false),
                now: (9, false)
            })
        );
    }

    /// The calls a report becomes (AudioService's own rules decide the mute a setStreamVolume implies: index 0 mutes, any other unmutes, onSetStreamVolume).
    #[test]
    fn reports_become_the_fewest_calls() {
        let r = |before, now| Report { before, now }.calls();
        // index only
        assert_eq!(r((9, false), (6, false)), (Some(6), None));
        // mute only: the index stays, so only the adjustment
        assert_eq!(r((6, false), (6, true)), (None, Some(true)));
        assert_eq!(r((6, true), (6, false)), (None, Some(false)));
        // a new index while muted: setStreamVolume unmutes, so the mute is sent again
        assert_eq!(r((6, true), (9, true)), (Some(9), Some(true)));
        // to 0 unmuted: setStreamVolume(0) mutes, so unmute follows
        assert_eq!(r((6, false), (0, false)), (Some(0), Some(false)));
        assert_eq!(r((6, false), (0, true)), (Some(0), None));
    }
}
