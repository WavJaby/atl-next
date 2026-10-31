// SPDX-License-Identifier: MIT
//! The export as C calls it: every dump request fails as AOSP's does without tombstoned.

use std::ffi::c_int;
use std::os::fd::AsRawFd;

// the export lives in the crate; nothing else of it is named here
use atl_debuggerd_client as _;

unsafe extern "C" {
    fn dump_backtrace_to_file_timeout(
        tid: i32,
        dump_type: u8,
        timeout_secs: c_int,
        fd: c_int,
    ) -> c_int;
}

#[test]
fn every_request_fails_as_without_tombstoned() {
    // AOSP debuggerd_client.cpp: `debuggerd_trigger_dump(...) ? 0 : -1`, false at the tombstoned connect
    const FAILED: c_int = -1;
    let out = std::fs::File::options()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null");
    // dump_type.h: kDebuggerdNativeBacktrace .. kDebuggerdTombstoneProto = 0..=4; android_os_Debug.cpp
    // passes timeouts in seconds, 0 = wait forever
    for dump_type in 0..=4 {
        for timeout in [0, 5] {
            // SAFETY: plain integers; the descriptor is open
            let r = unsafe {
                dump_backtrace_to_file_timeout(
                    std::process::id() as i32,
                    dump_type,
                    timeout,
                    out.as_raw_fd(),
                )
            };
            assert_eq!(r, FAILED, "type {dump_type}, timeout {timeout}");
        }
    }
}
