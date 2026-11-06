// SPDX-License-Identifier: MIT
//! JNI natives of com.example.atlgl.Probe: `ext64` calls glGetInteger64vEXT(GL_MAX_TEXTURE_SIZE) through the address AOSP libEGL's eglGetProcAddress returns (patch 0010: the driver's, not a
//! forwarder); `owner` names the object that address lies in (dladdr). Test fixture: glibc's dl* directly, no crates.

use std::ffi::{CStr, c_char, c_int, c_void};

#[repr(C)]
struct DlInfo {
    fname: *const c_char,
    fbase: *mut c_void,
    sname: *const c_char,
    saddr: *mut c_void,
}

unsafe extern "C" {
    fn dlopen(file: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
}

/// <dlfcn.h>: RTLD_NOW | RTLD_NOLOAD -- the libEGL the framework already loaded, never a second copy.
const RTLD_NOW: c_int = 2;
const RTLD_NOLOAD: c_int = 4;
/// GLES2 gl2.h GL_MAX_TEXTURE_SIZE.
const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
/// Written before the call: a call that does nothing leaves it.
const UNWRITTEN: i64 = -1;
/// Failure codes (all negative, none a texture size).
const NO_LIBEGL: i64 = -2;
const NO_ENTRY: i64 = -3;
/// The extension: in Mesa's GLES dispatch, not in AOSP's entries.in (`xtask test gl` derives the candidate set).
const NAME: &CStr = c"glGetInteger64vEXT";
/// JNINativeInterface's NewStringUTF slot (jni.h: 4 reserved entries, then the functions in order).
const NEW_STRING_UTF: usize = 167;

type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GetInteger64v = unsafe extern "C" fn(u32, *mut i64);

#[cfg(feature = "undefined-gl")]
unsafe extern "C" {
    fn glClear(mask: u32);
}

/// Never called; its reference keeps glClear an undefined dynamic symbol (feature `undefined-gl`).
#[cfg(feature = "undefined-gl")]
#[unsafe(no_mangle)]
pub extern "C" fn atl_gl_probe_undefined_gl() {
    // SAFETY: never called; only the link-time reference matters.
    unsafe { glClear(0) }
}

unsafe extern "C" fn no_op(_: u32, _: *mut i64) {}

/// eglGetProcAddress(NAME), or null.
fn entry() -> Option<*mut c_void> {
    // SAFETY: dlopen/dlsym with NUL-terminated names; NOLOAD never runs constructors.
    let egl = unsafe { dlopen(c"libEGL.so".as_ptr(), RTLD_NOW | RTLD_NOLOAD) };
    if egl.is_null() {
        return None;
    }
    // SAFETY: as above; the symbol, when present, is eglGetProcAddress with its EGL signature.
    let gpa = unsafe { dlsym(egl, c"eglGetProcAddress".as_ptr()) };
    if gpa.is_null() {
        return None;
    }
    // SAFETY: eglGetProcAddress(const char*) -> function pointer (EGL 1.5 §3.10).
    let f = unsafe { std::mem::transmute::<*mut c_void, GetProcAddress>(gpa)(NAME.as_ptr()) };
    (!f.is_null()).then_some(f)
}

/// The value glGetInteger64vEXT writes for GL_MAX_TEXTURE_SIZE (UNWRITTEN if none); `broken` calls a no-op instead.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_atlgl_Probe_ext64(
    _env: *mut c_void,
    _class: *mut c_void,
    broken: u8,
) -> i64 {
    // SAFETY: as in entry()
    if unsafe { dlopen(c"libEGL.so".as_ptr(), RTLD_NOW | RTLD_NOLOAD) }.is_null() {
        return NO_LIBEGL;
    }
    let Some(f) = entry() else { return NO_ENTRY };
    let call: GetInteger64v = if broken != 0 {
        no_op
    } else {
        // SAFETY: glGetInteger64vEXT(GLenum, GLint64*) (EXT_disjoint_timer_query).
        unsafe { std::mem::transmute::<*mut c_void, GetInteger64v>(f) }
    };
    let mut v = UNWRITTEN;
    // SAFETY: the caller's thread has a current GLES3 context; v outlives the call.
    unsafe { call(GL_MAX_TEXTURE_SIZE, &mut v) };
    v
}

/// The file holding eglGetProcAddress(NAME)'s address, or "none".
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_atlgl_Probe_owner(
    env: *mut *const *const c_void,
    _class: *mut c_void,
) -> *mut c_void {
    let mut name = c"none".as_ptr();
    if let Some(f) = entry() {
        let mut info = DlInfo {
            fname: std::ptr::null(),
            fbase: std::ptr::null_mut(),
            sname: std::ptr::null(),
            saddr: std::ptr::null_mut(),
        };
        // SAFETY: info is a valid out struct; dladdr only reads the address.
        if unsafe { dladdr(f, &mut info) } != 0 && !info.fname.is_null() {
            name = info.fname;
        }
    }
    // SAFETY: env is the JNIEnv* JNI passes; slot NEW_STRING_UTF of its function table is NewStringUTF(JNIEnv*, const char*).
    unsafe {
        let new_string: unsafe extern "C" fn(
            *mut *const *const c_void,
            *const c_char,
        ) -> *mut c_void = std::mem::transmute(*(*env).add(NEW_STRING_UTF));
        new_string(env, name)
    }
}
