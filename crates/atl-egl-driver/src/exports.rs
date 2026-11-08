// SPDX-License-Identifier: MIT
//! The exported egl* entries Loader.cpp dlsyms (build.rs `EGL_EXPORTS`), each checked against the header's signature below.
//! A handle that is not the driver's own (a pbuffer, any context / config / image / sync) goes to the host entry unchanged: the driver hands out the host's handles.

use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, Mutex};

use atl_gralloc::handle::{FD_DMABUF, Params};

use crate::abi::egl as e;
use crate::driver::{self, Driver, set_error, take_error};
use crate::fbo0;
use crate::surface::{self, WindowSurface};
use crate::window::{Buffer, Window};

const _: e::eglGetProcAddress = eglGetProcAddress;
const _: e::eglGetError = eglGetError;
const _: e::eglGetDisplay = eglGetDisplay;
const _: e::eglTerminate = eglTerminate;
const _: e::eglQueryString = eglQueryString;
const _: e::eglChooseConfig = eglChooseConfig;
const _: e::eglGetConfigAttrib = eglGetConfigAttrib;
const _: e::eglCreateWindowSurface = eglCreateWindowSurface;
const _: e::eglDestroySurface = eglDestroySurface;
const _: e::eglQuerySurface = eglQuerySurface;
const _: e::eglSurfaceAttrib = eglSurfaceAttrib;
const _: e::eglMakeCurrent = eglMakeCurrent;
const _: e::eglSwapBuffers = eglSwapBuffers;
const _: e::eglSwapBuffersWithDamageKHR = eglSwapBuffersWithDamageKHR;
const _: e::eglSwapInterval = eglSwapInterval;
const _: e::eglCreateImageKHR = eglCreateImageKHR;

type Proc = Option<unsafe extern "C" fn()>;

fn fail<T>(err: i32, v: T) -> T {
    set_error(err);
    v
}

/// The driver for a display handle it gave out.
fn driver_for(dpy: e::EGLDisplay) -> Option<&'static Driver> {
    driver::get().filter(|d| d.is_display(dpy))
}

fn own(f: *const ()) -> Proc {
    // SAFETY: `f` is one of this crate's `extern "C"` entries; the caller casts it back to its real type (eglGetProcAddress's contract).
    Some(unsafe { std::mem::transmute::<*const (), unsafe extern "C" fn()>(f) })
}

/// EGL 1.5 / EXT platform entries: they would bypass the driver's window surfaces, so the driver offers none and reports 1.4.
const WITHHELD: &[&[u8]] = &[
    b"eglGetPlatformDisplay",
    b"eglGetPlatformDisplayEXT",
    b"eglCreatePlatformWindowSurface",
    b"eglCreatePlatformWindowSurfaceEXT",
    b"eglCreatePlatformPixmapSurface",
    b"eglCreatePlatformPixmapSurfaceEXT",
];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> Proc {
    if name.is_null() {
        return None;
    }
    // SAFETY: a non-null name is a NUL-terminated string (EGL's contract).
    let n = unsafe { CStr::from_ptr(name) };
    let b = n.to_bytes();
    let ours: Option<*const ()> = match b {
        b"eglGetProcAddress" => Some(eglGetProcAddress as *const ()),
        b"eglGetError" => Some(eglGetError as *const ()),
        b"eglGetDisplay" => Some(eglGetDisplay as *const ()),
        b"eglTerminate" => Some(eglTerminate as *const ()),
        b"eglQueryString" => Some(eglQueryString as *const ()),
        b"eglChooseConfig" => Some(eglChooseConfig as *const ()),
        b"eglGetConfigAttrib" => Some(eglGetConfigAttrib as *const ()),
        b"eglCreateWindowSurface" => Some(eglCreateWindowSurface as *const ()),
        b"eglDestroySurface" => Some(eglDestroySurface as *const ()),
        b"eglQuerySurface" => Some(eglQuerySurface as *const ()),
        b"eglSurfaceAttrib" => Some(eglSurfaceAttrib as *const ()),
        b"eglMakeCurrent" => Some(eglMakeCurrent as *const ()),
        b"eglSwapBuffers" => Some(eglSwapBuffers as *const ()),
        b"eglSwapBuffersWithDamageKHR" => Some(eglSwapBuffersWithDamageKHR as *const ()),
        b"eglSwapInterval" => Some(eglSwapInterval as *const ()),
        b"eglCreateImageKHR" => Some(eglCreateImageKHR as *const ()),
        _ => fbo0::wrapper(b),
    };
    if let Some(f) = ours {
        return own(f);
    }
    if WITHHELD.contains(&b) {
        return None;
    }
    let p = driver::get()?.display.host().proc_address(n);
    // SAFETY: the host's address for `name`, the same contract as ours.
    (!p.is_null())
        .then(|| unsafe { std::mem::transmute::<*const c_void, unsafe extern "C" fn()>(p) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetError() -> i32 {
    if let Some(err) = take_error() {
        return err;
    }
    match driver::get() {
        // SAFETY: no arguments.
        Some(d) => unsafe { (d.egl.eglGetError)() },
        None => e::EGL_SUCCESS,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetDisplay(native: *mut c_void) -> e::EGLDisplay {
    if !native.is_null() {
        // EGL_DEFAULT_DISPLAY is the only display an Android driver is asked for (egl_display.cpp)
        return fail(e::EGL_BAD_PARAMETER, std::ptr::null_mut());
    }
    driver::get().map_or(std::ptr::null_mut(), |d| d.display.raw())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglTerminate(dpy: e::EGLDisplay) -> u32 {
    // the host display is shared by everything in the process (atl-egl-host: never terminated); the fronts' own state is theirs
    if driver_for(dpy).is_some() {
        e::EGL_TRUE as u32
    } else {
        fail(e::EGL_BAD_DISPLAY, 0)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglQueryString(dpy: e::EGLDisplay, name: i32) -> *const c_char {
    let Some(d) = driver_for(dpy) else {
        // client extensions (EGL_NO_DISPLAY) and foreign displays: the host's answer
        return match driver::get() {
            // SAFETY: the host entry with the caller's arguments.
            Some(d) => unsafe { (d.egl.eglQueryString)(dpy, name) },
            None => std::ptr::null(),
        };
    };
    match name {
        e::EGL_VERSION => d.version().as_ptr(),
        e::EGL_EXTENSIONS => d.extensions().as_ptr(),
        e::EGL_CLIENT_APIS => c"OpenGL_ES".as_ptr(),
        // SAFETY: the host entry with the caller's arguments (EGL_VENDOR and anything else).
        _ => unsafe { (d.egl.eglQueryString)(dpy, name) },
    }
}

/// eglChooseConfig with window surfaces asked for as the host's off-screen (PBUFFER) configs, then only those [`Driver::window_format`] accepts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglChooseConfig(
    dpy: e::EGLDisplay,
    attribs: *const i32,
    configs: *mut e::EGLConfig,
    size: i32,
    num: *mut i32,
) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    if num.is_null() {
        return fail(e::EGL_BAD_PARAMETER, 0);
    }
    // SAFETY: a non-null list is NONE-terminated (EGL's contract).
    let pairs = unsafe { attrib_pairs(attribs) };
    let mut host_attribs = Vec::with_capacity(pairs.len() * 2 + 3);
    // EGL's default EGL_SURFACE_TYPE is EGL_WINDOW_BIT
    let mut window = true;
    let mut typed = false;
    for (k, v) in pairs {
        match k {
            e::EGL_RECORDABLE_ANDROID
            | e::EGL_FRAMEBUFFER_TARGET_ANDROID
            | e::EGL_NATIVE_RENDERABLE => {}
            e::EGL_SURFACE_TYPE => {
                typed = true;
                window = v != e::EGL_DONT_CARE && v & e::EGL_WINDOW_BIT != 0;
                let v = if window {
                    (v & !e::EGL_WINDOW_BIT) | e::EGL_PBUFFER_BIT
                } else {
                    v
                };
                host_attribs.extend([k, v]);
            }
            _ => host_attribs.extend([k, v]),
        }
    }
    if !typed {
        host_attribs.extend([e::EGL_SURFACE_TYPE, e::EGL_PBUFFER_BIT]);
    }
    host_attribs.push(e::EGL_NONE);
    let mut n = 0;
    // SAFETY: count only: a null array with size 0.
    let ok = unsafe {
        (d.egl.eglChooseConfig)(dpy, host_attribs.as_ptr(), std::ptr::null_mut(), 0, &mut n)
    };
    if ok == 0 {
        return 0;
    }
    let mut all = vec![std::ptr::null_mut(); usize::try_from(n).unwrap_or(0)];
    // SAFETY: `all` holds `n` slots.
    let ok =
        unsafe { (d.egl.eglChooseConfig)(dpy, host_attribs.as_ptr(), all.as_mut_ptr(), n, &mut n) };
    if ok == 0 {
        return 0;
    }
    all.truncate(usize::try_from(n).unwrap_or(0));
    if window {
        all.retain(|&c| d.window_format(c).is_some());
    }
    if configs.is_null() {
        // SAFETY: checked non-null.
        unsafe { *num = all.len() as i32 };
    } else {
        let k = all.len().min(usize::try_from(size).unwrap_or(0));
        // SAFETY: the caller's array holds `size` slots; `k` <= size.
        unsafe {
            std::ptr::copy_nonoverlapping(all.as_ptr(), configs, k);
            *num = k as i32;
        }
    }
    e::EGL_TRUE as u32
}

/// # Safety
/// `attribs` is null or a NONE-terminated key/value list.
unsafe fn attrib_pairs(attribs: *const i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    if attribs.is_null() {
        return out;
    }
    let mut i = 0;
    loop {
        // SAFETY: per the caller, every key before NONE has a value after it.
        let k = unsafe { *attribs.add(i) };
        if k == e::EGL_NONE {
            return out;
        }
        // SAFETY: as above.
        out.push((k, unsafe { *attribs.add(i + 1) }));
        i += 2;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetConfigAttrib(
    dpy: e::EGLDisplay,
    config: e::EGLConfig,
    attr: i32,
    value: *mut i32,
) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    if value.is_null() {
        return fail(e::EGL_BAD_PARAMETER, 0);
    }
    let format = d.window_format(config);
    let v = match attr {
        e::EGL_NATIVE_VISUAL_ID => format.unwrap_or(0),
        e::EGL_NATIVE_RENDERABLE
        | e::EGL_RECORDABLE_ANDROID
        | e::EGL_FRAMEBUFFER_TARGET_ANDROID => {
            if format.is_some() {
                e::EGL_TRUE
            } else {
                e::EGL_FALSE
            }
        }
        e::EGL_SURFACE_TYPE => match d.host_attrib(config, attr) {
            Some(st) if format.is_some() => st | e::EGL_WINDOW_BIT,
            Some(st) => st,
            None => return fail(e::EGL_BAD_CONFIG, 0),
        },
        // SAFETY: the host entry with the caller's arguments.
        _ => return unsafe { (d.egl.eglGetConfigAttrib)(dpy, config, attr, value) },
    };
    // SAFETY: checked non-null.
    unsafe { *value = v };
    e::EGL_TRUE as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglCreateWindowSurface(
    dpy: e::EGLDisplay,
    config: e::EGLConfig,
    win: e::EGLNativeWindowType,
    _attribs: *const i32,
) -> e::EGLSurface {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, std::ptr::null_mut());
    };
    if d.window_format(config).is_none() {
        return fail(e::EGL_BAD_MATCH, std::ptr::null_mut());
    }
    // SAFETY: the window eglCreateWindowSurface received is live for the call (the front validated it with NATIVE_WINDOW_IS_VALID).
    let Some(window) = (unsafe { Window::acquire(win.cast()) }) else {
        return fail(e::EGL_BAD_NATIVE_WINDOW, std::ptr::null_mut());
    };
    match WindowSurface::new(d, window, config) {
        Ok(s) => {
            let s = Arc::new(Mutex::new(s));
            let handle = Arc::as_ptr(&s) as usize;
            match d.surfaces.lock() {
                Ok(mut m) => {
                    m.insert(handle, s);
                    handle as e::EGLSurface
                }
                Err(_) => fail(e::EGL_BAD_ALLOC, std::ptr::null_mut()),
            }
        }
        Err(err) => fail(err, std::ptr::null_mut()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglDestroySurface(dpy: e::EGLDisplay, s: e::EGLSurface) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    let removed = d
        .surfaces
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&(s as usize)));
    let Some(surface) = removed else {
        // SAFETY: not ours: the host's surface.
        return unsafe { (d.egl.eglDestroySurface)(dpy, s) };
    };
    let current_ctx = match fbo0::current() {
        Some((h, ctx)) if h == s as usize => {
            fbo0::set_current(None, 0);
            Some(ctx)
        }
        Some((_, ctx)) => Some(ctx),
        None => None,
    };
    // another thread still in a call on this surface keeps it alive: its GL names then go with their context
    if let Ok(m) = Arc::try_unwrap(surface)
        && let Ok(s) = m.into_inner()
    {
        s.destroy(d, current_ctx);
    }
    e::EGL_TRUE as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglQuerySurface(
    dpy: e::EGLDisplay,
    s: e::EGLSurface,
    attr: i32,
    value: *mut i32,
) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    let Some(surface) = d.surface(s) else {
        // SAFETY: not ours: the host's surface.
        return unsafe { (d.egl.eglQuerySurface)(dpy, s, attr, value) };
    };
    if value.is_null() {
        return fail(e::EGL_BAD_PARAMETER, 0);
    }
    let Ok(s) = surface.lock() else {
        return fail(e::EGL_BAD_SURFACE, 0);
    };
    let v = match attr {
        e::EGL_WIDTH => s.width,
        e::EGL_HEIGHT => s.height,
        e::EGL_RENDER_BUFFER => e::EGL_BACK_BUFFER,
        e::EGL_SWAP_BEHAVIOR => e::EGL_BUFFER_DESTROYED,
        e::EGL_CONFIG_ID => match d.host_attrib(s.config, e::EGL_CONFIG_ID) {
            Some(v) => v,
            None => return fail(e::EGL_BAD_SURFACE, 0),
        },
        _ => return fail(e::EGL_BAD_ATTRIBUTE, 0),
    };
    // SAFETY: checked non-null.
    unsafe { *value = v };
    e::EGL_TRUE as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSurfaceAttrib(
    dpy: e::EGLDisplay,
    s: e::EGLSurface,
    attr: i32,
    value: i32,
) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    if d.surface(s).is_none() {
        // SAFETY: not ours: the host's surface.
        return unsafe { (d.egl.eglSurfaceAttrib)(dpy, s, attr, value) };
    }
    match (attr, value) {
        (e::EGL_SWAP_BEHAVIOR, e::EGL_BUFFER_DESTROYED) => e::EGL_TRUE as u32,
        // no config carries EGL_SWAP_BEHAVIOR_PRESERVED_BIT for a window
        (e::EGL_SWAP_BEHAVIOR, _) => fail(e::EGL_BAD_MATCH, 0),
        _ => fail(e::EGL_BAD_ATTRIBUTE, 0),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglMakeCurrent(
    dpy: e::EGLDisplay,
    draw: e::EGLSurface,
    read: e::EGLSurface,
    ctx: e::EGLContext,
) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    let (ds, rs) = (d.surface(draw), d.surface(read));
    if ds.is_none() && rs.is_none() {
        // SAFETY: none of ours: the host's surfaces (or none).
        let ok = unsafe { (d.egl.eglMakeCurrent)(dpy, draw, read, ctx) };
        if ok != 0 {
            fbo0::set_current(None, 0);
        }
        return ok;
    }
    let Some(surface) = ds.filter(|_| draw == read) else {
        // a window surface as read or draw only, or two different ones: not supported
        return fail(e::EGL_BAD_MATCH, 0);
    };
    let none = std::ptr::null_mut();
    // SAFETY: surfaceless make-current of the caller's context (EGL_KHR_surfaceless_context, both target hosts).
    let ok = unsafe { (d.egl.eglMakeCurrent)(dpy, none, none, ctx) };
    if ok == 0 {
        return 0;
    }
    let Ok(mut s) = surface.lock() else {
        return fail(e::EGL_BAD_SURFACE, 0);
    };
    match s.bind(d, ctx as usize) {
        Ok(fbo) => {
            fbo0::set_current(Some((draw as usize, ctx as usize)), fbo);
            let first = d
                .seen_contexts
                .lock()
                .map(|mut m| m.insert(ctx as usize))
                .unwrap_or(false);
            if first {
                // EGL 1.5 §3.7.3: the first make-current of a context sets viewport and scissor to the draw surface
                // SAFETY: the context is current.
                unsafe {
                    (d.gl.glViewport)(0, 0, s.width, s.height);
                    (d.gl.glScissor)(0, 0, s.width, s.height);
                }
            }
            e::EGL_TRUE as u32
        }
        Err(err) => {
            fbo0::set_current(None, 0);
            fail(err, 0)
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapBuffers(dpy: e::EGLDisplay, s: e::EGLSurface) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    let Some(surface) = d.surface(s) else {
        // SAFETY: not ours: the host's surface.
        return unsafe { (d.egl.eglSwapBuffers)(dpy, s) };
    };
    let Some((h, ctx)) = fbo0::current().filter(|(h, _)| *h == s as usize) else {
        // EGL: swapping a surface not current on this thread
        return fail(e::EGL_BAD_SURFACE, 0);
    };
    debug_assert_eq!(h, s as usize);
    let Ok(mut s) = surface.lock() else {
        return fail(e::EGL_BAD_SURFACE, 0);
    };
    match s.swap(d, ctx) {
        Ok(fbo) => {
            fbo0::set_fbo(fbo);
            e::EGL_TRUE as u32
        }
        Err(err) => fail(err, 0),
    }
}

/// The fronts already set the damage on the window (`native_window_set_surface_damage`, egl_platform_entries.cpp); the driver redraws whole buffers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapBuffersWithDamageKHR(
    dpy: e::EGLDisplay,
    s: e::EGLSurface,
    _rects: *const i32,
    _n: i32,
) -> u32 {
    // SAFETY: the same arguments eglSwapBuffers takes.
    unsafe { eglSwapBuffers(dpy, s) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglSwapInterval(dpy: e::EGLDisplay, interval: i32) -> u32 {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, 0);
    };
    match fbo0::current().and_then(|(h, _)| d.surface(h as e::EGLSurface)) {
        Some(s) => {
            if let Ok(s) = s.lock() {
                s.set_swap_interval(interval);
            }
            e::EGL_TRUE as u32
        }
        // SAFETY: the host's current surface, the caller's arguments.
        None => unsafe { (d.egl.eglSwapInterval)(dpy, interval) },
    }
}

/// `EGL_ANDROID_image_native_buffer`: an `ANativeWindowBuffer` (gralloc's handle) as a host dma-buf image; every other target is the host's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglCreateImageKHR(
    dpy: e::EGLDisplay,
    ctx: e::EGLContext,
    target: u32,
    buffer: e::EGLClientBuffer,
    attribs: *const i32,
) -> e::EGLImageKHR {
    let Some(d) = driver_for(dpy) else {
        return fail(e::EGL_BAD_DISPLAY, std::ptr::null_mut());
    };
    if target != e::EGL_NATIVE_BUFFER_ANDROID as u32 {
        // SAFETY: the host entry with the caller's arguments.
        return unsafe { (d.egl.eglCreateImageKHR)(dpy, ctx, target, buffer, attribs) };
    }
    if !ctx.is_null() {
        // EGL_ANDROID_image_native_buffer: the context must be EGL_NO_CONTEXT
        return fail(e::EGL_BAD_CONTEXT, std::ptr::null_mut());
    }
    // SAFETY: the client buffer of this target is an ANativeWindowBuffer the caller keeps alive for the call.
    let Some(buf) = (unsafe { Buffer::acquire(buffer.cast()) }) else {
        return fail(e::EGL_BAD_PARAMETER, std::ptr::null_mut());
    };
    let Some(h) = buf.handle() else {
        return fail(e::EGL_BAD_PARAMETER, std::ptr::null_mut());
    };
    let Ok(p) = Params::from_handle(h.fds.len(), h.ints) else {
        return fail(e::EGL_BAD_PARAMETER, std::ptr::null_mut());
    };
    match surface::import(d, &p, h.fds[FD_DMABUF]) {
        Ok(image) => image.into_raw(),
        Err(err) => fail(err, std::ptr::null_mut()),
    }
}
