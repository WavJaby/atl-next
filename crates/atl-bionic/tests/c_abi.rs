// SPDX-License-Identifier: MIT
//! The exported C ABI as C code calls it, over an area file named by `ATL_PROP_AREA`. One test: the area
//! is mapped once per process.

use std::ffi::{CStr, c_char, c_int, c_void};

use atl_bionic::image::Image;

#[repr(C)]
struct PropInfo {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn __system_property_set(name: *const c_char, value: *const c_char) -> c_int;
    fn __system_property_find(name: *const c_char) -> *const PropInfo;
    fn __system_property_read_callback(
        pi: *const PropInfo,
        cb: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32)>,
        cookie: *mut c_void,
    );
    fn __system_property_foreach(
        cb: Option<unsafe extern "C" fn(*const PropInfo, *mut c_void)>,
        cookie: *mut c_void,
    ) -> c_int;
    fn __system_property_read(pi: *const PropInfo, name: *mut c_char, value: *mut c_char) -> c_int;
    fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
    fn __system_property_area_serial() -> u32;
    fn __system_property_serial(pi: *const PropInfo) -> u32;
    fn __system_property_wait(
        pi: *const PropInfo,
        old_serial: u32,
        new_serial: *mut u32,
        relative_timeout: *const c_void,
    ) -> bool;
    fn android_get_device_api_level() -> c_int;
    fn android_get_application_target_sdk_version() -> c_int;
    fn android_set_application_target_sdk_version(target: c_int);
}

/// bionic's `PROP_VALUE_MAX` / `PROP_NAME_MAX` (sys/system_properties.h), the buffer sizes C passes.
const VALUE_MAX: usize = 92;
const NAME_MAX: usize = 32;

unsafe extern "C" fn collect(
    cookie: *mut c_void,
    name: *const c_char,
    value: *const c_char,
    serial: u32,
) {
    // SAFETY: cookie is the Vec below; name and value are C strings for the call
    unsafe {
        let out = &mut *cookie.cast::<Vec<(String, String, u32)>>();
        let s = |p: *const c_char| CStr::from_ptr(p).to_string_lossy().into_owned();
        out.push((s(name), s(value), serial));
    }
}

unsafe extern "C" fn count(_pi: *const PropInfo, cookie: *mut c_void) {
    // SAFETY: cookie is the counter below
    unsafe { *cookie.cast::<usize>() += 1 };
}

#[test]
fn c_callers_see_the_area() {
    let long = "l".repeat(150);
    let mut image = Image::new();
    image.add("ro.build.version.sdk", "35").unwrap();
    image.add("ro.long", &long).unwrap();
    image.add("debug.x", "on").unwrap();
    let dir = std::env::temp_dir().join(format!("atl-bionic-cabi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("properties");
    std::fs::write(&file, image.into_bytes()).unwrap();
    // SAFETY: the only test in this binary; nothing else reads the environment concurrently
    unsafe { std::env::set_var("ATL_PROP_AREA", &file) };

    // SAFETY: every call follows bionic's contracts: C strings, PROP_VALUE_MAX / PROP_NAME_MAX buffers
    unsafe {
        let mut value = [0 as c_char; VALUE_MAX];
        assert_eq!(
            __system_property_get(c"ro.build.version.sdk".as_ptr(), value.as_mut_ptr()),
            2
        );
        assert_eq!(CStr::from_ptr(value.as_ptr()), c"35");
        assert_eq!(
            __system_property_get(c"absent".as_ptr(), value.as_mut_ptr()),
            0
        );
        assert_eq!(value[0], 0);
        assert_eq!(android_get_device_api_level(), 35);

        let pi = __system_property_find(c"ro.long".as_ptr());
        assert!(!pi.is_null());
        // `find` hands out stable pointers
        assert_eq!(pi, __system_property_find(c"ro.long".as_ptr()));
        let mut seen: Vec<(String, String, u32)> = Vec::new();
        __system_property_read_callback(pi, Some(collect), (&raw mut seen).cast());
        assert_eq!(seen[0].0, "ro.long");
        assert_eq!(seen[0].1, long);
        // legacy readers get bionic's message and its length
        let mut name = [0 as c_char; NAME_MAX];
        let len = __system_property_read(pi, name.as_mut_ptr(), value.as_mut_ptr());
        let msg = c"Must use __system_property_read_callback() to read";
        assert_eq!(len as usize, msg.count_bytes());
        assert_eq!(CStr::from_ptr(value.as_ptr()), msg);
        assert_eq!(CStr::from_ptr(name.as_ptr()), c"ro.long");

        let x = __system_property_find(c"debug.x".as_ptr());
        assert_eq!(__system_property_serial(x), 2 << 24);
        seen.clear();
        __system_property_read_callback(x, Some(collect), (&raw mut seen).cast());
        assert_eq!(seen, [("debug.x".to_owned(), "on".to_owned(), 2 << 24)]);

        let mut n = 0usize;
        assert_eq!(
            __system_property_foreach(Some(count), (&raw mut n).cast()),
            0
        );
        assert_eq!(n, 3);
        assert_eq!(__system_property_area_serial(), 3);

        // a C timeout: rustix's Timespec is two i64s, the libc's on every 64-bit host
        let ms = rustix::thread::futex::Timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        };
        let ms = (&raw const ms).cast::<c_void>();
        let mut new = 0u32;
        let area = core::ptr::null();
        assert!(!__system_property_wait(area, 3, &raw mut new, ms));
        assert!(!__system_property_wait(x, 2 << 24, &raw mut new, ms));
        assert_eq!(new, 0);
        // the serial already moved on: true and the current one, at once
        assert!(__system_property_wait(area, 2, &raw mut new, ms));
        assert_eq!(new, 3);
        // refused where bionic would crash
        assert!(!__system_property_wait(x, 0, core::ptr::null_mut(), ms));

        // no property service yet: bionic's failure when its socket is missing
        assert_eq!(
            __system_property_set(c"debug.x".as_ptr(), c"off".as_ptr()),
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::NotFound
        );

        // unset: bionic's __ANDROID_API_FUTURE__ (api-level.h), the level platform code is built for
        assert_eq!(android_get_application_target_sdk_version(), 10000);
        android_set_application_target_sdk_version(34);
        assert_eq!(android_get_application_target_sdk_version(), 34);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
