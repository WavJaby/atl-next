// SPDX-License-Identifier: MIT
//! `libGLES_atl.so`: the GLES driver AOSP's libEGL fronts load (Loader.cpp: `libGLES_<ro.hardware.egl>.so`; egl* by dlsym, then this driver's eglGetProcAddress; every gl* through eglGetProcAddress, so this library exports no gl* name).
//! It renders through the host's system EGL (atl-egl-host) on one device, and hands out the host's own handles (display, configs, contexts, images, syncs, pbuffers): every entry it does not implement is the host's, passed through by eglGetProcAddress.
//! Implemented here, because the host has no Android window platform: window surfaces ([`surface`]: dequeue -> EGLImage -> FBO, swap = native fence -> queueBuffer tagged `NATIVE_WINDOW_TRANSFORM_FLIP_V`),
//! configs that say they render to windows, `EGL_ANDROID_image_native_buffer`, and the GL entries that make framebuffer 0 the surface's FBO ([`fbo0`]).
//! Orientation: GL stores row 0 at the bottom, a window buffer is read top-down; the driver renders as GL does and tags every queued buffer FLIP_V, which the composer applies (one path, no per-frame blit).

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
        // the generator resolves typedefs to their types; these names keep the handles apart in our signatures (each is `void*` in eglplatform.h / egl.h, so the alias checks still hold)
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
    pub mod window {
        include!(concat!(env!("OUT_DIR"), "/window.rs"));
    }
}

mod driver;
mod exports;
mod fbo0;
mod surface;
mod window;
