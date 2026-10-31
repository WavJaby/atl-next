// SPDX-License-Identifier: MIT
//! The C ABI: libvndksupport's exports, as AOSP's `linker.cpp` makes them -- an SP-HAL library is
//! loaded with bionic's `android_dlopen_ext` in the namespace `android_get_exported_namespace` finds.
//! Both are libc's provider's (atl-bionic), whose `linker` is the one place namespaced loads go.
//! `generated::*` checks every signature here, exports and imports, against AOSP's and bionic's sources.
//! Pointers pass through unchanged: `dlopen` takes a null name (the main program) and reports a bad one
//! itself.

use core::ffi::{c_char, c_int, c_void};

use crate::generated::dlext::{ANDROID_DLEXT_USE_NAMESPACE, android_dlextinfo};
use crate::generated::libdl_android::android_namespace_t;

unsafe extern "C" {
    // atl-bionic, providing libc (bionic's libdl / libdl_android here)
    pub(crate) fn android_dlopen_ext(
        filename: *const c_char,
        flags: c_int,
        info: *const android_dlextinfo,
    ) -> *mut c_void;
    pub(crate) fn android_get_exported_namespace(name: *const c_char) -> *mut android_namespace_t;
    // glibc's libc (>= 2.34 holds libdl)
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_load_sphal_library(
    name: *const c_char,
    flag: c_int,
) -> *mut c_void {
    // AOSP tries `sphal`, `vendor`, `default` and falls back to plain dlopen when none is exported;
    // atl-bionic exports every name, so `sphal` is always found (were it not, android_dlopen_ext would
    // refuse the null namespace with bionic's error)
    // SAFETY: a NUL-terminated name
    let ns = unsafe { android_get_exported_namespace(c"sphal".as_ptr()) };
    // AOSP's designated initializer: every other field zero
    let info = android_dlextinfo {
        flags: u64::from(ANDROID_DLEXT_USE_NAMESPACE),
        reserved_addr: core::ptr::null_mut(),
        reserved_size: 0,
        relro_fd: 0,
        library_fd: 0,
        library_fd_offset: 0,
        library_namespace: ns.cast(),
    };
    // SAFETY: the caller's contract is dlopen's: null or a NUL-terminated path, dlopen flags; `info`
    // outlives the call
    unsafe { android_dlopen_ext(name, flag, &info) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_unload_sphal_library(handle: *mut c_void) -> c_int {
    // SAFETY: the caller's contract is dlclose's: a handle android_load_sphal_library returned
    unsafe { dlclose(handle) }
}
