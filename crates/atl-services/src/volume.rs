// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/audio/AudioService.java, frameworks/base/media/java/android/media/AudioSystem.java
//! The app-visible stream volumes AudioService keeps (VolumeStreamState): per stream an index in AOSP's tenths (index * 10), its min / max, a mute flag; streams that share an alias (the
//! non-voice STREAM_VOLUME_ALIAS_DEFAULT table: SYSTEM, NOTIFICATION, ... follow RING) move together, rescaled (rescaleIndex) to each stream's range.
//! Linux convention (behaviour decision 3): the host mixer owns the real output volume, so these are the app's own gain, remembered for this services session and never forwarded to the host
//! mixer -- an app setting STREAM_MUSIC to 0 must not mute the user's desktop, and the desktop volume keys are the mixer's, not an Android stream's. The start values are AOSP's defaults on a
//! device with no saved settings: AudioSystem.DEFAULT_STREAM_VOLUME, so an app reads a non-zero, unmuted media volume as on a fresh device.

/// AudioSystem.NUM_STREAM_TYPES (getNumStreamTypes): STREAM_VOICE_CALL 0 .. STREAM_ASSISTANT 11; ensureValidStreamType rejects anything outside.
pub const NUM_STREAM_TYPES: usize = 12;
pub const STREAM_RING: i32 = 2;
pub const STREAM_MUSIC: i32 = 3;
/// AudioSystem.DEFAULT_STREAM_VOLUME, per stream (the ro.config.*_vol_default overrides unset).
const DEFAULT_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [4, 7, 5, 5, 6, 5, 7, 7, 5, 5, 5, 5];
/// AudioService.MAX_STREAM_VOLUME (the ro.config.*_vol_steps overrides unset).
const MAX_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [5, 7, 7, 15, 7, 7, 15, 7, 15, 15, 15, 15];
/// AudioService.MIN_STREAM_VOLUME: the unprivileged minimum (getMinIndex(false)).
const MIN_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0];
/// AudioService.STREAM_VOLUME_ALIAS_DEFAULT: a platform that is neither voice-capable nor single-volume (a desktop has no telephony: AudioSystem.getPlatformType -> PLATFORM_DEFAULT).
const ALIAS: [usize; NUM_STREAM_TYPES] = [0, 2, 2, 3, 4, 2, 6, 2, 2, 3, 3, 3];
/// AudioManager ADJUST_*.
pub const ADJUST_RAISE: i32 = 1;
pub const ADJUST_LOWER: i32 = -1;
pub const ADJUST_SAME: i32 = 0;
pub const ADJUST_MUTE: i32 = -100;
pub const ADJUST_UNMUTE: i32 = 100;
pub const ADJUST_TOGGLE_MUTE: i32 = 101;

/// A stream type outside 0..NUM_STREAM_TYPES (ensureValidStreamType's IllegalArgumentException).
#[derive(Debug, PartialEq)]
pub struct BadStream(pub i32);

/// Every stream's state; indices in tenths as VolumeStreamState keeps them.
#[derive(Debug, Clone)]
pub struct Volumes {
    index: [i32; NUM_STREAM_TYPES],
    muted: [bool; NUM_STREAM_TYPES],
}

fn stream(s: i32) -> Result<usize, BadStream> {
    usize::try_from(s)
        .ok()
        .filter(|&s| s < NUM_STREAM_TYPES)
        .ok_or(BadStream(s))
}

/// AudioService.rescaleIndex(index, src, dst) over each stream's [min, max] in tenths.
fn rescale(index: i32, src: usize, dst: usize) -> i32 {
    let (smin, smax) = (MIN_STREAM_VOLUME[src] * 10, MAX_STREAM_VOLUME[src] * 10);
    let (dmin, dmax) = (MIN_STREAM_VOLUME[dst] * 10, MAX_STREAM_VOLUME[dst] * 10);
    let (sr, dr) = (smax - smin, dmax - dmin);
    if sr == 0 {
        return dmin;
    }
    dmin + ((index - smin) * dr + sr / 2) / sr
}

impl Default for Volumes {
    /// VolumeStreamState.readSettings with no saved values, then checkAllAliasStreamVolumes: each stream at its alias' default, rescaled to its own range.
    fn default() -> Self {
        let mut v = Volumes {
            index: [0; NUM_STREAM_TYPES],
            muted: [false; NUM_STREAM_TYPES],
        };
        for (s, &a) in ALIAS.iter().enumerate() {
            v.index[s] = rescale(DEFAULT_STREAM_VOLUME[a] * 10, a, s);
        }
        v
    }
}

impl Volumes {
    /// getStreamVolume: `(index + 5) / 10`, 0 while muted ("by convention").
    pub fn volume(&self, s: i32) -> Result<i32, BadStream> {
        let s = stream(s)?;
        Ok(if self.muted[s] {
            0
        } else {
            (self.index[s] + 5) / 10
        })
    }

    /// getLastAudibleStreamVolume: the index regardless of mute.
    pub fn last_audible(&self, s: i32) -> Result<i32, BadStream> {
        Ok((self.index[stream(s)?] + 5) / 10)
    }

    pub fn max(&self, s: i32) -> Result<i32, BadStream> {
        Ok(MAX_STREAM_VOLUME[stream(s)?])
    }

    pub fn min(&self, s: i32) -> Result<i32, BadStream> {
        Ok(MIN_STREAM_VOLUME[stream(s)?])
    }

    pub fn is_muted(&self, s: i32) -> Result<bool, BadStream> {
        Ok(self.muted[stream(s)?])
    }

    /// Sets the alias group from `index` (tenths) on stream `s`: VolumeStreamState.setIndex's clamp (getValidIndex), then setAllIndexes rescaled to every stream of the alias.
    fn set_group(&mut self, s: usize, index: i32) {
        let index = index.clamp(MIN_STREAM_VOLUME[s] * 10, MAX_STREAM_VOLUME[s] * 10);
        let a = ALIAS[s];
        for t in (0..NUM_STREAM_TYPES).filter(|&t| ALIAS[t] == a) {
            self.index[t] = if t == s { index } else { rescale(index, s, t) };
        }
    }

    fn mute_group(&mut self, s: usize, muted: bool) {
        let a = ALIAS[s];
        for t in (0..NUM_STREAM_TYPES).filter(|&t| ALIAS[t] == a) {
            self.muted[t] = muted;
        }
    }

    /// setStreamVolume: the index on the stream's own scale, clamped; a non-zero index unmutes (VolumeStreamState.setIndex: `mute(index == 0)` for a muteable stream).
    pub fn set(&mut self, s: i32, index: i32) -> Result<(), BadStream> {
        let s = stream(s)?;
        self.set_group(s, index * 10);
        let muted = self.index[s] == 0;
        self.mute_group(s, muted);
        Ok(())
    }

    /// adjustStreamVolume: RAISE / LOWER one step (10 tenths, on the stream's alias scale -- rescaleStep is identity for the alias itself), SAME nothing, MUTE / UNMUTE / TOGGLE_MUTE the
    /// group's mute flag. Raising a muted stream unmutes it; any other direction is AOSP's IllegalArgumentException ("Bad direction").
    pub fn adjust(&mut self, s: i32, direction: i32) -> Result<bool, BadStream> {
        let s = stream(s)?;
        let a = ALIAS[s];
        match direction {
            ADJUST_RAISE | ADJUST_LOWER => {
                if direction == ADJUST_RAISE && self.muted[a] {
                    self.mute_group(a, false);
                }
                let next = self.index[a] + direction * 10;
                self.set_group(a, next);
                if self.index[a] == 0 {
                    self.mute_group(a, true);
                }
            }
            ADJUST_SAME => {}
            ADJUST_MUTE => self.mute_group(a, true),
            ADJUST_UNMUTE => self.mute_group(a, false),
            ADJUST_TOGGLE_MUTE => {
                let m = !self.muted[a];
                self.mute_group(a, m);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh device: STREAM_MUSIC reads DEFAULT_STREAM_VOLUME[3] = 5 of MAX 15 (AudioSystem / AudioService tables); RING 5 of 7.
    #[test]
    fn defaults() {
        let v = Volumes::default();
        assert_eq!(v.volume(STREAM_MUSIC), Ok(5));
        assert_eq!(v.max(STREAM_MUSIC), Ok(15));
        assert_eq!(v.min(STREAM_MUSIC), Ok(0));
        assert_eq!(v.volume(STREAM_RING), Ok(5));
        assert_eq!(v.is_muted(STREAM_MUSIC), Ok(false));
        // STREAM_DTMF (8) follows RING: rescaleIndex(50, [0,70] -> [0,150]) = (50*150 + 35) / 70 = 107 tenths -> (107 + 5) / 10 = 11
        assert_eq!(v.volume(8), Ok(11));
        assert_eq!(v.volume(12), Err(BadStream(12)));
        assert_eq!(v.volume(-1), Err(BadStream(-1)));
    }

    /// setStreamVolume clamps to [min, max] and drags the alias group: TTS (9) follows MUSIC on the same 0..15 scale.
    #[test]
    fn set_and_alias() {
        let mut v = Volumes::default();
        v.set(STREAM_MUSIC, 99).expect("valid stream");
        assert_eq!(v.volume(STREAM_MUSIC), Ok(15));
        assert_eq!(v.volume(9), Ok(15));
        v.set(STREAM_MUSIC, 0).expect("valid stream");
        assert_eq!(
            (v.volume(STREAM_MUSIC), v.is_muted(STREAM_MUSIC)),
            (Ok(0), Ok(true))
        );
        // ALARM's minimum is 1 (MIN_STREAM_VOLUME[4])
        v.set(4, 0).expect("valid stream");
        assert_eq!(v.volume(4), Ok(1));
    }

    /// adjust: one step per call; mute keeps the index (getLastAudibleStreamVolume), raise unmutes.
    #[test]
    fn adjust_and_mute() {
        let mut v = Volumes::default();
        assert_eq!(v.adjust(STREAM_MUSIC, ADJUST_LOWER), Ok(true));
        assert_eq!(v.volume(STREAM_MUSIC), Ok(4));
        v.adjust(STREAM_MUSIC, ADJUST_MUTE).expect("valid stream");
        assert_eq!(
            (v.volume(STREAM_MUSIC), v.last_audible(STREAM_MUSIC)),
            (Ok(0), Ok(4))
        );
        v.adjust(STREAM_MUSIC, ADJUST_RAISE).expect("valid stream");
        assert_eq!(v.volume(STREAM_MUSIC), Ok(5));
        assert_eq!(v.adjust(STREAM_MUSIC, 7), Ok(false));
    }
}
