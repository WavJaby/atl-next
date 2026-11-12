// SPDX-License-Identifier: MIT
//! `__system_properties_init` as bionic defines it (system_property_api.cpp, `SystemProperties::Init`):
//! 0 once the area is mapped, -1 when it cannot be, a failure not kept (a later call retries), a success
//! kept (later calls return 0), errno untouched. Own binary: the store is mapped once per process.

use std::ffi::{CStr, c_char, c_int};

use atl_bionic::image::Image;

unsafe extern "C" {
    fn __system_properties_init() -> c_int;
    fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
}

/// bionic's `PROP_VALUE_MAX`.
const VALUE_MAX: usize = 92;

fn get(name: &CStr) -> (c_int, String) {
    let mut value = [0 as c_char; VALUE_MAX];
    // SAFETY: a C string and a PROP_VALUE_MAX buffer
    let len = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    // SAFETY: bionic's contract: the buffer is NUL-terminated
    let v = unsafe { CStr::from_ptr(value.as_ptr()) };
    (len, v.to_string_lossy().into_owned())
}

#[test]
fn init_fails_without_an_area_then_maps_once_it_exists() {
    let dir = std::env::temp_dir().join(format!("atl-bionic-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("properties");
    let _ = std::fs::remove_file(&file);
    // SAFETY: the only test in this binary; nothing else reads the environment concurrently
    unsafe { std::env::set_var(atl_android_paths::PROPERTY_AREA_ENV, &file) };

    // no file: -1, errno untouched; readers see every property unset
    errno::set_errno(errno::Errno(1234));
    // SAFETY: no arguments
    assert_eq!(unsafe { __system_properties_init() }, -1);
    assert_eq!(errno::errno().0, 1234);
    assert_eq!(get(c"ro.x"), (0, String::new()));

    // the file appears: a lazy reader keeps its failed attempt, an explicit init retries and maps it
    let mut image = Image::new();
    image.add("ro.x", "yes").unwrap();
    std::fs::write(&file, image.into_bytes()).unwrap();
    assert_eq!(get(c"ro.x"), (0, String::new()));
    // SAFETY: no arguments
    assert_eq!(unsafe { __system_properties_init() }, 0);
    assert_eq!(get(c"ro.x"), (3, "yes".to_owned()));

    // idempotent: the mapping stays even once the file is gone
    std::fs::remove_file(&file).unwrap();
    // SAFETY: no arguments
    assert_eq!(unsafe { __system_properties_init() }, 0);
    assert_eq!(get(c"ro.x"), (3, "yes".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}
