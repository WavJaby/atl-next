// SPDX-License-Identifier: MIT
//! The C ABI: libapexsupport's export, as AOSP's `apexsupport.cpp` makes it -- the library is loaded
//! with bionic's `android_dlopen_ext` in the namespace `android_get_exported_namespace` finds for the
//! APEX. Both are libc's provider's (atl-bionic). `generated::*` checks every signature here, export and
//! imports, against AOSP's and bionic's sources.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::ffi::CString;

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
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AApexSupport_loadLibrary(
    name: *const c_char,
    apex_name: *const c_char,
    flag: c_int,
) -> *mut c_void {
    // AOSP's contract: `_Nonnull` strings; null is refused instead of crashing
    if apex_name.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: a non-null, NUL-terminated string (the caller's contract)
    let apex = unsafe { CStr::from_ptr(apex_name) }.to_bytes();
    let ns_name =
        CString::new(crate::namespace_name(apex)).expect("a C string's bytes hold no NUL");
    // SAFETY: a NUL-terminated name
    let ns = unsafe { android_get_exported_namespace(ns_name.as_ptr()) };
    if ns.is_null() {
        return core::ptr::null_mut();
    }
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
    // SAFETY: the caller's contract is dlopen's: a NUL-terminated path, dlopen flags; `info` outlives
    // the call
    unsafe { android_dlopen_ext(name, flag, &info) }
}
