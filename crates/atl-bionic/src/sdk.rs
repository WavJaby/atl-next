// SPDX-License-Identifier: MIT
//! API levels: the device's and the application's target SDK.

use std::sync::atomic::{AtomicI32, Ordering::Relaxed};

use crate::generated::api_level::__ANDROID_API_FUTURE__;
use crate::props::VALUE_MAX;

/// The property bionic's `android_get_device_api_level` reads.
const DEVICE_LEVEL_PROPERTY: &[u8] = b"ro.build.version.sdk";

/// bionic's linker (`linker_sdk_versions.cpp`) starts at the level it was built for, which for platform
/// code is `__ANDROID_API_FUTURE__`; the launcher sets the app's (owner Q5).
static TARGET_SDK: AtomicI32 = AtomicI32::new(__ANDROID_API_FUTURE__);

/// bionic `set_application_target_sdk_version`: 0 means the platform level.
pub fn set_target_sdk(target: i32) {
    let target = if target == 0 {
        __ANDROID_API_FUTURE__
    } else {
        target
    };
    TARGET_SDK.store(target, Relaxed);
}

pub fn target_sdk() -> i32 {
    TARGET_SDK.load(Relaxed)
}

/// bionic `android_get_device_api_level` (`bits/get_device_api_level_inlines.h`): `atoi` of
/// `ro.build.version.sdk`, -1 when unset or not positive.
pub fn device_api_level() -> i32 {
    let mut value = [0u8; VALUE_MAX];
    let len = crate::store::get(DEVICE_LEVEL_PROPERTY, &mut value);
    if len < 1 {
        return -1;
    }
    let level = atoi(&value[..len]);
    if level > 0 { level } else { -1 }
}

/// C `atoi`: leading white space, an optional sign, decimal digits up to the first non-digit; values
/// past `int` saturate (C leaves them undefined).
fn atoi(s: &[u8]) -> i32 {
    let s = s
        .iter()
        .position(|b| !b" \t\n\x0b\x0c\r".contains(b))
        .map_or(&s[s.len()..], |i| &s[i..]);
    let (negative, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let magnitude = digits
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0i64, |n, d| {
            (n * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX) + 1)
        });
    let v = if negative { -magnitude } else { magnitude };
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoi_as_c() {
        assert_eq!(atoi(b"36"), 36);
        assert_eq!(atoi(b"  35abc"), 35);
        assert_eq!(atoi(b"-7"), -7);
        assert_eq!(atoi(b"+8"), 8);
        assert_eq!(atoi(b"x1"), 0);
        assert_eq!(atoi(b""), 0);
        assert_eq!(atoi(b"99999999999"), i32::MAX);
    }

    #[test]
    fn target_sdk_zero_is_the_platform_level() {
        set_target_sdk(30);
        assert_eq!(target_sdk(), 30);
        set_target_sdk(0);
        assert_eq!(target_sdk(), __ANDROID_API_FUTURE__);
    }
}
