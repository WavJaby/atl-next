// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/audio/AudioService.java, frameworks/base/media/java/android/media/AudioSystem.java
//! AOSP's stream volume tables, read by AudioService's state (atl-services `volume`) and by the app's host-volume controller (atl-audioflinger), so both count the same steps.

/// AudioSystem.NUM_STREAM_TYPES (getNumStreamTypes): STREAM_VOICE_CALL 0 .. STREAM_ASSISTANT 11; ensureValidStreamType rejects anything outside.
pub const NUM_STREAM_TYPES: usize = 12;
pub const STREAM_RING: i32 = 2;
pub const STREAM_MUSIC: i32 = 3;
/// AudioSystem.DEFAULT_STREAM_VOLUME, per stream (the ro.config.*_vol_default overrides unset).
pub const DEFAULT_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [4, 7, 5, 5, 6, 5, 7, 7, 5, 5, 5, 5];
/// AudioService.MAX_STREAM_VOLUME (the ro.config.*_vol_steps overrides unset).
pub const MAX_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [5, 7, 7, 15, 7, 7, 15, 7, 15, 15, 15, 15];
/// AudioService.MIN_STREAM_VOLUME: the unprivileged minimum (getMinIndex(false)).
pub const MIN_STREAM_VOLUME: [i32; NUM_STREAM_TYPES] = [1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0];
/// AudioService.STREAM_VOLUME_ALIAS_DEFAULT: a platform that is neither voice-capable nor single-volume (a desktop has no telephony: AudioSystem.getPlatformType -> PLATFORM_DEFAULT).
pub const ALIAS: [usize; NUM_STREAM_TYPES] = [0, 2, 2, 3, 4, 2, 6, 2, 2, 3, 3, 3];
