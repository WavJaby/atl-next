// SPDX-License-Identifier: MIT
//! bionic's malloc introspection as libmemunreachable calls it (its bionic.h declarations). Expected values: bionic's allocators
//! without heap-walk support -- malloc_common.cpp `__sanitizer_malloc_iterate` (HWASan: returns 0, calls nothing; disable and
//! enable empty) and malloc_common_dynamic.cpp `malloc_backtrace` without malloc debug (returns 0) -- never this crate's.

use std::ffi::{c_int, c_void};

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

type Callback = unsafe extern "C" fn(base: usize, size: usize, arg: *mut c_void);

unsafe extern "C" {
    fn malloc_disable();
    fn malloc_enable();
    fn malloc_iterate(
        base: usize,
        size: usize,
        callback: Option<Callback>,
        arg: *mut c_void,
    ) -> c_int;
    fn malloc_backtrace(pointer: *mut c_void, frames: *mut usize, frame_count: usize) -> isize;
}

unsafe extern "C" fn count(_base: usize, _size: usize, arg: *mut c_void) {
    // SAFETY: arg is the test's counter
    unsafe { *arg.cast::<usize>() += 1 };
}

#[test]
fn iterate_reports_no_chunk() {
    // a live allocation inside the walked range: bionic's supported allocators would report it
    let chunk = Box::new([0u8; 64]);
    let addr = chunk.as_ptr() as usize;
    let mut calls = 0usize;
    // SAFETY: libmemunreachable's sequence: disable, iterate a range, enable; the callback reads only its counter
    let rc = unsafe {
        malloc_disable();
        let rc = malloc_iterate(addr & !0xfff, 0x10000, Some(count), (&raw mut calls).cast());
        malloc_enable();
        rc
    };
    assert_eq!(rc, 0);
    assert_eq!(calls, 0, "no heap walk on glibc");
    drop(chunk);
    // allocation still works after enable
    assert_eq!(vec![1u8; 4096].len(), 4096);
}

#[test]
fn backtrace_has_no_frames() {
    let chunk = Box::new(0u64);
    let mut frames = [0usize; 16];
    // SAFETY: a live allocation and a buffer of frame_count entries
    let n = unsafe {
        malloc_backtrace(
            (&raw const *chunk).cast_mut().cast(),
            frames.as_mut_ptr(),
            frames.len(),
        )
    };
    assert_eq!(n, 0);
    assert_eq!(frames, [0; 16], "no frame written");
}
