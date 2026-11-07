// SPDX-License-Identifier: MIT
//! AudioFlinger and AudioPolicyService in the app process (game audio A1a): the two binders AOSP's audio clients look up by name, answering the calls that give them ids, a rate, a frame count and a latency.
//! The interfaces are AOSP's AIDL (libaudioclient `aidl/`), compiled by the converter into `atl_audioflinger_stubs`. Calls not answered here stay UNKNOWN_TRANSACTION and are counted (atl-services counters).
//! The one output described ([`OUTPUT_RATE_HZ`], [`OUTPUT_FRAMES`], [`OUTPUT_LATENCY_MS`]) is a fixed profile until the platform PCM output (A2) reports the server's negotiated rate and quantum.
//! Served by [`serve`] at launch, after `atl_service_view::install` (atl-app-launcher). Core crate: `unsafe` nowhere.
#![forbid(unsafe_code)]
#![cfg(feature = "converter")]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate android_media_audio_common_types;
extern crate atl_audioflinger_stubs;
extern crate audioclient_types_aidl;
extern crate binder;

mod flinger;
mod ids;
mod policy;

/// The name AudioSystem looks AudioFlinger up by (`checkService`).
pub const FLINGER_NAME: &str = "media.audio_flinger";
/// The name AudioSystem looks AudioPolicyService up by.
pub const POLICY_NAME: &str = "media.audio_policy";
/// The output's sample rate (Hz). The PipeWire default graph rate (`default.clock.rate`), until A2 serves the negotiated rate.
pub const OUTPUT_RATE_HZ: i32 = 48_000;
/// The output's frame count: one server quantum, PipeWire's default (`default.clock.quantum`), until A2.
pub const OUTPUT_FRAMES: i64 = 1024;
/// The output's latency in ms: two quanta, `2 * 1000 * 1024 / 48000` = 42 (truncated). AOSP's calculateMinFrameCount divides it by one quantum in ms (21), so the minimum buffer is two quanta.
pub const OUTPUT_LATENCY_MS: i32 = 42;

/// Serves [`FLINGER_NAME`] and [`POLICY_NAME`] to this process. Refused before `atl_service_view::install`. The launcher skips the call under `--no-audio-service` (the audio gate's break).
pub fn serve() -> Result<(), String> {
    atl_service_view::serve(FLINGER_NAME, flinger::new_binder())?;
    atl_service_view::serve(POLICY_NAME, policy::new_binder())
}
