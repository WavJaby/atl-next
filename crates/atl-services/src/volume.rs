// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/audio/AudioService.java, frameworks/base/media/java/android/media/AudioSystem.java
//! The app-visible stream volumes AudioService keeps (VolumeStreamState): per stream an index in AOSP's tenths (index * 10), its min / max, a mute flag; streams that share an alias (the
//! non-voice STREAM_VOLUME_ALIAS_DEFAULT table: SYSTEM, NOTIFICATION, ... follow RING) move together, rescaled (rescaleIndex) to each stream's range.
//! One state per app uid ([`AppVolumes`]), not one per device: deviation from AOSP's device-wide volume, forced by the owner decision that an app's volume is the host's per-application volume
//! (behaviour decision 3) -- app A's setStreamVolume does not change what app B reads.
//! STREAM_MUSIC's alias group is an absolute-volume device (AudioManager.DEVICE_VOLUME_BEHAVIOR_ABSOLUTE) once the app process registers its controller (atl-audioflinger, the app's own host
//! stream): an app-side change is dispatched to it, a host-side change comes back with FLAG_ABSOLUTE_VOLUME and is not dispatched again. Every other alias group stays the app's own gain:
//! answered and remembered, never forwarded to the host (no app needs ring / alarm / call gain on a desktop yet). Start values: AudioSystem.DEFAULT_STREAM_VOLUME, until the controller registers
//! the host's value.

use std::collections::BTreeMap;

pub use atl_services_wire::audio_volume::{
    ADJUST_MUTE, ADJUST_UNMUTE, FLAG_ABSOLUTE_VOLUME, NUM_STREAM_TYPES, STREAM_MUSIC, STREAM_RING,
};
use atl_services_wire::audio_volume::{
    ALIAS, DEFAULT_STREAM_VOLUME, MAX_STREAM_VOLUME, MIN_STREAM_VOLUME,
};
/// AudioManager ADJUST_*.
pub const ADJUST_RAISE: i32 = 1;
pub const ADJUST_LOWER: i32 = -1;
pub const ADJUST_SAME: i32 = 0;
pub const ADJUST_TOGGLE_MUTE: i32 = 101;

/// A stream type outside 0..NUM_STREAM_TYPES (ensureValidStreamType's IllegalArgumentException).
#[derive(Debug, PartialEq)]
pub struct BadStream(pub i32);

/// Every stream's state; indices in tenths as VolumeStreamState keeps them.
#[derive(Debug, Clone, PartialEq)]
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

/// AudioService.rescaleIndex(index, srcMin, srcMax, dstMin, dstMax):3362.
fn rescale_span(index: i32, (smin, smax): (i32, i32), (dmin, dmax): (i32, i32)) -> i32 {
    let (sr, dr) = (smax - smin, dmax - dmin);
    if sr == 0 {
        return dmin;
    }
    dmin + ((index - smin) * dr + sr / 2) / sr
}

/// A stream's [min, max] in tenths (VolumeStreamState.getMinIndex / getMaxIndex).
fn span(s: usize) -> (i32, i32) {
    (MIN_STREAM_VOLUME[s] * 10, MAX_STREAM_VOLUME[s] * 10)
}

/// AudioService.rescaleIndex(index, src, dst):3340 over each stream's [min, max] in tenths.
fn rescale(index: i32, src: usize, dst: usize) -> i32 {
    rescale_span(index, span(src), span(dst))
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

/// What a change did, as the broadcasts AOSP sends for it.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// (stream, previous, new) in steps for each stream whose step changed: VOLUME_CHANGED_ACTION each (VolumeStreamState.setIndex:9822-9869, every stream of the alias on a non-single-volume device).
    pub volume: Vec<(i32, i32, i32)>,
    /// (stream, muted) for each stream whose mute changed: STREAM_MUTE_CHANGED_ACTION each (muteAliasStreams:4280-4302, broadcastMuteSetting:4307-4318).
    pub mute: Vec<(i32, bool)>,
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

    /// The broadcasts owed for going from `before` to `self`: steps compared as setIndex rounds them (`(index + 5) / 10`), mute flags as muteAliasStreams' `changed`.
    fn since(&self, before: &Volumes) -> Changes {
        let mut c = Changes::default();
        for s in 0..NUM_STREAM_TYPES {
            let (old, new) = ((before.index[s] + 5) / 10, (self.index[s] + 5) / 10);
            let t = s as i32;
            if old != new {
                c.volume.push((t, old, new));
            }
            if before.muted[s] != self.muted[s] {
                c.mute.push((t, self.muted[s]));
            }
        }
        c
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

    /// setStreamVolume: the index on the stream's own scale, clamped; then onSetStreamVolume's `muteAliasStreams(stream, index == 0)`: a zero index mutes the group, any other unmutes it.
    pub fn set(&mut self, s: i32, index: i32) -> Result<Changes, BadStream> {
        let s = stream(s)?;
        let before = self.clone();
        self.set_group(s, index * 10);
        let muted = self.index[s] == 0;
        self.mute_group(s, muted);
        Ok(self.since(&before))
    }

    /// adjustStreamVolume: RAISE / LOWER one step (10 tenths, on the stream's alias scale -- rescaleStep is identity for the alias itself), SAME nothing, MUTE / UNMUTE / TOGGLE_MUTE the
    /// group's mute flag. Raising a muted stream unmutes it; any other direction is AOSP's IllegalArgumentException ("Bad direction"): None.
    pub fn adjust(&mut self, s: i32, direction: i32) -> Result<Option<Changes>, BadStream> {
        let s = stream(s)?;
        let a = ALIAS[s];
        let before = self.clone();
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
            _ => return Ok(None),
        }
        Ok(Some(self.since(&before)))
    }
}

/// An app's absolute-volume device as registered (AudioService.AbsoluteVolumeDeviceInfo): the alias group its VolumeInfo names and that VolumeInfo's index range, in steps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Absolute {
    pub stream: i32,
    pub min: i32,
    pub max: i32,
}

/// What an app-side change owes the app's absolute-volume device.
#[derive(Debug, PartialEq)]
pub enum Dispatch {
    /// Nothing: the stream is outside the MUSIC alias group, or the change came from the device (FLAG_ABSOLUTE_VOLUME).
    None,
    /// dispatchDeviceVolumeChanged (dispatchAbsoluteVolumeChanged:5310-5321): the group's index rescaled into the device's range (rescaleIndex(index, stream, VolumeInfo):3327), and its mute.
    Changed {
        device: Absolute,
        index: i32,
        muted: bool,
    },
    /// A MUSIC change of a uid that registered no device (no host stream): it stays app-visible and reaches no host volume; the caller counts it.
    NoDevice,
}

/// The per-app state: each uid's stream volumes and its absolute-volume device.
#[derive(Debug, Default)]
pub struct AppVolumes {
    apps: BTreeMap<u32, App>,
}

#[derive(Debug, Default)]
struct App {
    volumes: Volumes,
    device: Option<Absolute>,
}

impl AppVolumes {
    /// `uid`'s volumes; a uid seen first starts at AOSP's defaults.
    pub fn of(&mut self, uid: u32) -> &Volumes {
        &self.apps.entry(uid).or_default().volumes
    }

    /// registerDeviceVolumeDispatcherForAbsoluteVolume(register = true):8208-8250: the device is kept and its VolumeInfo's index, rescaled into the stream's range (rescaleIndex(VolumeInfo,
    /// stream):3314), set with setStreamVolumeInt -- no mute change ("Mute state is not updated because it is stream-wide"). A later registration replaces the earlier one.
    pub fn register(
        &mut self,
        uid: u32,
        device: Absolute,
        index: i32,
    ) -> Result<Changes, BadStream> {
        let s = stream(device.stream)?;
        let app = self.apps.entry(uid).or_default();
        app.device = Some(device);
        let before = app.volumes.clone();
        let tenths = rescale_span(index, (device.min, device.max), span(s));
        app.volumes.set_group(s, tenths);
        Ok(app.volumes.since(&before))
    }

    /// register = false: the device adopts variable volume behaviour again (nothing is dispatched any more).
    pub fn unregister(&mut self, uid: u32) {
        if let Some(app) = self.apps.get_mut(&uid) {
            app.device = None;
        }
    }

    /// What the change on stream `s` owes the device (setStreamVolume:5267-5271, adjustStreamVolume:4166-4170): a stream of the device's alias group, changed by the app (no FLAG_ABSOLUTE_VOLUME).
    fn dispatch(app: &App, s: usize, flags: i32) -> Dispatch {
        if flags & FLAG_ABSOLUTE_VOLUME != 0 {
            return Dispatch::None;
        }
        let Some(device) = app
            .device
            .filter(|d| stream(d.stream).is_ok_and(|ds| ALIAS[ds] == ALIAS[s]))
        else {
            return if ALIAS[s] == ALIAS[STREAM_MUSIC as usize] {
                Dispatch::NoDevice
            } else {
                Dispatch::None
            };
        };
        let a = ALIAS[s];
        Dispatch::Changed {
            device,
            index: rescale_span(app.volumes.index[a], span(a), (device.min, device.max)),
            muted: app.volumes.muted[a],
        }
    }

    /// setStreamVolume for `uid` with `flags`.
    pub fn set(
        &mut self,
        uid: u32,
        s: i32,
        index: i32,
        flags: i32,
    ) -> Result<(Changes, Dispatch), BadStream> {
        let app = self.apps.entry(uid).or_default();
        let changes = app.volumes.set(s, index)?;
        Ok((changes, Self::dispatch(app, stream(s)?, flags)))
    }

    /// adjustStreamVolume for `uid` with `flags` (the device does not handle adjustments itself: registered with handlesVolumeAdjustment false, so the result is dispatched). None: a bad direction.
    pub fn adjust(
        &mut self,
        uid: u32,
        s: i32,
        direction: i32,
        flags: i32,
    ) -> Result<Option<(Changes, Dispatch)>, BadStream> {
        let app = self.apps.entry(uid).or_default();
        let Some(changes) = app.volumes.adjust(s, direction)? else {
            return Ok(None);
        };
        // AOSP dispatches only when the adjustment did something (adjustVolume && direction != ADJUST_SAME)
        let dispatch = if direction == ADJUST_SAME {
            Dispatch::None
        } else {
            Self::dispatch(app, stream(s)?, flags)
        };
        Ok(Some((changes, dispatch)))
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

    /// setStreamVolume clamps to [min, max] and drags the alias group: TTS (9) follows MUSIC on the same 0..15 scale. Changes: one VOLUME_CHANGED per stream of the group (3, 9, 10, 11).
    #[test]
    fn set_and_alias() {
        let mut v = Volumes::default();
        let c = v.set(STREAM_MUSIC, 99).expect("valid stream");
        assert_eq!(v.volume(STREAM_MUSIC), Ok(15));
        assert_eq!(v.volume(9), Ok(15));
        // ACCESSIBILITY (10) has min 1: rescaleIndex(50, [0,150] -> [10,150]) = 10 + (50*140 + 75) / 150 = 57 -> step 6; to 15 it is 150 -> 15
        assert_eq!(
            c.volume,
            vec![(3, 5, 15), (9, 5, 15), (10, 6, 15), (11, 5, 15)]
        );
        assert!(c.mute.is_empty());
        let c = v.set(STREAM_MUSIC, 0).expect("valid stream");
        assert_eq!(
            (v.volume(STREAM_MUSIC), v.is_muted(STREAM_MUSIC)),
            (Ok(0), Ok(true))
        );
        // 10 stays at its min 1 and is muted with the group
        assert_eq!(c.mute, vec![(3, true), (9, true), (10, true), (11, true)]);
        // ALARM's minimum is 1 (MIN_STREAM_VOLUME[4])
        v.set(4, 0).expect("valid stream");
        assert_eq!(v.volume(4), Ok(1));
    }

    /// adjust: one step per call; mute keeps the index (getLastAudibleStreamVolume), raise unmutes.
    #[test]
    fn adjust_and_mute() {
        let mut v = Volumes::default();
        assert!(
            v.adjust(STREAM_MUSIC, ADJUST_LOWER)
                .expect("valid stream")
                .is_some()
        );
        assert_eq!(v.volume(STREAM_MUSIC), Ok(4));
        let c = v
            .adjust(STREAM_MUSIC, ADJUST_MUTE)
            .expect("valid stream")
            .expect("direction");
        assert_eq!(c.mute.first(), Some(&(3, true)));
        assert!(c.volume.is_empty());
        assert_eq!(
            (v.volume(STREAM_MUSIC), v.last_audible(STREAM_MUSIC)),
            (Ok(0), Ok(4))
        );
        v.adjust(STREAM_MUSIC, ADJUST_RAISE).expect("valid stream");
        assert_eq!(v.volume(STREAM_MUSIC), Ok(5));
        assert_eq!(v.adjust(STREAM_MUSIC, 7), Ok(None));
    }

    /// The device the app's controller registers: MUSIC, its own 0..15 range (the controller counts AOSP's music steps).
    const DEVICE: Absolute = Absolute {
        stream: STREAM_MUSIC,
        min: 0,
        max: 15,
    };

    /// Per uid: one app's change leaves another's state alone (the stated deviation from AOSP's device-wide volume).
    #[test]
    fn each_uid_has_its_own_volumes() {
        let mut a = AppVolumes::default();
        a.set(10001, STREAM_MUSIC, 9, 0).expect("valid stream");
        assert_eq!(a.of(10001).volume(STREAM_MUSIC), Ok(9));
        assert_eq!(a.of(10002).volume(STREAM_MUSIC), Ok(5));
    }

    /// Registration sets the group to the device's index without a mute change; an app-side change is dispatched with the rescaled index and mute; a device-side one (FLAG_ABSOLUTE_VOLUME) is not.
    #[test]
    fn absolute_device_dispatch() {
        let mut a = AppVolumes::default();
        let c = a.register(10001, DEVICE, 15).expect("valid stream");
        assert_eq!(c.volume.first(), Some(&(3, 5, 15)));
        assert!(c.mute.is_empty());
        let (c, d) = a.set(10001, STREAM_MUSIC, 9, 0).expect("valid stream");
        assert_eq!(c.volume.first(), Some(&(3, 15, 9)));
        assert_eq!(
            d,
            Dispatch::Changed {
                device: DEVICE,
                index: 9,
                muted: false
            }
        );
        // TTS (9) is in MUSIC's group: dispatched as the group's index
        let (_, d) = a.set(10001, 9, 6, 0).expect("valid stream");
        assert_eq!(
            d,
            Dispatch::Changed {
                device: DEVICE,
                index: 6,
                muted: false
            }
        );
        let (c, d) = a
            .set(10001, STREAM_MUSIC, 6, FLAG_ABSOLUTE_VOLUME)
            .expect("valid stream");
        assert!(c.volume.is_empty());
        assert_eq!(d, Dispatch::None);
        let (c, d) = a
            .adjust(10001, STREAM_MUSIC, ADJUST_MUTE, 0)
            .expect("valid stream")
            .expect("direction");
        assert_eq!(c.mute.first(), Some(&(3, true)));
        assert_eq!(
            d,
            Dispatch::Changed {
                device: DEVICE,
                index: 6,
                muted: true
            }
        );
        let (_, d) = a
            .adjust(10001, STREAM_MUSIC, ADJUST_UNMUTE, FLAG_ABSOLUTE_VOLUME)
            .expect("valid stream")
            .expect("direction");
        assert_eq!(d, Dispatch::None);
        assert_eq!(a.of(10001).is_muted(STREAM_MUSIC), Ok(false));
        // RING's group is not the device's: app-visible only, as before
        let (_, d) = a.set(10001, STREAM_RING, 3, 0).expect("valid stream");
        assert_eq!(d, Dispatch::None);
        a.unregister(10001);
        let (_, d) = a.set(10001, STREAM_MUSIC, 4, 0).expect("valid stream");
        assert_eq!(d, Dispatch::NoDevice);
    }

    /// A device range other than the stream's is rescaled both ways: 0..100 device, index 60 -> rescaleIndex(60, [0,100] -> [0,150]) = (60*150 + 50) / 100 = 90 tenths = step 9; back: (90*100 + 75) / 150 = 60.
    #[test]
    fn device_range_is_rescaled() {
        let mut a = AppVolumes::default();
        let device = Absolute {
            stream: STREAM_MUSIC,
            min: 0,
            max: 100,
        };
        a.register(10001, device, 60).expect("valid stream");
        assert_eq!(a.of(10001).volume(STREAM_MUSIC), Ok(9));
        let (_, d) = a
            .adjust(10001, STREAM_MUSIC, ADJUST_SAME, 0)
            .expect("valid stream")
            .expect("direction");
        assert_eq!(d, Dispatch::None);
        let (_, d) = a.set(10001, STREAM_MUSIC, 9, 0).expect("valid stream");
        assert_eq!(
            d,
            Dispatch::Changed {
                device,
                index: 60,
                muted: false
            }
        );
    }

    /// A MUSIC change of a uid with no registered device reaches no host volume (Dispatch::NoDevice, counted by the service).
    #[test]
    fn a_uid_without_a_device_dispatches_nothing() {
        let mut a = AppVolumes::default();
        let (c, d) = a.set(10003, STREAM_MUSIC, 2, 0).expect("valid stream");
        assert_eq!(c.volume.first(), Some(&(3, 5, 2)));
        assert_eq!(d, Dispatch::NoDevice);
    }
}
