// SPDX-License-Identifier: MIT
//! Raw EGL / GLES types, enums and the resolved function table. Values from the Khronos registry headers (EGL/egl.h, eglext.h, GLES2/gl2.h, gl2ext.h); KHRONOS_APIENTRY is empty on Linux, so every entry is `extern "C"`.

use std::ffi::{c_char, c_void};

pub type EGLDisplay = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLSync = *mut c_void;
pub type EGLDevice = *mut c_void;
pub type EGLint = i32;
pub type EGLenum = u32;
pub type EGLBoolean = u32;
pub type EGLuint64 = u64;
pub type GLenum = u32;
pub type GLuint = u32;
pub type GLint = i32;
pub type GLsizei = i32;
pub type GLfloat = f32;
pub type GLbitfield = u32;

pub const EGL_FALSE: EGLBoolean = 0;
pub const EGL_SUCCESS: EGLint = 0x3000;
pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_VENDOR: EGLint = 0x3053;
pub const EGL_VERSION: EGLint = 0x3054;
pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
pub const EGL_RED_SIZE: EGLint = 0x3024;
pub const EGL_GREEN_SIZE: EGLint = 0x3023;
pub const EGL_BLUE_SIZE: EGLint = 0x3022;
pub const EGL_ALPHA_SIZE: EGLint = 0x3021;
pub const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
pub const EGL_OPENGL_ES_API: EGLenum = 0x30A0;
pub const EGL_CONTEXT_MAJOR_VERSION: EGLint = 0x3098;
pub const EGL_PLATFORM_DEVICE_EXT: EGLenum = 0x313F;
pub const EGL_DRM_DEVICE_FILE_EXT: EGLint = 0x3233;
pub const EGL_DRM_RENDER_NODE_FILE_EXT: EGLint = 0x3377;
pub const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
pub const EGL_SYNC_NATIVE_FENCE_ANDROID: EGLenum = 0x3144;
pub const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: EGLint = 0x3145;
pub const EGL_NO_NATIVE_FENCE_FD_ANDROID: EGLint = -1;
pub const EGL_CONDITION_SATISFIED: EGLint = 0x30F6;
pub const EGL_SYNC_FLUSH_COMMANDS_BIT: EGLint = 0x0001;
pub const EGL_FOREVER: EGLuint64 = u64::MAX;

pub const GL_NO_ERROR: GLenum = 0;
pub const GL_VENDOR: GLenum = 0x1F00;
pub const GL_RENDERER: GLenum = 0x1F01;
pub const GL_VERSION: GLenum = 0x1F02;
pub const GL_TEXTURE_2D: GLenum = 0x0DE1;
pub const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
pub const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
pub const GL_NEAREST: GLint = 0x2600;
pub const GL_FRAMEBUFFER: GLenum = 0x8D40;
pub const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
pub const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
pub const GL_RENDERBUFFER: GLenum = 0x8D41;
pub const GL_RGBA: GLenum = 0x1908;
pub const GL_UNSIGNED_BYTE: GLenum = 0x1401;
pub const GL_SCISSOR_TEST: GLenum = 0x0C11;
pub const GL_COLOR_BUFFER_BIT: GLbitfield = 0x4000;

/// Declares the function table: one field per entry, all resolved at load, a missing one is a load error naming it.
macro_rules! table {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        pub struct Fns {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        impl Fns {
            /// Resolves every entry through `resolve`, which returns null for a name the host lacks.
            pub fn resolve(mut resolve: impl FnMut(&std::ffi::CStr) -> *const c_void) -> Result<Self, String> {
                Ok(Self {
                    $($name: {
                        let p = resolve(&std::ffi::CString::new(stringify!($name)).expect("no NUL in a name"));
                        if p.is_null() {
                            return Err(stringify!($name).to_owned());
                        }
                        // SAFETY: `p` is the host's non-null address for this registry name, whose C signature is the declared one (Khronos headers).
                        unsafe { std::mem::transmute::<*const c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    },)*
                })
            }
        }
    };
}

table! {
    eglGetError: fn() -> EGLint;
    eglQueryDevicesEXT: fn(EGLint, *mut EGLDevice, *mut EGLint) -> EGLBoolean;
    eglQueryDeviceStringEXT: fn(EGLDevice, EGLint) -> *const c_char;
    eglGetPlatformDisplayEXT: fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay;
    eglInitialize: fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean;
    eglTerminate: fn(EGLDisplay) -> EGLBoolean;
    eglQueryString: fn(EGLDisplay, EGLint) -> *const c_char;
    eglBindAPI: fn(EGLenum) -> EGLBoolean;
    eglChooseConfig: fn(EGLDisplay, *const EGLint, *mut EGLConfig, EGLint, *mut EGLint) -> EGLBoolean;
    eglCreateContext: fn(EGLDisplay, EGLConfig, EGLContext, *const EGLint) -> EGLContext;
    eglDestroyContext: fn(EGLDisplay, EGLContext) -> EGLBoolean;
    eglMakeCurrent: fn(EGLDisplay, EGLSurface, EGLSurface, EGLContext) -> EGLBoolean;
    eglCreateImageKHR: fn(EGLDisplay, EGLContext, EGLenum, *mut c_void, *const EGLint) -> EGLImage;
    eglDestroyImageKHR: fn(EGLDisplay, EGLImage) -> EGLBoolean;
    eglQueryDmaBufModifiersEXT: fn(EGLDisplay, EGLint, EGLint, *mut u64, *mut EGLBoolean, *mut EGLint) -> EGLBoolean;
    eglCreateSyncKHR: fn(EGLDisplay, EGLenum, *const EGLint) -> EGLSync;
    eglDestroySyncKHR: fn(EGLDisplay, EGLSync) -> EGLBoolean;
    eglClientWaitSyncKHR: fn(EGLDisplay, EGLSync, EGLint, EGLuint64) -> EGLint;
    eglWaitSyncKHR: fn(EGLDisplay, EGLSync, EGLint) -> EGLint;
    eglDupNativeFenceFDANDROID: fn(EGLDisplay, EGLSync) -> EGLint;
    glGetError: fn() -> GLenum;
    glGetString: fn(GLenum) -> *const u8;
    glGenTextures: fn(GLsizei, *mut GLuint);
    glDeleteTextures: fn(GLsizei, *const GLuint);
    glBindTexture: fn(GLenum, GLuint);
    glTexParameteri: fn(GLenum, GLenum, GLint);
    glEGLImageTargetTexture2DOES: fn(GLenum, EGLImage);
    glEGLImageTargetRenderbufferStorageOES: fn(GLenum, EGLImage);
    glGenRenderbuffers: fn(GLsizei, *mut GLuint);
    glDeleteRenderbuffers: fn(GLsizei, *const GLuint);
    glBindRenderbuffer: fn(GLenum, GLuint);
    glFramebufferRenderbuffer: fn(GLenum, GLenum, GLenum, GLuint);
    glGenFramebuffers: fn(GLsizei, *mut GLuint);
    glDeleteFramebuffers: fn(GLsizei, *const GLuint);
    glBindFramebuffer: fn(GLenum, GLuint);
    glFramebufferTexture2D: fn(GLenum, GLenum, GLenum, GLuint, GLint);
    glCheckFramebufferStatus: fn(GLenum) -> GLenum;
    glViewport: fn(GLint, GLint, GLsizei, GLsizei);
    glEnable: fn(GLenum);
    glDisable: fn(GLenum);
    glScissor: fn(GLint, GLint, GLsizei, GLsizei);
    glClearColor: fn(GLfloat, GLfloat, GLfloat, GLfloat);
    glClear: fn(GLbitfield);
    glReadPixels: fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void);
    glFlush: fn();
    glFinish: fn();
}
