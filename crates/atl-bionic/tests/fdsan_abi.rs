// SPDX-License-Identifier: MIT
//! fdsan as libbase's `unique_fd` and libcutils' `native_handle` call it, on real descriptors. Expected values from bionic's
//! fdsan.cpp: tags `type << 56 | value & (2^56 - 1)` (fdsan.h: `ANDROID_FDSAN_OWNER_TYPE_UNIQUE_FD` = 3), `close_with_tag`
//! returns `close`'s result, an unknown fd is unowned. One test: the error level is process-wide.

use std::ffi::c_int;
use std::os::fd::IntoRawFd;

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

unsafe extern "C" {
    fn android_fdsan_create_owner_tag(owner_type: u32, tag: u64) -> u64;
    fn android_fdsan_exchange_owner_tag(fd: c_int, expected_tag: u64, new_tag: u64);
    fn android_fdsan_close_with_tag(fd: c_int, tag: u64) -> c_int;
    fn android_fdsan_get_owner_tag(fd: c_int) -> u64;
    fn android_fdsan_set_error_level(level: u32) -> u32;
}

const UNIQUE_FD: u32 = 3;
/// Linux `EBADF`.
const EBADF: i32 = 9;

fn open() -> c_int {
    std::fs::File::open("/dev/null")
        .expect("/dev/null")
        .into_raw_fd()
}

fn is_open(fd: c_int) -> bool {
    std::path::Path::new(&format!("/proc/self/fd/{fd}")).exists()
}

#[test]
fn unique_fd_lifecycle() {
    // SAFETY: fdsan's C contracts; every descriptor is this test's own
    unsafe {
        // the start level is DISABLED (this crate's deviation, src/fdsan.rs): mismatches below only report nothing
        assert_eq!(android_fdsan_set_error_level(0), 0);

        let fd = open();
        let owner = 0x7fff_1234_5670_u64;
        let tag = android_fdsan_create_owner_tag(UNIQUE_FD, owner);
        assert_eq!(tag, 0x0300_7fff_1234_5670);
        assert_eq!(android_fdsan_create_owner_tag(UNIQUE_FD, 0), 0);
        assert_eq!(android_fdsan_get_owner_tag(fd), 0);

        // unique_fd::reset: exchange 0 -> its tag; a moved unique_fd: tag -> new tag
        android_fdsan_exchange_owner_tag(fd, 0, tag);
        assert_eq!(android_fdsan_get_owner_tag(fd), tag);
        let moved = android_fdsan_create_owner_tag(UNIQUE_FD, owner + 8);
        android_fdsan_exchange_owner_tag(fd, tag, moved);
        assert_eq!(android_fdsan_get_owner_tag(fd), moved);

        // close by its owner: closed, tag cleared, close's result
        assert_eq!(android_fdsan_close_with_tag(fd, moved), 0);
        assert!(!is_open(fd));
        assert_eq!(android_fdsan_get_owner_tag(fd), 0);
        // double close: close's -1 / EBADF (a report, not shown at DISABLED)
        assert_eq!(android_fdsan_close_with_tag(fd, moved), -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(EBADF));

        // a close by the wrong owner still closes (disabled: no abort)
        let fd = open();
        android_fdsan_exchange_owner_tag(fd, 0, tag);
        assert_eq!(android_fdsan_close_with_tag(fd, 0), 0);
        assert!(!is_open(fd));

        // native_handle: untagged descriptors close with tag 0
        let fd = open();
        assert_eq!(android_fdsan_close_with_tag(fd, 0), 0);
        assert!(!is_open(fd));
        assert_eq!(android_fdsan_get_owner_tag(-1), 0);
    }
}
