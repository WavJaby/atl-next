// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/mix.h and wrap/engine.h. Handles and scalars cross; raw pointers cross only for memory the caller maps (lib.rs `Mapping`) and the mixer's quantum buffer (lib.rs `Mix`), and each `unsafe` call says so.
//! The mixer's wrappers forward AOSP calls and nothing else: the quantum loop and the gains are Rust's (lib.rs).
//! Engine answers: a status (AOSP's status_t, 0 = NO_ERROR) and an out value; list answers are AIDL parcel bytes (engine.rs).

#[cxx::bridge(namespace = "atl::audio")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/mix.h");
        include!("wrap/engine.h");

        /// AOSP's server side of one track (`Track` in wrap/mix.h).
        type Track;
        /// AOSP's AudioMixerBase at the output's quantum and rate (`Mix` in wrap/mix.h).
        type Mix;

        /// sizeof(audio_track_cblk_t), the size of the control block at the start of a track's region.
        fn cblk_bytes() -> usize;
        /// # Safety
        /// `cblk` addresses `cblk_bytes()` bytes and `buffer` `frames * frame_bytes` bytes, both mapped read-write (static: read-only is enough for the buffer) and kept mapped for the life of the returned track.
        /// `format` (audio_format_t) is the frames' format; `frame_bytes` a whole number of its samples.
        unsafe fn new_track(
            cblk: *mut u8,
            buffer: *mut u8,
            format: u32,
            frames: usize,
            frame_bytes: usize,
            is_static: bool,
            rate: u32,
        ) -> UniquePtr<Track>;
        fn new_mix(quantum_frames: usize, rate: u32) -> UniquePtr<Mix>;
        /// # Safety
        /// `main` addresses one quantum of stereo float frames (`quantum_frames * 2` floats) that is not moved, resized or written by anyone else while the mixer holds the track.
        unsafe fn mix_add(
            mix: Pin<&mut Mix>,
            track: Pin<&mut Track>,
            name: i32,
            session: i32,
            channel_mask: u32,
            rate: u32,
            main: *mut f32,
        ) -> bool;
        fn mix_exists(mix: Pin<&mut Mix>, name: i32) -> bool;
        fn mix_destroy(mix: Pin<&mut Mix>, name: i32);
        fn mix_enable(mix: Pin<&mut Mix>, name: i32);
        fn mix_disable(mix: Pin<&mut Mix>, name: i32);
        fn mix_set_volume(mix: Pin<&mut Mix>, name: i32, left: f32, right: f32);
        fn mix_process(mix: Pin<&mut Mix>);
        fn track_start(track: Pin<&mut Track>);
        fn track_flush_if_needed(track: Pin<&mut Track>);
        fn track_frames_ready(track: Pin<&mut Track>) -> u64;
        fn track_frames_flushed(track: Pin<&mut Track>) -> i64;
        fn track_frames_released(track: Pin<&mut Track>) -> i64;
        fn track_client_gain(track: Pin<&mut Track>, left: &mut f32, right: &mut f32);
        fn track_shaper_gain(track: Pin<&mut Track>, frame: i64) -> f32;
        fn track_draining(track: Pin<&mut Track>) -> bool;
        fn track_set_draining(track: Pin<&mut Track>, draining: bool);
        fn track_apply_volume_shaper(
            track: Pin<&mut Track>,
            configuration: &[u8],
            operation: &[u8],
        ) -> i32;
        fn track_volume_shaper_state(
            track: Pin<&mut Track>,
            id: i32,
            volume: &mut f32,
            x_offset: &mut f32,
        ) -> bool;
        fn take_track_frames() -> u64;
        fn take_underruns() -> u64;
        fn take_underrun_frames() -> u64;
        fn track_underruns(track: &Track) -> u32;

        /// AOSP's policy engine with its default config (`PolicyEngine` in wrap/engine.h).
        type PolicyEngine;
        fn new_policy_engine() -> UniquePtr<PolicyEngine>;
        fn policy_engine_status(policy: &PolicyEngine) -> i32;
        fn list_product_strategies(policy: &PolicyEngine, out: &mut Vec<u8>) -> i32;
        fn product_strategy_for_attributes(
            policy: &PolicyEngine,
            attributes: &[u8],
            fallback_on_default: bool,
            out: &mut i32,
        ) -> i32;
        fn product_strategy_for_stream(policy: &PolicyEngine, stream: i32, out: &mut i32) -> i32;
        fn list_volume_groups(policy: &PolicyEngine, out: &mut Vec<u8>) -> i32;
        fn volume_group_for_attributes(
            policy: &PolicyEngine,
            attributes: &[u8],
            fallback_on_default: bool,
            out: &mut i32,
        ) -> i32;
    }
}
