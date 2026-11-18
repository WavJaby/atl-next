// SPDX-License-Identifier: MIT
//! `libEGL_atl.so`: the GLES driver AOSP's libEGL fronts load in Loader.cpp's three-library form (`libEGL_<ro.hardware.egl>.so`, `libGLESv1_CM_<..>.so`, `libGLESv2_<..>.so`). The same file is also placed as `libGLESv2_atl.so`, so the ES2 table comes from this driver: egl* by dlsym, then this driver's eglGetProcAddress; every gl* through eglGetProcAddress, so this library exports no gl* name.
//! `libGLESv1_CM_atl.so` is ANGLE's `libGLESv1_CM_angle.so` (environment-side): libEGL's ES1 table points straight at ANGLE, and ES1 contexts are ANGLE contexts embedded here ([`es1`]).
//! ES2/ES3 contexts render through the host's system EGL (atl-egl-host) on one device, and the driver hands out the host's own handles (display, configs, contexts, images, syncs, pbuffers): every entry it does not implement is the host's, passed through by eglGetProcAddress.
//! Implemented here, because the host has no Android window platform: window surfaces ([`surface`]: dequeue -> EGLImage -> FBO, swap = native fence -> queueBuffer),
//! configs that say they render to windows and to ES1, `EGL_ANDROID_image_native_buffer`, and the GL entries that make framebuffer 0 the surface's FBO ([`fbo0`]).
//! Orientation: a window buffer is read top-down and stored so, by one of two capability tiers probed at load (driver.rs `Orientation`: `GL_MESA_framebuffer_flip_y` FBOs, else a vertical-flip blit at swap); no buffer transform is set and the app's own passes through.

pub mod abi {
    //! The C interface, generated from AOSP's headers: EGL / GLES as atl-egl-host types them (one typing per entry), the window records and ANGLE's EGL here (build.rs).
    pub use atl_egl_host::ffi::abi::{egl, gles};
    #[allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        clippy::all
    )]
    pub mod window {
        include!(concat!(env!("OUT_DIR"), "/window.rs"));
    }
    #[allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        clippy::all
    )]
    pub mod angle {
        include!(concat!(env!("OUT_DIR"), "/angle.rs"));
    }
}

mod driver;
mod es1;
mod es1_ext;
mod exports;
mod fbo0;
mod readback_stats;
mod surface;
mod window;
