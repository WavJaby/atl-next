// SPDX-License-Identifier: MIT
//! The extension exports as AOSP code calls them, on the host's EGL with no window system: a surfaceless Mesa display (EGL_MESA_platform_surfaceless), a GLES 2 context current without a surface (EGL_KHR_surfaceless_context).
//! Needs the host's EGL vendor library (Mesa's, software rendering suffices). Ground truth for the full API is dEQP (EGL, GLES); this checks the forwarding.
//! Constants are the Khronos registry's, as AOSP's pinned eglext.h / gl2.h define them.

use std::ffi::{c_char, c_uint, c_void};
use std::ptr::{null, null_mut};

// the exports live in the crate; nothing else of it is named here
use atl_opengl as _;

type Display = *mut c_void;
type Handle = *mut c_void;

const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: c_uint = 1;
const EGL_FALSE: c_uint = 0;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_OPENGL_ES_API: c_uint = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_PLATFORM_SURFACELESS_MESA: c_uint = 0x31DD;
const EGL_SYNC_FENCE_KHR: c_uint = 0x30F9;
const EGL_SYNC_FLUSH_COMMANDS_BIT_KHR: i32 = 0x0001;
const EGL_FOREVER_KHR: u64 = 0xFFFF_FFFF_FFFF_FFFF;
const EGL_CONDITION_SATISFIED_KHR: i32 = 0x30F6;
const EGL_GL_TEXTURE_2D_KHR: c_uint = 0x30B1;
const GL_TEXTURE_2D: c_uint = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: c_uint = 0x2801;
const GL_LINEAR: i32 = 0x2601;
const GL_RGBA: c_uint = 0x1908;
const GL_UNSIGNED_BYTE: c_uint = 0x1401;
const GL_NO_ERROR: c_uint = 0;

// core EGL 1.5: the host libEGL's own exports
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
    fn eglGetProcAddress(name: *const c_char) -> Option<unsafe extern "C" fn()>;
}

// core GLES 2.0: the host libGLESv2's
#[link(name = "GLESv2")]
unsafe extern "C" {
    fn glGenTextures(n: i32, textures: *mut c_uint);
    fn glBindTexture(target: c_uint, texture: c_uint);
    fn glTexParameteri(target: c_uint, name: c_uint, param: i32);
    fn glTexImage2D(
        target: c_uint,
        level: i32,
        internal: i32,
        w: i32,
        h: i32,
        border: i32,
        format: c_uint,
        ty: c_uint,
        data: *const c_void,
    );
    fn glGetError() -> c_uint;
    fn glFinish();
}

// extensions: this crate's exports, which the host libraries do not have
unsafe extern "C" {
    fn eglCreateSyncKHR(dpy: Display, ty: c_uint, attribs: *const i32) -> Handle;
    fn eglClientWaitSyncKHR(dpy: Display, sync: Handle, flags: i32, timeout: u64) -> i32;
    fn eglDestroySyncKHR(dpy: Display, sync: Handle) -> c_uint;
    fn eglCreateImageKHR(
        dpy: Display,
        ctx: Handle,
        target: c_uint,
        buffer: *mut c_void,
        attribs: *const i32,
    ) -> Handle;
    fn eglDestroyImageKHR(dpy: Display, image: Handle) -> c_uint;
    fn glEGLImageTargetTexture2DOES(target: c_uint, image: *mut c_void);
    fn eglStreamConsumerGLTextureExternalKHR(dpy: Display, stream: Handle) -> c_uint;
}

#[test]
fn extensions_forward_to_the_host_driver() {
    // SAFETY: EGL / GLES calls with valid handles in the order the specs require; every out pointer is a live local
    unsafe {
        let dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, null_mut(), null());
        assert!(
            !dpy.is_null(),
            "no surfaceless EGL display: is Mesa's EGL vendor installed?"
        );
        assert_eq!(eglInitialize(dpy, null_mut(), null_mut()), EGL_TRUE);
        assert_eq!(eglBindAPI(EGL_OPENGL_ES_API), EGL_TRUE);
        // EGL_SURFACE_TYPE defaults to EGL_WINDOW_BIT, which a surfaceless display has no config for
        let attribs = [
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_SURFACE_TYPE,
            EGL_PBUFFER_BIT,
            EGL_NONE,
        ];
        let (mut config, mut n) = (null_mut(), 0);
        assert_eq!(
            eglChooseConfig(dpy, attribs.as_ptr(), &mut config, 1, &mut n),
            EGL_TRUE
        );
        assert_eq!(n, 1);
        let ctx = eglCreateContext(
            dpy,
            config,
            null_mut(),
            [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE].as_ptr(),
        );
        assert!(!ctx.is_null());
        assert_eq!(eglMakeCurrent(dpy, null_mut(), null_mut(), ctx), EGL_TRUE);

        // EGL_KHR_fence_sync: a fence after no commands is signalled once flushed
        let sync = eglCreateSyncKHR(dpy, EGL_SYNC_FENCE_KHR, null());
        assert!(!sync.is_null());
        assert_eq!(
            eglClientWaitSyncKHR(dpy, sync, EGL_SYNC_FLUSH_COMMANDS_BIT_KHR, EGL_FOREVER_KHR),
            EGL_CONDITION_SATISFIED_KHR
        );
        assert_eq!(eglDestroySyncKHR(dpy, sync), EGL_TRUE);

        // EGL_KHR_gl_texture_2D_image + GL_OES_EGL_image: a texture's image bound to a second texture
        let mut tex = [0; 2];
        glGenTextures(2, tex.as_mut_ptr());
        glBindTexture(GL_TEXTURE_2D, tex[0]);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexImage2D(
            GL_TEXTURE_2D,
            0,
            GL_RGBA as i32,
            4,
            4,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            null(),
        );
        glFinish();
        // EGLClientBuffer of a GL texture = its name (EGL_KHR_gl_image)
        let image = eglCreateImageKHR(
            dpy,
            ctx,
            EGL_GL_TEXTURE_2D_KHR,
            tex[0] as usize as *mut c_void,
            [EGL_NONE].as_ptr(),
        );
        assert!(!image.is_null());
        glBindTexture(GL_TEXTURE_2D, tex[1]);
        glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, image);
        assert_eq!(glGetError(), GL_NO_ERROR);
        assert_eq!(eglDestroyImageKHR(dpy, image), EGL_TRUE);

        // EGL_KHR_stream_consumer_gltexture: Mesa implements no EGL streams, so the lookup finds nothing (the probe answers, else this checks nothing) and the shell returns the documented EGL_FALSE
        assert!(eglGetProcAddress(c"eglStreamConsumerGLTextureExternalKHR".as_ptr()).is_none());
        assert_eq!(
            eglStreamConsumerGLTextureExternalKHR(dpy, null_mut()),
            EGL_FALSE
        );

        assert_eq!(
            eglMakeCurrent(dpy, null_mut(), null_mut(), null_mut()),
            EGL_TRUE
        );
        assert_eq!(eglDestroyContext(dpy, ctx), EGL_TRUE);
        assert_eq!(eglTerminate(dpy), EGL_TRUE);
    }
}
