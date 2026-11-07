// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/player.h: the service's AIBinder crosses as a +1 raw pointer; libbinder_rs owns it on the Rust side.

#[cxx::bridge(namespace = "atl::player")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/player.h");

        /// libbinder_ndk's AIBinder.
        type AIBinder;

        /// AOSP's `MediaPlayerService` constructed as instantiate() does, +1 ref; null when it could not be made.
        fn make_media_player() -> *mut AIBinder;
    }
}
