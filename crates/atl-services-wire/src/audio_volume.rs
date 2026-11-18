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

/// The output device STREAM_MUSIC plays to here (getDeviceForStream): the app's own host stream, registered by its controller as an absolute-volume device and accepted by AudioService only as
/// this device. AudioDeviceAttributes.ROLE_OUTPUT (= AudioPort.ROLE_SINK 2), AudioDeviceInfo.TYPE_BUILTIN_SPEAKER 2, AudioSystem.DEVICE_OUT_SPEAKER 0x2 (its internal type).
pub const ROLE_OUTPUT: i32 = 2;
pub const TYPE_BUILTIN_SPEAKER: i32 = 2;
pub const DEVICE_OUT_SPEAKER: i32 = 0x2;
/// AudioDeviceVolumeManager.DEVICE_VOLUME_BEHAVIOR_ABSOLUTE (AudioManager.java:6697): the device applies the volume; AudioService keeps the index and forwards changes.
pub const DEVICE_VOLUME_BEHAVIOR_ABSOLUTE: i32 = 3;
/// AudioManager.FLAG_ABSOLUTE_VOLUME (AudioManager.java:637): the change comes from the absolute-volume device itself; AudioService updates the index and does not dispatch it back.
pub const FLAG_ABSOLUTE_VOLUME: i32 = 1 << 13;
/// AudioManager.ADJUST_MUTE / ADJUST_UNMUTE: the device's mute reports.
pub const ADJUST_MUTE: i32 = -100;
pub const ADJUST_UNMUTE: i32 = 100;
