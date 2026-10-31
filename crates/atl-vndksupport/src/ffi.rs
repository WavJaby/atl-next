// SPDX-License-Identifier: MIT
//! The C ABI: libvndksupport's exports, each the C library call AOSP's single-namespace path makes.
//! `generated::linker` checks every signature here against AOSP's header. Pointers pass through
//! unchanged: `dlopen` takes a null name (the main program) and reports a bad one itself.

use core::ffi::{c_char, c_int, c_void};

unsafe extern "C" {
    // glibc's libc (>= 2.34 holds libdl)
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_load_sphal_library(
    name: *const c_char,
    flag: c_int,
) -> *mut c_void {
    // SAFETY: the caller's contract is dlopen's: null or a NUL-terminated path, dlopen flags
    unsafe { dlopen(name, flag) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_unload_sphal_library(handle: *mut c_void) -> c_int {
    // SAFETY: the caller's contract is dlclose's: a handle android_load_sphal_library returned
    unsafe { dlclose(handle) }
}
