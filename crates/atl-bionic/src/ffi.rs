// SPDX-License-Identifier: MIT
//! The C ABI (bridge 1 class b): thin shells that turn raw pointers into safe values at once and call
//! safe code. `generated::*` checks every signature here against bionic's headers.
//!
//! Pointer contracts are bionic's (`_Nonnull` strings are NUL-terminated, value buffers hold
//! `PROP_VALUE_MAX` bytes); null where bionic would crash is refused instead.

use core::ffi::{CStr, VaList, c_char, c_int, c_void};
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Acquire;
use std::ffi::CString;

use crate::generated::dlext::android_dlextinfo;
use crate::generated::libdl_android::android_namespace_t;
use crate::generated::resolv_netid::addrinfo;
use crate::generated::signal_names;
use crate::generated::system_properties::{PROP_NAME_MAX, prop_info, timespec};
use crate::generated::timespec as ts;
use crate::props::{Area, PropInfo, VALUE_MAX, Value};
use crate::{async_safe, dlext, linker, sdk, store};

type ReadCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32)>;
type ForeachCallback = Option<unsafe extern "C" fn(*const prop_info, *mut c_void)>;

/// bionic returns `-1` as `uint32_t` from the serial queries on error.
const NO_SERIAL: u32 = u32::MAX;
/// Offset of `prop_info.serial` (bionic's layout, generated).
const PROP_INFO_SERIAL: usize = crate::generated::prop_format::prop_info::serial;

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

/// bionic: 0 once the area is mapped, -1 if it cannot be; errno left as it was (`ErrnoRestorer`).
#[unsafe(no_mangle)]
pub extern "C" fn __system_properties_init() -> c_int {
    let saved = errno::errno();
    let ok = store::init();
    errno::set_errno(saved);
    if ok { 0 } else { -1 }
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

/// bionic `SystemProperties::Serial` reads `pi->serial` of any `prop_info`, also one outside the area:
/// libcutils trace-dev.inc polls a zeroed 96-byte stand-in (`empty_pi`, serial 0) until the property it
/// stands for exists, and must see 0 there, not this crate's error value, which equals its "not
/// initialised" sentinel. Such a record has no writer, so its serial is read once, without the dirty wait.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_serial(pi: *const prop_info) -> u32 {
    if pi.is_null() {
        return NO_SERIAL;
    }
    if let Some(s) = store::area().and_then(|a| Some(a.stable_serial(record(&a, pi)?))) {
        return s;
    }
    // SAFETY: bionic's contract: `pi` is a `prop_info`, whose serial is an aligned 32-bit atomic at this offset
    let serial = unsafe { &*pi.cast::<u8>().add(PROP_INFO_SERIAL).cast::<AtomicU32>() };
    serial.load(Acquire)
}

/// A signed integer field of `size` bytes, widened.
///
/// # Safety
/// `p` points to `size` readable bytes.
unsafe fn int_field(p: *const u8, size: usize) -> i64 {
    match size {
        // SAFETY: the caller's contract
        4 => i64::from(unsafe { p.cast::<i32>().read_unaligned() }),
        // SAFETY: the caller's contract; `field` leaves only 8
        _ => unsafe { p.cast::<i64>().read_unaligned() },
    }
}

/// `struct timespec` fields as (offset, size): clang's, the same for bionic and the host (build.rs).
const TV_SEC: (usize, usize) = field(ts::TIMESPEC_TV_SEC, ts::TIMESPEC_TV_SEC_SIZE);
const TV_NSEC: (usize, usize) = field(ts::TIMESPEC_TV_NSEC, ts::TIMESPEC_TV_NSEC_SIZE);

const fn field(offset: u64, size: u64) -> (usize, usize) {
    assert!((size == 4 || size == 8) && offset + size <= ts::TIMESPEC_SIZE);
    (offset as usize, size as usize)
}

/// The C timeout as rustix's, none for null.
///
/// # Safety
/// A non-null `p` points to a `struct timespec`.
unsafe fn timeout(p: *const timespec) -> Option<rustix::thread::futex::Timespec> {
    let p = (!p.is_null()).then_some(p.cast::<u8>())?;
    // SAFETY: the caller's contract; both fields lie inside the struct
    unsafe {
        Some(rustix::thread::futex::Timespec {
            tv_sec: int_field(p.add(TV_SEC.0), TV_SEC.1),
            tv_nsec: int_field(p.add(TV_NSEC.0), TV_NSEC.1),
        })
    }
}

/// bionic `SystemProperties::Wait`. Refused (false) where bionic crashes (null `new_serial_ptr`, a
/// pointer that is no record) and without an area (bionic returns `-1` as `bool`, true, leaving
/// `*new_serial_ptr` unset).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __system_property_wait(
    pi: *const prop_info,
    old_serial: u32,
    new_serial_ptr: *mut u32,
    relative_timeout: *const timespec,
) -> bool {
    let Some(area) = store::area() else {
        return false;
    };
    let pi = match pi.is_null() {
        true => None,
        false => match record(&area, pi) {
            Some(pi) => Some(pi),
            None => return false,
        },
    };
    // SAFETY: bionic's contract, null or a writable uint32_t no Rust reference aliases
    let Some(out) = (unsafe { new_serial_ptr.as_mut() }) else {
        return false;
    };
    // SAFETY: bionic's contract, null or a timespec
    let timeout = unsafe { timeout(relative_timeout) };
    match area.wait(pi, old_serial, timeout.as_ref()) {
        Some(new) => {
            *out = new;
            true
        }
        None => false,
    }
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

/// `enum android_fdsan_owner_type` travels as its underlying `unsigned int`.
#[unsafe(no_mangle)]
pub extern "C" fn android_fdsan_create_owner_tag(owner_type: u32, tag: u64) -> u64 {
    crate::fdsan::create_owner_tag(owner_type, tag)
}

#[unsafe(no_mangle)]
pub extern "C" fn android_fdsan_exchange_owner_tag(fd: c_int, expected_tag: u64, new_tag: u64) {
    crate::fdsan::exchange_owner_tag(fd, expected_tag, new_tag);
}

#[unsafe(no_mangle)]
pub extern "C" fn android_fdsan_get_owner_tag(fd: c_int) -> u64 {
    crate::fdsan::get_owner_tag(fd)
}

unsafe extern "C" {
    /// The host libc's `close`.
    #[link_name = "close"]
    fn libc_close(fd: c_int) -> c_int;
}

/// `close`'s result, errno as `close` left it (bionic's).
#[unsafe(no_mangle)]
pub extern "C" fn android_fdsan_close_with_tag(fd: c_int, tag: u64) -> c_int {
    let (rc, _) = crate::fdsan::close_with_tag(fd, tag, |fd| {
        // SAFETY: closing a descriptor touches no memory; that the caller owns it is bionic's contract
        let rc = unsafe { libc_close(fd) };
        (rc, errno::errno().0)
    });
    rc
}

/// bionic/malloc.h. glibc's allocator has none of bionic's options (allocation limit, leak info,
/// GWP-ASan, heapprofd, zygote profiling), so every opcode gets what bionic gives one it does not handle
/// (`HeapprofdMallopt`, malloc_heapprofd.cpp): `errno = ENOTSUP`, false.
#[unsafe(no_mangle)]
pub extern "C" fn android_mallopt(_opcode: c_int, _arg: *mut c_void, _arg_size: usize) -> bool {
    errno::set_errno(errno::Errno(rustix::io::Errno::NOTSUP.raw_os_error()));
    false
}

// bionic's malloc introspection (malloc_common.cpp, malloc_common_dynamic.cpp), for libmemunreachable. glibc's allocator offers
// no heap walk and no way to stop other threads' allocations, so each answers as bionic's allocators without the support do:
// disable / enable do nothing (as HWASan's), malloc_backtrace has 0 frames (as without malloc debug).
// Deviation, malloc_iterate: it fails, -1 with errno ENOTSUP, where HWASan's returns 0 and calls nothing. A walk that reports no
// chunk reads as "no allocation, so no leak" to libmemunreachable; a failure lets its caller say the heap could not be walked.

#[unsafe(no_mangle)]
pub extern "C" fn malloc_disable() {}

#[unsafe(no_mangle)]
pub extern "C" fn malloc_enable() {}

#[unsafe(no_mangle)]
pub extern "C" fn malloc_iterate(
    _base: usize,
    _size: usize,
    _callback: Option<unsafe extern "C" fn(usize, usize, *mut c_void)>,
    _arg: *mut c_void,
) -> c_int {
    errno::set_errno(errno::Errno(rustix::io::Errno::NOTSUP.raw_os_error()));
    -1
}

#[unsafe(no_mangle)]
pub extern "C" fn malloc_backtrace(
    _pointer: *mut c_void,
    _frames: *mut u64,
    _frame_count: usize,
) -> isize {
    0
}

/// bionic's netd resolver entry (resolv_netid.h). Single-network semantics: one network exists, so `netid` and `mark` are
/// ignored and the call is the host's `getaddrinfo` with the same arguments and result, as bionic's own `getaddrinfo` is this
/// function with `NETID_UNSET` / `MARK_UNSET` (getaddrinfo.c). Multi-network support plugs in here, through the platform
/// layer's network service: this is the one place to extend.
/// Callers are compiled against the host libc's `<netdb.h>`, so `struct addrinfo` and the `EAI_*` results are the host's on
/// both sides; only pointers pass through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_getaddrinfofornet(
    node: *const c_char,
    service: *const c_char,
    hints: *const addrinfo,
    _netid: core::ffi::c_uint,
    _mark: core::ffi::c_uint,
    res: *mut *mut addrinfo,
) -> c_int {
    unsafe extern "C" {
        fn getaddrinfo(
            node: *const c_char,
            service: *const c_char,
            hints: *const addrinfo,
            res: *mut *mut addrinfo,
        ) -> c_int;
    }
    // SAFETY: the caller's arguments under getaddrinfo's own contract, unchanged (the same pointers bionic forwards)
    unsafe { getaddrinfo(node, service, hints, res) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_set_abort_message(msg: *const c_char) {
    // SAFETY: bionic's contract, null or a NUL-terminated string
    crate::abort::set(unsafe { c_str(msg) });
}

/// Where glibc lacks it (build.rs `IN_HOST_HEADERS`). `pthread_t` travels as bionic's `long`, glibc's
/// `unsigned long`: one register either way.
#[cfg(atl_bionic_api = "pthread")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_gettid_np(thread: core::ffi::c_long) -> c_int {
    // SAFETY: bionic's contract, a live pthread_t of this process
    unsafe { crate::thread::gettid_of(thread as usize) }
}

#[cfg(atl_bionic_api = "progname")]
unsafe extern "C" {
    /// glibc's short program name (`<errno.h>`, `_GNU_SOURCE`), exported writable; bionic's `__progname`.
    static mut program_invocation_short_name: *const c_char;
}

/// Where glibc lacks it (build.rs `IN_HOST_HEADERS`: no glibc has it): the program name `setprogname` set, else the short name of argv[0] (glibc initialises it as bionic's libc init sets `__progname`).
#[cfg(atl_bionic_api = "progname")]
#[unsafe(no_mangle)]
pub extern "C" fn getprogname() -> *const c_char {
    // SAFETY: a read of glibc's pointer, unsynchronised as bionic reads `__progname`
    unsafe { program_invocation_short_name }
}

/// Where glibc lacks it (build.rs `IN_HOST_HEADERS`: no glibc has it). The name points into `name`, as bionic's.
#[cfg(atl_bionic_api = "progname")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setprogname(name: *const c_char) {
    // SAFETY: bionic's contract, a NUL-terminated string that outlives the program name; null, where bionic crashes, is ignored
    let Some(path) = (unsafe { c_str(name) }) else {
        return;
    };
    let offset = crate::progname::short_name_offset(path);
    // SAFETY: offset <= path.len(), inside `name`'s string or at its NUL; glibc's variable is a plain pointer, written unsynchronised as bionic writes `__progname`
    unsafe { program_invocation_short_name = name.add(offset) };
}

/// bionic's `__assert(file, line, msg)` (assert.cpp): fatal message, abort. Exported under its build.rs `RENAMED_IN_HOST` name: glibc's `__assert` takes `(assertion, file, line)`.
#[unsafe(export_name = "atl_bionic___assert")]
pub unsafe extern "C" fn __assert(file: *const c_char, line: c_int, msg: *const c_char) {
    // SAFETY: bionic's contract, NUL-terminated strings; null prints as empty
    let (file, msg) = unsafe { (c_str(file), c_str(msg)) };
    async_safe::assert_failed(file.unwrap_or_default(), line, msg.unwrap_or_default());
    std::process::abort()
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

/// The namespace a C handle names: every non-null handle is one this crate returned, so `DEFAULT`.
fn namespace(ns: *mut android_namespace_t) -> Option<&'static linker::Namespace> {
    (!ns.is_null()).then_some(&linker::DEFAULT)
}

#[unsafe(no_mangle)]
pub extern "C" fn android_create_namespace(
    _name: *const c_char,
    _ld_library_path: *const c_char,
    _default_library_path: *const c_char,
    _type: u64,
    _permitted_when_isolated_path: *const c_char,
    _parent: *mut android_namespace_t,
) -> *mut android_namespace_t {
    core::ptr::from_ref(linker::create()).cast_mut().cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_get_LD_LIBRARY_PATH(buffer: *mut c_char, buffer_size: usize) {
    match linker::default_library_paths(buffer_size) {
        Ok(paths) => {
            let bytes = paths.to_bytes_with_nul();
            // SAFETY: bionic's contract, `buffer` writable for `buffer_size` bytes, which hold `bytes` (checked)
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), buffer, bytes.len()) };
        }
        Err(required) => {
            // bionic linker.cpp: async_safe_fatal, then abort
            let msg = format!(
                "android_get_LD_LIBRARY_PATH failed, buffer too small: buffer len {buffer_size}, required len {required}"
            );
            async_safe::fatal_text(b"", msg.as_bytes());
            std::process::abort()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_dlwarning(
    obj: *mut c_void,
    f: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) {
    let warnings = linker::take_warnings();
    // bionic get_dlwarning: f(obj, nullptr) when there are none, else f(obj, text); it calls a null f, which crashes there
    let text = warnings.as_deref().map_or(core::ptr::null(), CStr::as_ptr);
    if let Some(f) = f {
        // SAFETY: bionic's contract, the caller's callback and its object; `text` lives to the end of this call
        unsafe { f(obj, text) }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn android_link_namespaces(
    namespace_from: *mut android_namespace_t,
    namespace_to: *mut android_namespace_t,
    shared_libs_sonames: *const c_char,
) -> bool {
    // SAFETY: bionic's contract, null or a NUL-terminated string
    let sonames = unsafe { c_str(shared_libs_sonames) };
    match linker::link(namespace(namespace_from), namespace(namespace_to), sonames) {
        Ok(()) => true,
        Err(r) => {
            // bionic dlfcn.cpp __bionic_format_dlerror: "<call> failed: <detail>"
            let detail = CString::new(format!("android_link_namespaces failed: {r}"))
                .expect("the refusal text has no NUL");
            linker::fail(&detail);
            false
        }
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

/// bionic private `__get_tls()` (build.rs `TLS_HEADER`, declared there by hand: bionic's is an inline asm
/// read, no declaration to check against): this thread's slot 0.
#[unsafe(no_mangle)]
pub extern "C" fn __get_tls() -> *mut *mut c_void {
    crate::tls::slot_zero()
}

/// A C `const char* const [_NSIG]`: null or a static NUL-terminated string per signal number.
#[repr(transparent)]
pub struct SignalTable([*const c_char; signal_names::NSIG]);

// SAFETY: immutable, and every pointer is null or a `'static` C string literal
unsafe impl Sync for SignalTable {}

/// bionic's strsignal.cpp tables: entry `pick` of each `signal_names::ENTRIES` row at its number, null elsewhere.
const fn signal_table(pick: usize) -> SignalTable {
    let mut table = [core::ptr::null(); signal_names::NSIG];
    let mut i = 0;
    while i < signal_names::ENTRIES.len() {
        let (number, name, description) = signal_names::ENTRIES[i];
        table[number] = if pick == 0 { name } else { description }.as_ptr();
        i += 1;
    }
    SignalTable(table)
}

/// bionic's `sys_signame` (`<signal.h>`, build.rs `SIGDEFS`): "HUP" for SIGHUP.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static sys_signame: SignalTable = signal_table(0);

/// bionic's `sys_siglist` (`<signal.h>`, build.rs `SIGDEFS`): "Hangup" for SIGHUP. glibc's own is a non-default compat version, so links bind this one.
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static sys_siglist: SignalTable = signal_table(1);

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(table: &SignalTable, number: usize) -> Option<&'static str> {
        let p = table.0[number];
        // SAFETY: null or a 'static C string literal (SignalTable)
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_str().unwrap())
    }

    // Numbers: Linux generic signal ABI (asm-generic/signal.h: SIGHUP 1, SIGKILL 9, SIGSYS 31; _NSIG 64 -> bionic NSIG 65). Strings: bionic_sigdefs.h at the pin.
    #[test]
    fn tables_match_bionic() {
        assert_eq!(signal_names::NSIG, 65);
        assert_eq!(entry(&sys_signame, 1), Some("HUP"));
        assert_eq!(entry(&sys_siglist, 1), Some("Hangup"));
        assert_eq!(entry(&sys_signame, 9), Some("KILL"));
        assert_eq!(entry(&sys_siglist, 31), Some("Bad system call"));
        // signal 0 and the real-time range stay null (strsignal.cpp: "that's the ABI")
        assert_eq!(entry(&sys_signame, 0), None);
        assert_eq!(entry(&sys_siglist, 34), None);
    }
}
