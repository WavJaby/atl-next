// SPDX-License-Identifier: MIT
//! `__system_property_serial` of a `prop_info` outside the area, as libcutils trace-dev.inc passes one: a zeroed 96-byte,
//! 8-aligned stand-in (`empty_pi`). Expected values from bionic `SystemProperties::Serial`, which reads `pi->serial`
//! (prop_info.h: the first field, an `atomic_uint_least32_t`) whether or not an area is mapped: 0 for the stand-in, what
//! was stored for another. One test: no area is mapped in this process.

use std::sync::atomic::{AtomicU32, Ordering};

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

#[repr(C)]
struct PropInfo {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn __system_property_serial(pi: *const PropInfo) -> u32;
    fn __system_property_find(name: *const std::ffi::c_char) -> *const PropInfo;
}

/// trace-dev.inc's `alignas(uint64_t) static char empty_pi[96]`.
#[repr(C, align(8))]
struct EmptyPi([u8; 96]);

#[test]
fn serial_of_a_callers_prop_info() {
    // SAFETY: no other test in this binary reads the environment
    unsafe {
        std::env::set_var(
            atl_android_paths::PROPERTY_AREA_ENV,
            "/nonexistent/atl-bionic-serial-test",
        )
    };
    let empty = EmptyPi([0; 96]);
    // SAFETY: bionic's contracts: a NUL-terminated name; a prop_info-sized, aligned record
    unsafe {
        assert!(__system_property_find(c"debug.atrace.tags.enableflags".as_ptr()).is_null());
        assert_eq!(__system_property_serial((&raw const empty).cast()), 0);
    }
    let mut other = EmptyPi([0; 96]);
    // SAFETY: the record's first 4 bytes, aligned (align 8)
    unsafe { &*(&raw mut other.0).cast::<AtomicU32>() }.store(0x0200_0004, Ordering::Release);
    // SAFETY: as above
    let serial = unsafe { __system_property_serial((&raw const other).cast()) };
    assert_eq!(serial, 0x0200_0004);
    // null: refused with bionic's error value (-1 as uint32_t), where bionic would crash
    // SAFETY: null is checked before any read
    let serial = unsafe { __system_property_serial(std::ptr::null()) };
    assert_eq!(serial, u32::MAX);
}
