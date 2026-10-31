// SPDX-License-Identifier: MIT
//! The C ABI (bridge 1 class b): thin shells that turn raw pointers into safe values at once and call
//! safe code. `generated::*` checks every signature here against bionic's headers.
//!
//! Pointer contracts are bionic's (`_Nonnull` strings are NUL-terminated, value buffers hold
//! `PROP_VALUE_MAX` bytes); null where bionic would crash is refused instead.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::sync::atomic::Ordering::Acquire;

use crate::generated::system_properties::{PROP_NAME_MAX, prop_info};
use crate::props::{Area, PropInfo, VALUE_MAX, Value};
use crate::{sdk, store};

type ReadCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32)>;
type ForeachCallback = Option<unsafe extern "C" fn(*const prop_info, *mut c_void)>;

/// bionic returns `-1` as `uint32_t` from the serial queries on error.
const NO_SERIAL: u32 = u32::MAX;

/// The bytes of a C string; none for null.
///
/// # Safety
/// A non-null `p` points to a NUL-terminated string that outlives `'a`.
unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a [u8]> {
    // SAFETY: the caller's contract
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_bytes())
}

/// The caller's value buffer.
///
/// # Safety
/// A non-null `p` points to `PROP_VALUE_MAX` writable bytes no Rust reference aliases.
unsafe fn value_buffer<'a>(p: *mut c_char) -> Option<&'a mut [u8; VALUE_MAX]> {
    // SAFETY: the caller's contract
    (!p.is_null()).then(|| unsafe { &mut *p.cast::<[u8; VALUE_MAX]>() })
}

fn record(area: &Area, pi: *const prop_info) -> Option<PropInfo> {
    area.info_at(pi.cast())
}

fn handle(area: &Area, pi: PropInfo) -> *const prop_info {
    area.ptr(pi.offset()).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_set(name: *const c_char, value: *const c_char) -> c_int {
    // SAFETY: bionic's contract, NUL-terminated strings
    let (name, value) = unsafe { (c_str(name), c_str(value)) };
    match store::set(name.unwrap_or_default(), value.unwrap_or_default()) {
        Ok(()) => 0,
        Err(e) => {
            errno::set_errno(errno::Errno(e.raw_os_error()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_find(name: *const c_char) -> *const prop_info {
    // SAFETY: bionic's contract, a NUL-terminated string
    let Some(name) = (unsafe { c_str(name) }) else {
        return core::ptr::null();
    };
    store::area()
        .and_then(|a| Some(handle(&a, a.find(name)?)))
        .unwrap_or(core::ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_read_callback(
    pi: *const prop_info,
    callback: ReadCallback,
    cookie: *mut c_void,
) {
    let Some((area, callback)) = store::area().zip(callback) else {
        return;
    };
    let Some(s) = record(&area, pi).and_then(|pi| area.snapshot(pi)) else {
        return;
    };
    let name = area.ptr(s.name).cast();
    match &s.value {
        // SAFETY: the caller's callback; name and value are NUL-terminated in the area, which is never
        // unmapped
        Value::InArea(v) => unsafe { callback(cookie, name, area.ptr(*v).cast(), s.serial) },
        // SAFETY: as above; the copy is NUL-terminated and outlives the call
        Value::Copied(v) => unsafe { callback(cookie, name, v.as_ptr().cast(), s.serial) },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_foreach(
    callback: ForeachCallback,
    cookie: *mut c_void,
) -> c_int {
    let Some((area, callback)) = store::area().zip(callback) else {
        return -1;
    };
    // SAFETY: the caller's callback, given records of the area, which is never unmapped
    area.for_each(&mut |pi| unsafe { callback(handle(&area, pi), cookie) });
    // bionic's Foreach reports success once initialised, whatever the walk met
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_read(
    pi: *const prop_info,
    name: *mut c_char,
    value: *mut c_char,
) -> c_int {
    // SAFETY: bionic's contract, a PROP_VALUE_MAX buffer
    let Some(out) = (unsafe { value_buffer(value) }) else {
        return -1;
    };
    let Some((area, pi)) = store::area().and_then(|a| Some((a, record(&a, pi)?))) else {
        out[0] = 0;
        return 0;
    };
    let len = store::read(area, pi, out);
    if !name.is_null() {
        // SAFETY: bionic's contract for a non-null name: PROP_NAME_MAX writable bytes
        let name =
            unsafe { core::slice::from_raw_parts_mut(name.cast::<u8>(), PROP_NAME_MAX as usize) };
        area.strlcpy(pi.name_offset(), name);
    }
    len as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int {
    // SAFETY: bionic's contract, a NUL-terminated name and a PROP_VALUE_MAX buffer
    let (name, out) = unsafe { (c_str(name), value_buffer(value)) };
    let (Some(name), Some(out)) = (name, out) else {
        return -1;
    };
    store::get(name, out) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn __system_property_area_serial() -> u32 {
    store::area().map_or(NO_SERIAL, |a| a.serial().load(Acquire))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_serial(pi: *const prop_info) -> u32 {
    store::area()
        .and_then(|a| Some(a.stable_serial(record(&a, pi)?)))
        .unwrap_or(NO_SERIAL)
}

#[unsafe(no_mangle)]
pub extern "C" fn android_get_device_api_level() -> c_int {
    sdk::device_api_level()
}

#[unsafe(no_mangle)]
pub extern "C" fn android_get_application_target_sdk_version() -> c_int {
    sdk::target_sdk()
}

#[unsafe(no_mangle)]
pub extern "C" fn android_set_application_target_sdk_version(target: c_int) {
    sdk::set_target_sdk(target);
}
