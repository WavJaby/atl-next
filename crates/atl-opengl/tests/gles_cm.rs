// SPDX-License-Identifier: MIT
//! libGLESv1_CM's `gl*PointerBounds` as libandroid_runtime's GLES 1 bindings call them, on a GLES 1 context of a surfaceless Mesa display (EGL_MESA_platform_surfaceless, EGL_KHR_surfaceless_context).
//! Forwarding is observed through GL state: each array pointer function records size / type / stride / pointer, which glGetIntegerv / glGetPointerv read back (GLES 1.1 §6.1.3); the count is ignored, as AOSP's bodies ignore it.
//! Where the driver has no GLES 1 (Debian's Mesa), the check drops to symbol level.
//! Its own test binary: a GLES 1 context, separate from smoke.rs's GLES 2 one. Constants are the Khronos registry's, as AOSP's pinned egl.h / gl.h / glext.h define them.

use std::ffi::{CStr, c_char, c_uint, c_void};
use std::ptr::{null, null_mut};

// the exports live in the crate; nothing else of it is named here
use atl_opengl as _;

type Display = *mut c_void;
type Handle = *mut c_void;

const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: c_uint = 1;
const EGL_BAD_ALLOC: i32 = 0x3003;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES_BIT: i32 = 0x0001;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_OPENGL_ES_API: c_uint = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_PLATFORM_SURFACELESS_MESA: c_uint = 0x31DD;
const GL_NO_ERROR: c_uint = 0;
const GL_UNSIGNED_BYTE: c_uint = 0x1401;
const GL_FLOAT: c_uint = 0x1406;
const GL_EXTENSIONS: c_uint = 0x1F03;
const GL_VERTEX_ARRAY_SIZE: c_uint = 0x807A;
const GL_VERTEX_ARRAY_TYPE: c_uint = 0x807B;
const GL_VERTEX_ARRAY_STRIDE: c_uint = 0x807C;
const GL_VERTEX_ARRAY_POINTER: c_uint = 0x808E;
const GL_NORMAL_ARRAY_POINTER: c_uint = 0x808F;
const GL_COLOR_ARRAY_POINTER: c_uint = 0x8090;
const GL_TEXTURE_COORD_ARRAY_POINTER: c_uint = 0x8092;
const GL_POINT_SIZE_ARRAY_POINTER_OES: c_uint = 0x898C;
const GL_MATRIX_INDEX_ARRAY_POINTER_OES: c_uint = 0x8849;
const GL_WEIGHT_ARRAY_POINTER_OES: c_uint = 0x86AC;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetPlatformDisplay(
        platform: c_uint,
        native: *mut c_void,
        attribs: *const isize,
    ) -> Display;
    fn eglInitialize(dpy: Display, major: *mut i32, minor: *mut i32) -> c_uint;
    fn eglTerminate(dpy: Display) -> c_uint;
    fn eglBindAPI(api: c_uint) -> c_uint;
    fn eglChooseConfig(
        dpy: Display,
        attribs: *const i32,
        configs: *mut Handle,
        size: i32,
        n: *mut i32,
    ) -> c_uint;
    fn eglCreateContext(dpy: Display, config: Handle, share: Handle, attribs: *const i32)
    -> Handle;
    fn eglMakeCurrent(dpy: Display, draw: Handle, read: Handle, ctx: Handle) -> c_uint;
    fn eglDestroyContext(dpy: Display, ctx: Handle) -> c_uint;
    fn eglGetError() -> i32;
}

// GLES 1.1 core: the host libGLESv1_CM's
#[link(name = "GLESv1_CM")]
unsafe extern "C" {
    fn glGetIntegerv(name: c_uint, data: *mut i32);
    fn glGetPointerv(name: c_uint, data: *mut *mut c_void);
    fn glGetString(name: c_uint) -> *const c_char;
    fn glGetError() -> c_uint;
}

// AOSP GLES_CM/gl.cpp: C linkage, this crate's exports
unsafe extern "C" {
    fn glColorPointerBounds(size: i32, ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
    fn glNormalPointerBounds(ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
    fn glTexCoordPointerBounds(size: i32, ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
    fn glVertexPointerBounds(size: i32, ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
    fn glPointSizePointerOESBounds(ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
    fn glMatrixIndexPointerOESBounds(
        size: i32,
        ty: c_uint,
        stride: i32,
        ptr: *const c_void,
        count: i32,
    );
    fn glWeightPointerOESBounds(size: i32, ty: c_uint, stride: i32, ptr: *const c_void, count: i32);
}

/// The array pointer GL recorded for `name`.
///
/// # Safety
/// A GLES 1 context is current.
unsafe fn pointer(name: c_uint) -> *mut c_void {
    let mut p = null_mut();
    // SAFETY: the caller's context; one out pointer, a live local
    unsafe { glGetPointerv(name, &mut p) };
    p
}

#[test]
fn bounds_forward_to_the_plain_array_functions() {
    // distinct addresses inside one live array: GL only records them (client arrays are read at draw time, and nothing draws)
    let data = [0u8; 64];
    let at = |i: usize| data[i..].as_ptr().cast::<c_void>();
    // SAFETY: EGL / GLES calls with valid handles in the order the specs require; every out pointer is a live local
    unsafe {
        let dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, null_mut(), null());
        assert!(
            !dpy.is_null(),
            "no surfaceless EGL display: is Mesa's EGL vendor installed?"
        );
        assert_eq!(eglInitialize(dpy, null_mut(), null_mut()), EGL_TRUE);
        assert_eq!(eglBindAPI(EGL_OPENGL_ES_API), EGL_TRUE);
        let attribs = [
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES_BIT,
            EGL_SURFACE_TYPE,
            EGL_PBUFFER_BIT,
            EGL_NONE,
        ];
        let (mut config, mut n) = (null_mut(), 0);
        assert_eq!(
            eglChooseConfig(dpy, attribs.as_ptr(), &mut config, 1, &mut n),
            EGL_TRUE
        );
        assert_eq!(n, 1, "no GLES 1 config");
        let ctx = eglCreateContext(
            dpy,
            config,
            null_mut(),
            [EGL_CONTEXT_CLIENT_VERSION, 1, EGL_NONE].as_ptr(),
        );
        if ctx.is_null() {
            // a Mesa built without GLES 1 offers the config but fails the context with EGL_BAD_ALLOC (Debian 13's 25.0.7, measured 2026-11-01): symbol level only.
            // The seven are linked (this binary resolved them) and callable with no context current, where libglvnd dispatches GL calls to no-ops
            assert_eq!(eglGetError(), EGL_BAD_ALLOC, "no GLES 1 context");
            glVertexPointerBounds(3, GL_FLOAT, 12, at(1), 0);
            glColorPointerBounds(4, GL_UNSIGNED_BYTE, 0, at(2), 0);
            glNormalPointerBounds(GL_FLOAT, 0, at(3), 0);
            glTexCoordPointerBounds(2, GL_FLOAT, 0, at(4), 0);
            glPointSizePointerOESBounds(GL_FLOAT, 0, at(5), 0);
            glMatrixIndexPointerOESBounds(2, GL_UNSIGNED_BYTE, 0, at(6), 0);
            glWeightPointerOESBounds(2, GL_FLOAT, 0, at(7), 0);
            assert_eq!(eglTerminate(dpy), EGL_TRUE);
            return;
        }
        assert_eq!(eglMakeCurrent(dpy, null_mut(), null_mut(), ctx), EGL_TRUE);

        // size, type, stride and pointer reach glVertexPointer; the count (any value) does not matter
        glVertexPointerBounds(3, GL_FLOAT, 12, at(1), 1_000_000);
        let mut v = 0;
        glGetIntegerv(GL_VERTEX_ARRAY_SIZE, &mut v);
        assert_eq!(v, 3);
        glGetIntegerv(GL_VERTEX_ARRAY_TYPE, &mut v);
        assert_eq!(v, GL_FLOAT as i32);
        glGetIntegerv(GL_VERTEX_ARRAY_STRIDE, &mut v);
        assert_eq!(v, 12);
        assert_eq!(pointer(GL_VERTEX_ARRAY_POINTER).cast_const(), at(1));

        glColorPointerBounds(4, GL_UNSIGNED_BYTE, 0, at(2), 0);
        assert_eq!(pointer(GL_COLOR_ARRAY_POINTER).cast_const(), at(2));
        glNormalPointerBounds(GL_FLOAT, 0, at(3), 0);
        assert_eq!(pointer(GL_NORMAL_ARRAY_POINTER).cast_const(), at(3));
        glTexCoordPointerBounds(2, GL_FLOAT, 0, at(4), 0);
        assert_eq!(pointer(GL_TEXTURE_COORD_ARRAY_POINTER).cast_const(), at(4));
        glPointSizePointerOESBounds(GL_FLOAT, 0, at(5), 0);
        assert_eq!(pointer(GL_POINT_SIZE_ARRAY_POINTER_OES).cast_const(), at(5));
        assert_eq!(glGetError(), GL_NO_ERROR);

        // OES_matrix_palette is optional in GLES 1.1; the two go through this crate's eglGetProcAddress shells, so they are observable only where the driver offers it
        let extensions = CStr::from_ptr(glGetString(GL_EXTENSIONS)).to_string_lossy();
        if extensions.split(' ').any(|e| e == "GL_OES_matrix_palette") {
            glMatrixIndexPointerOESBounds(2, GL_UNSIGNED_BYTE, 0, at(6), 0);
            assert_eq!(
                pointer(GL_MATRIX_INDEX_ARRAY_POINTER_OES).cast_const(),
                at(6)
            );
            glWeightPointerOESBounds(2, GL_FLOAT, 0, at(7), 0);
            assert_eq!(pointer(GL_WEIGHT_ARRAY_POINTER_OES).cast_const(), at(7));
            assert_eq!(glGetError(), GL_NO_ERROR);
        }

        assert_eq!(
            eglMakeCurrent(dpy, null_mut(), null_mut(), null_mut()),
            EGL_TRUE
        );
        assert_eq!(eglDestroyContext(dpy, ctx), EGL_TRUE);
        assert_eq!(eglTerminate(dpy), EGL_TRUE);
    }
}
