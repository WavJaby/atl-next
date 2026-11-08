// SPDX-License-Identifier: MIT
//! The process's one driver instance: the host display it renders on, the host entries it calls, the window surfaces it made, the per-thread error.
//! Device: the EGL device of the DRM node gralloc allocates on (gralloc's own trial, atl-gralloc-service `Backend::start`), else the host's software device -- a node without an EGL device (the kernel's vgem on WSL) only allocates.
//! Rendering stays on that device: P0a measured a LINEAR buffer from one GPU failing to render on the other in both directions (owner decision 2026-11-09: no cross-GPU).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow};
use atl_egl_host::{Display, Host};
use atl_gralloc::format::{RGBA_8888, RGBX_8888};
use atl_gralloc_service::backend::{Backend, DRM_DIR};

use crate::abi::egl as e;
use crate::abi::gles as g;
use crate::surface::WindowSurface;

/// Declares a table of host entries typed by the generated header aliases, resolved through the host's eglGetProcAddress at load.
macro_rules! host_table {
    ($name:ident, $m:ident { $($f:ident),* $(,)? }) => {
        #[allow(non_snake_case)]
        pub struct $name {
            $(pub $f: $m::$f,)*
        }
        impl $name {
            fn resolve(host: &Host) -> Result<Self> {
                Ok(Self {
                    $($f: {
                        let p = host.proc_address(&CString::new(stringify!($f)).expect("no NUL in a name"));
                        if p.is_null() {
                            return Err(anyhow!("host lacks {}", stringify!($f)));
                        }
                        // SAFETY: `p` is the host's non-null address for this registry name, whose signature is the AOSP header's (the alias).
                        unsafe { std::mem::transmute::<*const c_void, $m::$f>(p) }
                    },)*
                })
            }
        }
    };
}

host_table!(
    HostEgl,
    e {
        eglGetError,
        eglQueryString,
        eglChooseConfig,
        eglGetConfigAttrib,
        eglDestroySurface,
        eglQuerySurface,
        eglSurfaceAttrib,
        eglMakeCurrent,
        eglSwapBuffers,
        eglSwapInterval,
        eglCreateImageKHR,
    }
);

host_table!(
    HostGl,
    g {
        glBindFramebuffer,
        glGetIntegerv,
        glInvalidateFramebuffer,
        glInvalidateSubFramebuffer,
        glDiscardFramebufferEXT,
        glDrawBuffers,
        glReadBuffer,
        glDeleteFramebuffers,
        glIsFramebuffer,
        glGenFramebuffers,
        glGenRenderbuffers,
        glBindRenderbuffer,
        glRenderbufferStorage,
        glFramebufferRenderbuffer,
        glFramebufferTexture2D,
        glCheckFramebufferStatus,
        glGenTextures,
        glBindTexture,
        glTexParameteri,
        glDeleteTextures,
        glDeleteRenderbuffers,
        glEGLImageTargetTexture2DOES,
        glViewport,
        glScissor,
        glFinish,
        glGetString,
        glFramebufferParameteriMESA,
    }
);

/// The GL extension window surfaces need: FBOs render top-down, as a window buffer is read (Mesa's own Android platform renders window surfaces flipped the same way), so no buffer transform is set and the app's own passes through.
/// Without it the driver refuses to load (hosts lacking it: NVIDIA, unsupported until gralloc modifiers land).
pub const FLIP_Y: &str = "GL_MESA_framebuffer_flip_y";

/// Host display extensions passed on: what holds unchanged for the driver's own objects too (images, syncs, contexts, surfaceless).
/// Left out: surface extensions the host implements for its own surfaces only (partial_update, buffer_age, swap_buffers_with_damage, mutable_render_buffer, gl_colorspace).
const PASSED_EXTENSIONS: &[&str] = &[
    "EGL_ANDROID_native_fence_sync",
    "EGL_EXT_create_context_robustness",
    "EGL_IMG_context_priority",
    "EGL_KHR_config_attribs",
    "EGL_KHR_create_context",
    "EGL_KHR_create_context_no_error",
    "EGL_KHR_fence_sync",
    "EGL_KHR_gl_renderbuffer_image",
    "EGL_KHR_gl_texture_2D_image",
    "EGL_KHR_image",
    "EGL_KHR_image_base",
    "EGL_KHR_no_config_context",
    "EGL_KHR_reusable_sync",
    "EGL_KHR_surfaceless_context",
    "EGL_KHR_wait_sync",
];
/// Implemented by the driver itself.
const OWN_EXTENSIONS: &[&str] = &[
    "EGL_ANDROID_image_native_buffer",
    "EGL_ANDROID_recordable",
    "EGL_KHR_swap_buffers_with_damage",
];
/// Version reported to the fronts: 1.4, so libEGL uses eglGetDisplay / eglCreateWindowSurface (egl_display.cpp) rather than the 1.5 platform entries this driver does not take.
const VERSION: &CStr = c"1.4 atl";

pub struct Driver {
    pub display: Display,
    pub egl: HostEgl,
    pub gl: HostGl,
    /// The host has EGL_ANDROID_native_fence_sync: swap hands the consumer a sync_file instead of finishing on the CPU.
    pub native_fence: bool,
    extensions: CString,
    /// Window surfaces by handle (the boxed surface's address).
    pub surfaces: Mutex<HashMap<usize, Arc<Mutex<WindowSurface>>>>,
    /// Contexts already made current once: EGL sets viewport and scissor to the surface only the first time.
    pub seen_contexts: Mutex<HashSet<usize>>,
    /// The device rendered on: its render node / DRM file, or `software`.
    device: String,
    renderer_logged: AtomicBool,
}

// SAFETY: the host display is a process-global EGL handle usable from any thread (EGL 1.5 §3.2); the tables are function pointers; the maps are behind mutexes.
unsafe impl Send for Driver {}
// SAFETY: as Send.
unsafe impl Sync for Driver {}

static DRIVER: OnceLock<Option<Driver>> = OnceLock::new();

/// The driver, loaded on first use; `None` when the host cannot carry it (logged once on stderr, which the app's log captures).
pub fn get() -> Option<&'static Driver> {
    DRIVER
        .get_or_init(|| match Driver::load() {
            Ok(d) => Some(d),
            Err(err) => {
                // the process's stderr is its log (atl-broker); without this line every egl* call would just fail
                #[allow(clippy::print_stderr, reason = "one line, once per process")]
                {
                    eprintln!("libGLES_atl: no host EGL device: {err:#}");
                }
                None
            }
        })
        .as_ref()
}

impl Driver {
    fn load() -> Result<Self> {
        let host = Host::load()?;
        let (display, device) = choose_display(&host)?;
        {
            // GL extensions need a current context: a throwaway one, released before any app context exists (the driver loads on the first eglGetDisplay)
            let ctx = display.create_context()?;
            let gl = ctx.make_current()?;
            let ext = gl.string(atl_egl_host::ffi::GL_EXTENSIONS);
            anyhow::ensure!(
                ext.split_whitespace().any(|x| x == FLIP_Y),
                "host GL on {device} lacks {FLIP_Y} (window surfaces render top-down through it)"
            );
        }
        let egl = HostEgl::resolve(&host)?;
        let gl = HostGl::resolve(&host)?;
        let native_fence = display.has_extension("EGL_ANDROID_native_fence_sync");
        let mut ext: Vec<&str> = PASSED_EXTENSIONS
            .iter()
            .copied()
            .filter(|x| display.has_extension(x))
            .chain(OWN_EXTENSIONS.iter().copied())
            .collect();
        ext.sort_unstable();
        Ok(Self {
            display,
            egl,
            gl,
            native_fence,
            extensions: CString::new(ext.join(" "))?,
            surfaces: Mutex::new(HashMap::new()),
            seen_contexts: Mutex::new(HashSet::new()),
            device,
            renderer_logged: AtomicBool::new(false),
        })
    }

    /// On the first make-current with a context: one stderr line (the app's log) naming the device and `GL_RENDERER`, so a run proves which GPU rendered.
    pub fn log_renderer_once(&self) {
        if self.renderer_logged.swap(true, Ordering::Relaxed) {
            return;
        }
        // SAFETY: a context is current on this thread (the caller's make-current succeeded).
        let p = unsafe { (self.gl.glGetString)(atl_egl_host::ffi::GL_RENDERER) };
        let renderer = if p.is_null() {
            "none".into()
        } else {
            // SAFETY: GL returns a static NUL-terminated string for GL_RENDERER.
            unsafe { CStr::from_ptr(p.cast()) }.to_string_lossy()
        };
        #[allow(clippy::print_stderr, reason = "one line, once per process")]
        {
            eprintln!("libGLES_atl: device {} GL_RENDERER {renderer}", self.device);
        }
    }

    pub fn extensions(&self) -> &CStr {
        &self.extensions
    }

    pub fn version(&self) -> &'static CStr {
        VERSION
    }

    pub fn is_display(&self, dpy: e::EGLDisplay) -> bool {
        dpy == self.display.raw()
    }

    pub fn surface(&self, s: e::EGLSurface) -> Option<Arc<Mutex<WindowSurface>>> {
        self.surfaces.lock().ok()?.get(&(s as usize)).cloned()
    }

    /// A host config attribute; `None` when the host refuses it (its error cleared).
    pub fn host_attrib(&self, config: e::EGLConfig, attr: i32) -> Option<i32> {
        let mut v = 0;
        // SAFETY: the host display, a host config handle the caller got from the host, a valid out-pointer.
        let ok = unsafe { (self.egl.eglGetConfigAttrib)(self.display.raw(), config, attr, &mut v) };
        if ok == 0 {
            // SAFETY: no arguments; clears the refusal so it is not reported later as another call's error.
            unsafe { (self.egl.eglGetError)() };
            return None;
        }
        Some(v)
    }

    /// The HAL pixel format a window surface of `config` gets (the config's `EGL_NATIVE_VISUAL_ID`): RGBA / RGBX 8888 on a config the host renders off-screen (PBUFFER), else none.
    pub fn window_format(&self, config: e::EGLConfig) -> Option<i32> {
        let st = self.host_attrib(config, e::EGL_SURFACE_TYPE)?;
        if st & e::EGL_PBUFFER_BIT == 0 {
            return None;
        }
        let size = |a| self.host_attrib(config, a);
        match (
            size(e::EGL_RED_SIZE)?,
            size(e::EGL_GREEN_SIZE)?,
            size(e::EGL_BLUE_SIZE)?,
            size(e::EGL_ALPHA_SIZE)?,
        ) {
            (8, 8, 8, 8) => Some(RGBA_8888),
            (8, 8, 8, 0) => Some(RGBX_8888),
            _ => None,
        }
    }
}

/// gralloc's node, then the EGL device on it, else the software device; with the device's name.
fn choose_display(host: &Arc<Host>) -> Result<(Display, String)> {
    let node = Backend::start(Path::new(DRM_DIR), None)
        .ok()
        .map(|b| b.node().to_string_lossy().into_owned());
    let devices = host.devices()?;
    let on_node = node.as_deref().and_then(|n| {
        devices
            .iter()
            .find(|d| d.render_node.as_deref() == Some(n) || d.drm_file.as_deref() == Some(n))
    });
    let device = on_node
        .or_else(|| devices.iter().find(|d| d.is_software()))
        .with_context(|| {
            format!("no EGL device on gralloc's node {node:?} and no software device")
        })?;
    let name = device
        .render_node
        .clone()
        .or_else(|| device.drm_file.clone())
        .unwrap_or_else(|| "software".to_owned());
    Ok((device.display()?, name))
}

thread_local! {
    /// The error of this thread's last failed driver call, until eglGetError reads it.
    static ERROR: Cell<i32> = const { Cell::new(e::EGL_SUCCESS) };
}

pub fn set_error(err: i32) {
    ERROR.with(|c| c.set(err));
}

/// This thread's driver error, cleared by reading it.
pub fn take_error() -> Option<i32> {
    ERROR.with(|c| {
        let v = c.replace(e::EGL_SUCCESS);
        (v != e::EGL_SUCCESS).then_some(v)
    })
}
