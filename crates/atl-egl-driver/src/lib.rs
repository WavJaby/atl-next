// SPDX-License-Identifier: MIT
//! `libGLES_atl.so`: the GLES driver AOSP's libEGL fronts load (Loader.cpp: `libGLES_<ro.hardware.egl>.so`; egl* by dlsym, then this driver's eglGetProcAddress; every gl* through eglGetProcAddress, so this library exports no gl* name).
//! It renders through the host's system EGL (atl-egl-host) on one device, and hands out the host's own handles (display, configs, contexts, images, syncs, pbuffers): every entry it does not implement is the host's, passed through by eglGetProcAddress.
//! Implemented here, because the host has no Android window platform: window surfaces ([`surface`]: dequeue -> EGLImage -> FBO, swap = native fence -> queueBuffer),
//! configs that say they render to windows, `EGL_ANDROID_image_native_buffer`, and the GL entries that make framebuffer 0 the surface's FBO ([`fbo0`]).
//! Orientation: a window buffer is read top-down and stored so, by one of two capability tiers probed at load (driver.rs `Orientation`: `GL_MESA_framebuffer_flip_y` FBOs, else a vertical-flip blit at swap); no buffer transform is set and the app's own passes through.

pub mod abi {
    //! The C interface, generated from AOSP's headers: EGL / GLES as atl-egl-host types them (one typing per entry), the window records here (build.rs).
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
}

mod driver;
mod exports;
mod fbo0;
mod surface;
mod window;
