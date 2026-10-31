// SPDX-License-Identifier: MIT
//! The exports as C calls them: AOSP's `sphal` load through `android_dlopen_ext` returns dlopen's
//! results, and `dlclose`'s.

use std::ffi::{c_char, c_int, c_void};

// the exports live in the crate, the bionic API they call in atl-bionic; nothing else of either is named
use atl_bionic as _;
use atl_vndksupport as _;

unsafe extern "C" {
    fn android_load_sphal_library(name: *const c_char, flag: c_int) -> *mut c_void;
    fn android_unload_sphal_library(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

/// glibc's `RTLD_NOW` (2) and `RTLD_LOCAL` (0), the flags Gralloc5.cpp passes; the same on every glibc
/// arch (bits/dlfcn.h).
const FLAGS: c_int = 2;

#[test]
fn loads_and_unloads_in_the_one_namespace() {
    // SAFETY: NUL-terminated names, dlopen flags, a handle from the load
    unsafe {
        // glibc's own libc is always loadable
        let handle = android_load_sphal_library(c"libc.so.6".as_ptr(), FLAGS);
        assert!(!handle.is_null());
        assert_eq!(android_unload_sphal_library(handle), 0);
        // what dlopen cannot find, it reports as null, its error in dlerror
        dlerror();
        let none = android_load_sphal_library(c"libatl-no-such-library.so".as_ptr(), FLAGS);
        assert!(none.is_null());
        assert!(!dlerror().is_null());
    }
}
