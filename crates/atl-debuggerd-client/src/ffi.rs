// SPDX-License-Identifier: MIT
//! The C ABI: libdebuggerd_client's exports. `generated::client` checks every signature here against
//! AOSP's header. No pointer is taken; `unsafe` is only the exported names.

use core::ffi::c_int;

/// AOSP `dump_backtrace_to_file_timeout`: `-1` when `debuggerd_trigger_dump` fails, as it does at the
/// tombstoned connect before any signal. AOSP also writes its error line and the target's wait channels
/// to `fd`; nothing is written here, as there is no dump to frame.
#[unsafe(no_mangle)]
pub extern "C" fn dump_backtrace_to_file_timeout(
    _tid: i32,
    _dump_type: u8,
    _timeout_secs: c_int,
    _fd: c_int,
) -> c_int {
    -1
}
