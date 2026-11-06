// SPDX-License-Identifier: MIT
//! The cxx bridge to wrap/view.h: raw AIBinder pointers cross, libbinder_rs owns them on the Rust side.

#[cxx::bridge(namespace = "atl::view")]
pub(crate) mod ffi {
    unsafe extern "C++" {
        include!("wrap/view.h");

        /// libbinder_ndk's AIBinder.
        type AIBinder;

        /// libbinder's default service manager := `view`. Must run before any `defaultServiceManager()` (libbinder aborts otherwise).
        unsafe fn install(view: *mut AIBinder);

        /// The real servicemanager, +1 ref; null without one.
        fn context_object() -> *mut AIBinder;
    }
}
