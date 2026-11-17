// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/mix.h. Handles and scalars cross; raw pointers cross only for memory the caller maps (lib.rs `Mapping`), and each `unsafe` call says so.

#[cxx::bridge(namespace = "atl::audio")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/mix.h");

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
        fn mix_add(
            mix: Pin<&mut Mix>,
            track: Pin<&mut Track>,
            name: i32,
            session: i32,
            channel_mask: u32,
            rate: u32,
        ) -> bool;
        fn mix_remove(mix: Pin<&mut Mix>, name: i32);
        fn mix_enable(mix: Pin<&mut Mix>, track: Pin<&mut Track>, name: i32, on: bool);
        fn mix_render(mix: Pin<&mut Mix>, out: &mut [f32]);
        fn take_track_frames() -> u64;
        fn muted_tracks() -> u32;
    }
}
