// SPDX-License-Identifier: MIT
//! The export as C calls it: AOSP's load from an APEX namespace returns dlopen's results -- a library
//! the process can load, else null with the error in `dlerror` -- and null for a null APEX name.

use std::ffi::{c_char, c_int, c_void};

// the export lives in the crate, the bionic API it calls in atl-bionic; nothing else of either is named
use atl_apexsupport as _;
use atl_bionic as _;

unsafe extern "C" {
    fn AApexSupport_loadLibrary(
        name: *const c_char,
        apex: *const c_char,
        flag: c_int,
    ) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn dlclose(handle: *mut c_void) -> c_int;
}

/// glibc's `RTLD_NOW` (2) and `RTLD_LOCAL` (0), the flags libbinder's openDeclaredPassthroughHal
/// callers pass; the same on every glibc arch (bits/dlfcn.h).
const FLAGS: c_int = 2;

#[test]
fn loads_through_the_apex_namespace() {
    // SAFETY: NUL-terminated names, dlopen flags, a handle from the load
    unsafe {
        // glibc's own libc is always loadable, from any namespace name
        let handle =
            AApexSupport_loadLibrary(c"libc.so.6".as_ptr(), c"com.android.foo".as_ptr(), FLAGS);
        assert!(!handle.is_null());
        assert_eq!(dlclose(handle), 0);
        // an APEX HAL library this host does not install: null, the error in dlerror
        dlerror();
        let none = AApexSupport_loadLibrary(
            c"android.hardware.foo.IFoo.default.so".as_ptr(),
            c"com.android.hardware.foo".as_ptr(),
            FLAGS,
        );
        assert!(none.is_null());
        assert!(!dlerror().is_null());
        assert!(
            AApexSupport_loadLibrary(c"libc.so.6".as_ptr(), core::ptr::null(), FLAGS).is_null()
        );
    }
}
