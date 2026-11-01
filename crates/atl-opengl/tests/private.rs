// SPDX-License-Identifier: MIT
//! libEGL's private API as framework code calls it: through the mangled C++ names, on the host's EGL with a surfaceless Mesa display (EGL_MESA_platform_surfaceless).
//! Its own test binary: the display is process-wide, and smoke.rs initializes and terminates the same one.
//! Constants are the Khronos registry's, as AOSP's pinned egl.h / eglext.h define them.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::ptr::{null, null_mut};

// the exports live in the crate; nothing else of it is named here
use atl_opengl as _;

type Display = *mut c_void;

const EGL_TRUE: c_uint = 1;
const EGL_PLATFORM_SURFACELESS_MESA: c_uint = 0x31DD;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetPlatformDisplay(
        platform: c_uint,
        native: *mut c_void,
        attribs: *const isize,
    ) -> Display;
    fn eglInitialize(dpy: Display, major: *mut i32, minor: *mut i32) -> c_uint;
    fn eglTerminate(dpy: Display) -> c_uint;
}

// AOSP private/EGL/{cache,display}.h: `namespace android`, C++ linkage
unsafe extern "C" {
    #[link_name = "_ZN7android22egl_set_cache_filenameEPKc"]
    fn egl_set_cache_filename(filename: *const c_char);
    #[link_name = "_ZN7android18egl_get_init_countEPv"]
    fn egl_get_init_count(dpy: Display) -> c_int;
}

// hwui renderthread/EglManager.cpp: global scope, C++ linkage; Itanium mangling of eglBeginFrame(void*, void*)
unsafe extern "C" {
    #[link_name = "_Z13eglBeginFramePvS_"]
    fn eglBeginFrame(dpy: Display, surface: *mut c_void);
}

#[test]
fn begin_frame_accepts_any_handles() {
    // AOSP validates both handles and only sets the EGL error on bad ones; hwui passes its display and current surface
    // SAFETY: the export ignores its arguments
    unsafe {
        eglBeginFrame(null_mut(), null_mut());
        eglBeginFrame(0x1234 as Display, 0x5678 as *mut c_void);
    }
}

#[test]
fn init_count_follows_initialize_and_terminate() {
    // SAFETY: EGL calls with the display EGL returned, in the order the spec allows
    unsafe {
        let dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, null_mut(), null());
        assert!(
            !dpy.is_null(),
            "no surfaceless EGL display: is Mesa's EGL vendor installed?"
        );
        assert_eq!(egl_get_init_count(null_mut()), 0, "EGL_NO_DISPLAY");
        assert_eq!(egl_get_init_count(dpy), 0, "before eglInitialize");
        assert_eq!(eglInitialize(dpy, null_mut(), null_mut()), EGL_TRUE);
        assert_eq!(egl_get_init_count(dpy), 1, "initialized");
        // AOSP counts 2 here; the host EGL keeps no count (crate::private::init_count)
        assert_eq!(eglInitialize(dpy, null_mut(), null_mut()), EGL_TRUE);
        assert_eq!(egl_get_init_count(dpy), 1, "initialized twice");
        // one eglTerminate ends the host display, however many eglInitialize preceded it (EGL 1.5 §3.2)
        assert_eq!(eglTerminate(dpy), EGL_TRUE);
        assert_eq!(egl_get_init_count(dpy), 0, "terminated");
        // a handle EGL never returned
        let forged = 0x1234 as Display;
        assert_eq!(egl_get_init_count(forged), 0, "forged handle");
    }
}

#[test]
fn set_cache_filename_writes_nothing() {
    let dir = std::env::temp_dir().join(format!("atl-opengl-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("com.android.opengl.shaders_cache");
    let path = std::ffi::CString::new(file.to_str().expect("UTF-8 temp path")).expect("no NUL");
    // SAFETY: a NUL-terminated path, as hwui passes
    unsafe { egl_set_cache_filename(path.as_ptr()) };
    assert!(!file.exists(), "a no-op creates no cache file");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
