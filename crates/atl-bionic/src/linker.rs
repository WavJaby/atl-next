// SPDX-License-Identifier: MIT
//! Loading a library in a linker namespace: the one place every namespaced load goes through --
//! `android_dlopen_ext`, and through it libvndksupport's SP-HAL loads (its `android_load_sphal_library`
//! calls `android_dlopen_ext` as AOSP's does). Our libnativeloader's namespaces plug in here.
//!
//! This host has exactly one namespace: the process's glibc link map. Every exported namespace name
//! resolves to it, and a load in it is `dlopen(name, flags)`, so a namespace argument is accepted and
//! ignored. bionic returns null for a name it does not export; here every name is exported, as a load
//! in any of them lands in the same map.

use core::ffi::{CStr, c_char, c_int, c_void};

/// A linker namespace; only `DEFAULT` exists.
pub struct Namespace {
    /// bionic's name for the default namespace.
    _name: &'static str,
}

/// The one namespace: every load's, whether the caller names a namespace or not (not zero-sized: its
/// address is the handle C code holds).
pub static DEFAULT: Namespace = Namespace { _name: "(default)" };

unsafe extern "C" {
    // glibc's libc (>= 2.34 holds libdl)
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
}

/// bionic `android_get_exported_namespace`: the namespace exported as `name`.
pub fn exported(_name: &[u8]) -> &'static Namespace {
    &DEFAULT
}

/// Loads `name` in `ns` with `dlopen` flags `flags`: `dlopen`'s result, its error in `dlerror`.
///
/// # Safety
/// `dlopen`'s contract: `name` null (the main program) or a NUL-terminated path.
pub unsafe fn open(name: *const c_char, flags: c_int, _ns: &Namespace) -> *mut c_void {
    // SAFETY: the caller's contract
    unsafe { dlopen(name, flags) }
}

/// Fails a load with `detail` for `dlerror`: glibc owns `dlerror` and has no call to set it, so glibc's
/// own `dlopen` refuses a call whose mode has no binding (`RTLD_LAZY` / `RTLD_NOW`) with `detail` as
/// the object name -- checked before any file is looked up. `dlerror` then reads `<detail>: invalid
/// mode for dlopen(): Invalid argument` (glibc `dl-open.c`, `dlerror.c`).
pub fn fail(detail: &CStr) {
    // SAFETY: a NUL-terminated string; mode 0 is refused before glibc reads the name as a path
    let handle = unsafe { dlopen(detail.as_ptr(), 0) };
    debug_assert!(handle.is_null(), "glibc loaded with mode 0");
}
