// SPDX-License-Identifier: MIT
//! fdsan's error level (bionic `fdsan.cpp`). glibc's `close` checks no owner tag and nothing here tags a
//! descriptor, so no level ever reports: the same as bionic for a process whose descriptors are all
//! untagged. Only the level is kept, for what the closure calls (libsfplugin_ccodec C2NodeImpl).

use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

use crate::generated::fdsan::ANDROID_FDSAN_ERROR_LEVEL_FATAL;

/// bionic's libc init (`__libc_init_fdsan`) sets `ANDROID_FDSAN_ERROR_LEVEL_FATAL` unless `debug.fdsan`
/// says otherwise; the property is not read here, as no check reads the level -- only the first
/// exchange's result can differ.
static LEVEL: AtomicU32 = AtomicU32::new(ANDROID_FDSAN_ERROR_LEVEL_FATAL);

/// bionic `android_fdsan_set_error_level`: the previous level (an atomic exchange there too). bionic's
/// vfork-child exception does not arise: nothing calls it between `vfork` and `exec` here.
pub fn set_error_level(level: u32) -> u32 {
    LEVEL.swap(level, SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_returns_previous() {
        // enumerator values from bionic's fdsan.h: DISABLED, WARN_ONCE, WARN_ALWAYS, FATAL = 0..3 in order
        assert_eq!(set_error_level(2), 3);
        assert_eq!(set_error_level(1), 2);
        assert_eq!(set_error_level(0), 1);
    }
}
