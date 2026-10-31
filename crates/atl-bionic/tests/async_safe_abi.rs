// SPDX-License-Identifier: MIT
//! libasync_safe's exports called as C calls them: variadic arguments and a `va_list` through the
//! platform calling convention. Expected strings are bionic's tests/async_safe_test.cpp cases; byte
//! counts are those of bionic's logd-less path, `tag: msg\n` to stderr.

use std::ffi::{CStr, c_char, c_int};

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

unsafe extern "C" {
    fn async_safe_format_buffer(buf: *mut c_char, size: usize, fmt: *const c_char, ...) -> c_int;
    fn async_safe_format_log(priority: c_int, tag: *const c_char, fmt: *const c_char, ...)
    -> c_int;
    fn async_safe_write_log(priority: c_int, tag: *const c_char, msg: *const c_char) -> c_int;
}

/// liblog's `ANDROID_LOG_INFO`.
const INFO: c_int = 4;

fn text(buf: &[c_char]) -> &str {
    // SAFETY: the formatter NUL-terminates a non-empty buffer
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .expect("the formats here print ASCII")
}

#[test]
fn variadic_calls() {
    let mut buf = [0 as c_char; 64];
    // SAFETY: each format's conversions match the arguments passed (C's printf contract)
    unsafe {
        let n = async_safe_format_buffer(
            buf.as_mut_ptr(),
            buf.len(),
            c"a%lld,%d,%d,%dz".as_ptr(),
            0x1000000000i64,
            6 as c_int,
            7 as c_int,
            8 as c_int,
        );
        assert_eq!((text(&buf), n), ("a68719476736,6,7,8z", 19));
        async_safe_format_buffer(
            buf.as_mut_ptr(),
            buf.len(),
            c"a%sb".as_ptr(),
            std::ptr::null::<c_char>(),
        );
        assert_eq!(text(&buf), "a(null)b");
        async_safe_format_buffer(
            buf.as_mut_ptr(),
            buf.len(),
            c"a%d,%pz".as_ptr(),
            5 as c_int,
            std::ptr::null::<u8>(),
        );
        assert_eq!(text(&buf), "a5,0x0z");
        async_safe_format_buffer(
            buf.as_mut_ptr(),
            buf.len(),
            c"A%s:%dZ".as_ptr(),
            c"hello".as_ptr(),
            1234 as c_int,
        );
        assert_eq!(text(&buf), "Ahello:1234Z");
        // the untruncated length; nothing written to a zero-size buffer
        assert_eq!(
            async_safe_format_buffer(std::ptr::null_mut(), 0, c"xxxx".as_ptr()),
            4
        );

        // "t: x=5\n" through the va_list variant
        assert_eq!(
            async_safe_format_log(INFO, c"t".as_ptr(), c"x=%d".as_ptr(), 5 as c_int),
            7
        );
        // "tag: msg\n"
        assert_eq!(
            async_safe_write_log(INFO, c"tag".as_ptr(), c"msg".as_ptr()),
            9
        );
        // null where bionic would crash is refused
        assert_eq!(
            async_safe_write_log(INFO, std::ptr::null(), c"msg".as_ptr()),
            -1
        );
    }
}
