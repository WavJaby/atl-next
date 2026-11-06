// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/store.h: the store's AIBinder crosses as a +1 raw pointer; libbinder_rs owns it on the Rust side.

#[cxx::bridge(namespace = "atl::codec")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/store.h");

        /// libbinder_ndk's AIBinder.
        type AIBinder;

        /// AOSP's `codec2::aidl::utils::ComponentStore` over the platform store, +1 ref; null when it could not be made.
        fn make_component_store() -> *mut AIBinder;
    }
}
