// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/rms.h: the service's AIBinder crosses as a +1 raw pointer; libbinder_rs owns it on the Rust side.

#[cxx::bridge(namespace = "atl::media")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/rms.h");

        /// libbinder_ndk's AIBinder.
        type AIBinder;

        /// AOSP's `ResourceManagerService::Create()`, +1 ref; null when it could not be made.
        fn make_resource_manager() -> *mut AIBinder;
    }
}
