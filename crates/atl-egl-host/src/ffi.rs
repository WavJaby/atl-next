// SPDX-License-Identifier: MIT
//! Raw EGL / GLES types, enums and the resolved function table, all from the ABI build.rs generates from AOSP's Khronos headers: one typing of every entry, shared with atl-egl-driver.
//! The generator resolves typedefs to their types and types macros as `i32`; the names and typed constants below only restate those (no value is written here).

use std::ffi::c_void;

pub mod abi {
    //! The C interface, generated from AOSP's headers (build.rs).
    #![allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        clippy::all
    )]
    pub mod egl {
        include!(concat!(env!("OUT_DIR"), "/egl.rs"));
        // handle names keep the handles apart in signatures (each is `void*` in eglplatform.h / egl.h, so the alias checks still hold)
        pub type EGLDisplay = *mut ::core::ffi::c_void;
        pub type EGLConfig = *mut ::core::ffi::c_void;
        pub type EGLSurface = *mut ::core::ffi::c_void;
        pub type EGLContext = *mut ::core::ffi::c_void;
        pub type EGLClientBuffer = *mut ::core::ffi::c_void;
        pub type EGLImageKHR = *mut ::core::ffi::c_void;
        pub type EGLNativeWindowType = *mut ANativeWindow;
    }
    pub mod gles {
        include!(concat!(env!("OUT_DIR"), "/gles.rs"));
    }
}

use abi::{egl as e, gles as g};

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

/// Typed constants: each the generated macro value in the C type its users take (EGLint attributes, EGLenum targets, GLenum names).
macro_rules! typed {
    ($m:ident: $($name:ident: $t:ty),* $(,)?) => {
        $(pub const $name: $t = $m::$name as $t;)*
    };
}

typed!(e:
    EGL_FALSE: EGLBoolean,
    EGL_SUCCESS: EGLint,
    EGL_NONE: EGLint,
    EGL_VENDOR: EGLint,
    EGL_VERSION: EGLint,
    EGL_EXTENSIONS: EGLint,
    EGL_HEIGHT: EGLint,
    EGL_WIDTH: EGLint,
    EGL_RENDERABLE_TYPE: EGLint,
    EGL_SURFACE_TYPE: EGLint,
    EGL_RED_SIZE: EGLint,
    EGL_GREEN_SIZE: EGLint,
    EGL_BLUE_SIZE: EGLint,
    EGL_ALPHA_SIZE: EGLint,
    EGL_OPENGL_ES2_BIT: EGLint,
    EGL_OPENGL_ES_API: EGLenum,
    EGL_CONTEXT_MAJOR_VERSION: EGLint,
    EGL_PLATFORM_DEVICE_EXT: EGLenum,
    EGL_PLATFORM_WAYLAND_KHR: EGLenum,
    EGL_DEVICE_EXT: EGLint,
    EGL_DRM_DEVICE_FILE_EXT: EGLint,
    EGL_DRM_RENDER_NODE_FILE_EXT: EGLint,
    EGL_LINUX_DMA_BUF_EXT: EGLenum,
    EGL_LINUX_DRM_FOURCC_EXT: EGLint,
    EGL_DMA_BUF_PLANE0_FD_EXT: EGLint,
    EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint,
    EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint,
    EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint,
    EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint,
    EGL_SYNC_NATIVE_FENCE_ANDROID: EGLenum,
    EGL_SYNC_NATIVE_FENCE_FD_ANDROID: EGLint,
    EGL_NO_NATIVE_FENCE_FD_ANDROID: EGLint,
    EGL_CONDITION_SATISFIED: EGLint,
    EGL_SYNC_FLUSH_COMMANDS_BIT: EGLint,
);

/// EGL_FOREVER (egl.h `0xFFFFFFFFFFFFFFFFull`): all bits of EGLTime; the generator's macros are `i32`, so it is spelled as the type's maximum.
pub const EGL_FOREVER: EGLuint64 = EGLuint64::MAX;

typed!(g:
    GL_NO_ERROR: GLenum,
    GL_VENDOR: GLenum,
    GL_RENDERER: GLenum,
    GL_VERSION: GLenum,
    GL_EXTENSIONS: GLenum,
    GL_TEXTURE_2D: GLenum,
    GL_TEXTURE_MIN_FILTER: GLenum,
    GL_TEXTURE_MAG_FILTER: GLenum,
    GL_NEAREST: GLint,
    GL_FRAMEBUFFER: GLenum,
    GL_COLOR_ATTACHMENT0: GLenum,
    GL_FRAMEBUFFER_COMPLETE: GLenum,
    GL_RENDERBUFFER: GLenum,
    GL_RGBA: GLenum,
    GL_UNSIGNED_BYTE: GLenum,
    GL_SCISSOR_TEST: GLenum,
    GL_COLOR_BUFFER_BIT: GLbitfield,
);

/// Declares the function table: one field per entry, typed by its generated alias, all resolved at load; a missing one is a load error naming it.
macro_rules! table {
    ($($m:ident: $($name:ident),*;)*) => {
        #[allow(non_snake_case)]
        pub struct Fns {
            $($(pub $name: $m::$name,)*)*
        }
        impl Fns {
            /// Resolves every entry through `resolve`, which returns null for a name the host lacks.
            pub fn resolve(mut resolve: impl FnMut(&std::ffi::CStr) -> *const c_void) -> Result<Self, String> {
                Ok(Self {
                    $($($name: {
                        let p = resolve(&std::ffi::CString::new(stringify!($name)).expect("no NUL in a name"));
                        if p.is_null() {
                            return Err(stringify!($name).to_owned());
                        }
                        // SAFETY: `p` is the host's non-null address for this registry name, whose C signature is the header's (the generated alias).
                        unsafe { std::mem::transmute::<*const c_void, $m::$name>(p) }
                    },)*)*
                })
            }
        }
    };
}

table! {
    e: eglGetError, eglQueryDevicesEXT, eglQueryDeviceStringEXT, eglGetPlatformDisplayEXT, eglQueryDisplayAttribEXT, eglInitialize, eglTerminate, eglQueryString, eglBindAPI, eglChooseConfig, eglCreateContext, eglDestroyContext, eglMakeCurrent, eglCreateImageKHR, eglDestroyImageKHR, eglQueryDmaBufModifiersEXT, eglCreateSyncKHR, eglDestroySyncKHR, eglClientWaitSyncKHR, eglWaitSyncKHR, eglDupNativeFenceFDANDROID;
    g: glGetError, glGetString, glGenTextures, glDeleteTextures, glBindTexture, glTexParameteri, glEGLImageTargetTexture2DOES, glEGLImageTargetRenderbufferStorageOES, glGenRenderbuffers, glDeleteRenderbuffers, glBindRenderbuffer, glFramebufferRenderbuffer, glGenFramebuffers, glDeleteFramebuffers, glBindFramebuffer, glFramebufferTexture2D, glCheckFramebufferStatus, glViewport, glEnable, glDisable, glScissor, glClearColor, glClear, glReadPixels, glFlush, glFinish;
}
