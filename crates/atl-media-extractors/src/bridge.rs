// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/extractors.h: the service's AIBinder crosses as a +1 raw pointer; libbinder_rs owns it on the Rust side.

#[cxx::bridge(namespace = "atl::extractors")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/extractors.h");

        /// libbinder_ndk's AIBinder.
        type AIBinder;

        /// AOSP's `MediaExtractorService` constructed as instantiate() does, +1 ref; null when it could not be made. The constructor loads the extractor plugins.
        fn make_media_extractor_service() -> *mut AIBinder;
    }
}
