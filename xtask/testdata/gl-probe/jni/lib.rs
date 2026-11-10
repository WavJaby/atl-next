// SPDX-License-Identifier: MIT
//! JNI natives of com.example.atlgl.Probe: `ext64` calls glGetInteger64vEXT(GL_MAX_TEXTURE_SIZE) through the address AOSP libEGL's eglGetProcAddress returns (patch 0010: the driver's, not a
//! forwarder); `owner` names the object that address lies in (dladdr); `hostRenderer` is the host EGL's GL_RENDERER on the device of the held DRM nodes, queried on the host's libEGL.so.1 through atl-egl-host, not through AOSP's fronts. Test fixture: glibc's dl* directly, atl-egl-host for the host EGL.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

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
/// JNINativeInterface's GetStringUTFChars and ReleaseStringUTFChars slots.
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;

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

/// JNI NewStringUTF of `text`; a NUL inside it makes the string empty (CString).
fn new_string(env: *mut *const *const c_void, text: &str) -> *mut c_void {
    type New = unsafe extern "C" fn(*mut *const *const c_void, *const c_char) -> *mut c_void;
    let c = CString::new(text).unwrap_or_default();
    // SAFETY: env is the JNIEnv* JNI passes; slot NEW_STRING_UTF is NewStringUTF(JNIEnv*, const char*); c is NUL-terminated and outlives the call.
    unsafe {
        let new: New = std::mem::transmute(*(*env).add(NEW_STRING_UTF));
        new(env, c.as_ptr())
    }
}

/// JNI GetStringUTFChars: a Java string's text, copied out; None for null.
fn java_text(env: *mut *const *const c_void, s: *mut c_void) -> Option<String> {
    type Get =
        unsafe extern "C" fn(*mut *const *const c_void, *mut c_void, *mut u8) -> *const c_char;
    type Release = unsafe extern "C" fn(*mut *const *const c_void, *mut c_void, *const c_char);
    if s.is_null() {
        return None;
    }
    // SAFETY: env is the JNIEnv* JNI passes and s a jstring it passed in; the slots are GetStringUTFChars and ReleaseStringUTFChars, and the chars are read before their release.
    unsafe {
        let get: Get = std::mem::transmute(*(*env).add(GET_STRING_UTF_CHARS));
        let release: Release = std::mem::transmute(*(*env).add(RELEASE_STRING_UTF_CHARS));
        let chars = get(env, s, std::ptr::null_mut());
        if chars.is_null() {
            return None;
        }
        let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
        release(env, s, chars);
        Some(text)
    }
}

/// `hostRenderer(nodes, broken)`: `ok <device> <GL_RENDERER>`, or `err <message>`. The query runs on a thread of its own: the context it makes current is released when that thread ends, so the app's current context on the calling thread is never touched.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_example_atlgl_Probe_hostRenderer(
    env: *mut *const *const c_void,
    _class: *mut c_void,
    nodes: *mut c_void,
    broken: u8,
) -> *mut c_void {
    let text = match java_text(env, nodes) {
        None => "err no DRM node list".to_owned(),
        Some(nodes) => {
            match std::thread::spawn(move || host_renderer(&nodes, broken != 0)).join() {
                Ok(Ok(text)) => format!("ok {text}"),
                Ok(Err(err)) => format!("err {err}"),
                Err(_) => "err host query panicked".to_owned(),
            }
        }
    };
    new_string(env, &text)
}

/// `<device> <GL_RENDERER>` of the host EGL device the held nodes name: the device whose render or card node is held, else the software device. A held node no EGL device claims (gralloc's vgem) leaves the software device. `broken` mangles the renderer.
fn host_renderer(nodes: &str, broken: bool) -> Result<String, String> {
    let host = atl_egl_host::Host::load().map_err(|e| format!("{e:#}"))?;
    let devices = host.devices().map_err(|e| format!("{e:#}"))?;
    let held: Vec<&str> = nodes.split_whitespace().filter(|n| *n != "none").collect();
    let claims = |d: &atl_egl_host::Device| {
        held.iter()
            .any(|n| d.render_node.as_deref() == Some(*n) || d.drm_file.as_deref() == Some(*n))
    };
    let device = devices
        .iter()
        .find(|d| claims(d))
        .or_else(|| devices.iter().find(|d| d.is_software()))
        .ok_or_else(|| format!("no EGL device for `{nodes}` and no software device"))?;
    let name = device
        .render_node
        .clone()
        .or_else(|| device.drm_file.clone())
        .unwrap_or_else(|| "software".to_owned());
    let display = device.display().map_err(|e| format!("{e:#}"))?;
    let context = display.create_context().map_err(|e| format!("{e:#}"))?;
    let gl = context.make_current().map_err(|e| format!("{e:#}"))?;
    let renderer = gl.string(atl_egl_host::ffi::GL_RENDERER);
    let renderer = if broken {
        format!("{renderer} (mangled host)")
    } else {
        renderer
    };
    Ok(format!("{name} {renderer}"))
}
