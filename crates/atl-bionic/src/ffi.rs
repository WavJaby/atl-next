// SPDX-License-Identifier: MIT
//! The C ABI (bridge 1 class b): thin shells that turn raw pointers into safe values at once and call
//! safe code. `generated::*` checks every signature here against bionic's headers.
//!
//! Pointer contracts are bionic's (`_Nonnull` strings are NUL-terminated, value buffers hold
//! `PROP_VALUE_MAX` bytes); null where bionic would crash is refused instead.

use core::ffi::{CStr, VaList, c_char, c_int, c_void};
use core::sync::atomic::Ordering::Acquire;
use std::ffi::CString;

use crate::generated::dlext::android_dlextinfo;
use crate::generated::libdl_android::android_namespace_t;
use crate::generated::system_properties::{PROP_NAME_MAX, prop_info};
use crate::props::{Area, PropInfo, VALUE_MAX, Value};
use crate::{async_safe, dlext, linker, sdk, store};

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

/// `enum android_fdsan_error_level` travels as its underlying `unsigned int`.
#[unsafe(no_mangle)]
pub extern "C" fn android_fdsan_set_error_level(new_level: u32) -> u32 {
    crate::fdsan::set_error_level(new_level)
}

/// bionic/malloc.h. glibc's allocator has none of bionic's options (allocation limit, leak info,
/// GWP-ASan, heapprofd, zygote profiling), so every opcode gets what bionic gives one it does not handle
/// (`HeapprofdMallopt`, malloc_heapprofd.cpp): `errno = ENOTSUP`, false.
#[unsafe(no_mangle)]
pub extern "C" fn android_mallopt(_opcode: c_int, _arg: *mut c_void, _arg_size: usize) -> bool {
    errno::set_errno(errno::Errno(rustix::io::Errno::NOTSUP.raw_os_error()));
    false
}

// android/dlext.h (libdl) and libdl_android's exported-namespace lookup, through `linker`.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_dlopen_ext(
    filename: *const c_char,
    flags: c_int,
    info: *const android_dlextinfo,
) -> *mut c_void {
    // SAFETY: bionic's contract, null or an android_dlextinfo
    let info = unsafe { info.as_ref() };
    let (ext, has_namespace) =
        info.map_or((0, false), |i| (i.flags, !i.library_namespace.is_null()));
    if let Err(r) = dlext::check(ext, has_namespace) {
        let detail =
            CString::new(format!("dlopen failed: {r}")).expect("the refusal text has no NUL");
        linker::fail(&detail);
        return core::ptr::null_mut();
    }
    // the namespace USE_NAMESPACE names and the caller's are both the one namespace
    // SAFETY: dlopen's contract is the caller's
    unsafe { linker::open(filename, flags, &linker::DEFAULT) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_get_exported_namespace(
    name: *const c_char,
) -> *mut android_namespace_t {
    // SAFETY: bionic's contract, a NUL-terminated string
    match unsafe { c_str(name) } {
        // bionic: a null name exports nothing
        None => core::ptr::null_mut(),
        Some(name) => core::ptr::from_ref(linker::exported(name))
            .cast_mut()
            .cast(),
    }
}

// libasync_safe. A format's arguments are the caller's printf contract; a null format or tag, where
// bionic would crash, is refused (-1; `fatal` formats nothing).

/// A C `va_list` as the formatter's arguments.
struct CArgs<'v, 'a>(&'v mut VaList<'a>);

impl<'s> async_safe::Args<'s> for CArgs<'_, '_> {
    fn int(&mut self) -> c_int {
        // SAFETY: the format asks for an int here (printf contract)
        unsafe { self.0.next_arg::<c_int>() }
    }
    fn u32(&mut self) -> u32 {
        // SAFETY: the format asks for a 32-bit value here
        unsafe { self.0.next_arg::<u32>() }
    }
    fn u64(&mut self) -> u64 {
        // SAFETY: the format asks for a 64-bit value here
        unsafe { self.0.next_arg::<u64>() }
    }
    fn ptr(&mut self) -> usize {
        // SAFETY: the format asks for a pointer here
        unsafe { self.0.next_arg::<*const c_void>() as usize }
    }
    fn str(&mut self) -> Option<&'s [u8]> {
        // SAFETY: the format asks for a string here: null or NUL-terminated, alive for the call
        unsafe { c_str(self.0.next_arg::<*const c_char>()) }
    }
}

unsafe extern "C" {
    // glibc >= 2.32: static English texts, no locale, no allocation (async-signal-safe)
    safe fn strerrordesc_np(errnum: c_int) -> *const c_char;
    safe fn strerrorname_np(errnum: c_int) -> *const c_char;
}

/// The bytes of a static C string glibc returned; none for null.
fn static_text(p: *const c_char) -> Option<&'static [u8]> {
    // SAFETY: glibc's error tables are static NUL-terminated strings
    unsafe { c_str(p) }
}

const ERRORS: async_safe::Errors = async_safe::Errors {
    describe: |e| static_text(strerrordesc_np(e)),
    name: |e| static_text(strerrorname_np(e)),
};

/// A result as bionic returns it: the count, or -1 with errno set.
fn count(r: Result<usize, i32>) -> c_int {
    match r {
        Ok(n) => n as c_int,
        Err(e) => {
            errno::set_errno(errno::Errno(e));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn async_safe_format_buffer(
    buffer: *mut c_char,
    size: usize,
    format: *const c_char,
    mut args: ...
) -> c_int {
    // SAFETY: bionic's contract, a NUL-terminated format
    let Some(format) = (unsafe { c_str(format) }) else {
        return -1;
    };
    let buf: &mut [u8] = match (buffer.is_null(), size) {
        (_, 0) => &mut [],
        (true, _) => return -1,
        // SAFETY: bionic's contract, `size` writable bytes no Rust reference aliases
        (false, _) => unsafe { core::slice::from_raw_parts_mut(buffer.cast(), size) },
    };
    let mut out = async_safe::Buffer::new(buf);
    async_safe::vformat(&mut out, format, &mut CArgs(&mut args), &ERRORS);
    out.total as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn async_safe_format_log(
    priority: c_int,
    tag: *const c_char,
    format: *const c_char,
    args: ...
) -> c_int {
    // SAFETY: forwarded as received
    unsafe { async_safe_format_log_va_list(priority, tag, format, args) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn async_safe_format_log_va_list(
    priority: c_int,
    tag: *const c_char,
    format: *const c_char,
    mut args: VaList<'_>,
) -> c_int {
    // SAFETY: bionic's contract, NUL-terminated strings
    let (Some(tag), Some(format)) = (unsafe { (c_str(tag), c_str(format)) }) else {
        return -1;
    };
    count(async_safe::format_log(
        priority,
        tag,
        format,
        &mut CArgs(&mut args),
        &ERRORS,
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn async_safe_write_log(
    priority: c_int,
    tag: *const c_char,
    msg: *const c_char,
) -> c_int {
    // SAFETY: bionic's contract, NUL-terminated strings
    let (Some(tag), Some(msg)) = (unsafe { (c_str(tag), c_str(msg)) }) else {
        return -1;
    };
    count(async_safe::write_log(priority, tag, msg))
}

/// The `async_safe_fatal` macro's call before its `abort()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn async_safe_fatal_no_abort(format: *const c_char, mut args: ...) {
    // SAFETY: bionic's contract, a NUL-terminated format
    let format = unsafe { c_str(format) }.unwrap_or_default();
    async_safe::fatal(None, format, &mut CArgs(&mut args), &ERRORS);
}
