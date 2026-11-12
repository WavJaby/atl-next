// SPDX-License-Identifier: MIT
//! The process's one driver instance: the host display it renders on, the host entries it calls, the window surfaces it made, the per-thread error.
//! Device: the EGL device of the DRM node gralloc allocates on, as atld names it in the process's properties (atl_gralloc::node, read through bionic's `__system_property_get` in the process's global scope); on the trial tier a node without an EGL device (the kernel's vgem on WSL) only allocates and the software device renders.
//! Rendering stays on that device: P0a measured a LINEAR buffer from one GPU failing to render on the other in both directions (owner decision 2026-11-09: no cross-GPU).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use atl_egl_host::{Device, Display, Host};
use atl_gralloc::format::{self, RGBA_8888, RGBX_8888};
use atl_gralloc::handle::DeviceNumber;
use atl_gralloc::node::{self, NodeSource};

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
        eglGetConfigs,
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
        glBlitFramebuffer,
        glIsEnabled,
        glEnable,
        glDisable,
    }
);

/// The GL extension of the fast orientation tier: surface FBOs render top-down, as a window buffer is read (Mesa's own Android platform renders window surfaces flipped the same way).
pub const FLIP_Y: &str = "GL_MESA_framebuffer_flip_y";

/// How a window buffer comes to hold the frame top-down, as its consumer reads it; either way no buffer transform is set, the app's own passes through, and the stored rows are the same.
/// One concept, two capability tiers (chosen by [`Orientation::probe`] on the host's GL extensions, never by vendor): `Blit` is the baseline every host runs, `FlipY` the fast path.
#[derive(Clone, Copy)]
pub enum Orientation {
    /// Fast tier: the buffer's FBO is framebuffer 0 and renders top-down through [`FLIP_Y`]; no per-frame copy.
    FlipY(g::glFramebufferParameteriMESA),
    /// Baseline: framebuffer 0 is a per-context back FBO rendering bottom-up as GL does; swap copies it into the buffer with one vertical-flip blit.
    Blit,
}

impl Orientation {
    /// The tier for a host whose GL extension string is `ext`: `FlipY` when it lists [`FLIP_Y`] and its entry resolves, else `Blit`.
    /// cfg `atl_egl_tier_blit` (build.rs `TIERS`, `ATL_EGL_TIER=blit`) reads the extension as absent, so a gate runs the baseline on a flip-y host through this same probe.
    fn probe(host: &Host, ext: &str) -> (Self, &'static str) {
        if cfg!(atl_egl_tier_blit) {
            return (Self::Blit, "blit (build ATL_EGL_TIER=blit)");
        }
        if !ext.split_whitespace().any(|x| x == FLIP_Y) {
            return (Self::Blit, "blit (host lacks GL_MESA_framebuffer_flip_y)");
        }
        let p = host.proc_address(c"glFramebufferParameteriMESA");
        if p.is_null() {
            return (Self::Blit, "blit (glFramebufferParameteriMESA unresolved)");
        }
        // SAFETY: the host's non-null address for this registry name, whose signature is the AOSP header's (the alias).
        let f = unsafe { std::mem::transmute::<*const c_void, g::glFramebufferParameteriMESA>(p) };
        (Self::FlipY(f), "flip_y (GL_MESA_framebuffer_flip_y)")
    }
}

/// Host display extensions passed on: what holds unchanged for the driver's own objects too (images, syncs, contexts, surfaceless).
/// Left out: surface extensions the host implements for its own surfaces only (partial_update, buffer_age, swap_buffers_with_damage, mutable_render_buffer, gl_colorspace).
const PASSED_EXTENSIONS: &[&str] = &[
    "EGL_ANDROID_native_fence_sync",
    "EGL_EXT_create_context_robustness",
    // ANGLE's DisplayEGL reads EGL_COLOR_COMPONENT_TYPE_EXT of each native config only when this is advertised; without it every config is FIXED and a 16-bit float config passes a default request
    "EGL_EXT_pixel_format_float",
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
    /// The orientation tier window surfaces run, probed once at load.
    pub orientation: Orientation,
    extensions: CString,
    /// Window surfaces by handle (the boxed surface's address).
    pub surfaces: Mutex<HashMap<usize, Arc<Mutex<WindowSurface>>>>,
    /// Contexts already made current once: EGL sets viewport and scissor to the surface only the first time.
    pub seen_contexts: Mutex<HashSet<usize>>,
    /// The device rendered on: its render node / DRM file, or `software`.
    device: String,
    /// The st_rdev every imported buffer must carry: gralloc's node, whose EGL device this is (on the software fallback of the trial tier: the allocation-only node, vgem, the software device maps its buffers).
    pub buffer_device: DeviceNumber,
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
        let (display, device, buffer_device) = choose_display(&host)?;
        let (orientation, tier) = {
            // GL extensions need a current context: a throwaway one, released before any app context exists (the driver loads on the first eglGetDisplay)
            let ctx = display.create_context()?;
            let gl = ctx.make_current()?;
            Orientation::probe(&host, &gl.string(atl_egl_host::ffi::GL_EXTENSIONS))
        };
        #[allow(
            clippy::print_stderr,
            reason = "one line, once per process: which tier ran"
        )]
        {
            eprintln!("libGLES_atl: window tier {tier}");
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
            orientation,
            extensions: CString::new(ext.join(" "))?,
            surfaces: Mutex::new(HashMap::new()),
            seen_contexts: Mutex::new(HashSet::new()),
            device,
            buffer_device,
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

    /// The Android format a config maps to ([`FORMATS`] on its color layout), else none: the config is not handed out.
    pub fn android_format(&self, config: e::EGLConfig) -> Option<i32> {
        let size = |a| self.host_attrib(config, a);
        let rgba = [
            size(e::EGL_RED_SIZE)?,
            size(e::EGL_GREEN_SIZE)?,
            size(e::EGL_BLUE_SIZE)?,
            size(e::EGL_ALPHA_SIZE)?,
        ];
        // a host without EGL_EXT_pixel_format_float refuses the attribute (host_attrib clears the refusal): every config is FIXED
        let float = self.host_attrib(config, e::EGL_COLOR_COMPONENT_TYPE_EXT)
            == Some(e::EGL_COLOR_COMPONENT_TYPE_FLOAT_EXT);
        format_of(rgba, float)
    }

    /// The HAL format a window surface of `config` gets: [`Self::android_format`] on a config the host renders off-screen (PBUFFER), and only a format atl-gralloc has a row for, so a window never asks the allocator for a buffer it refuses.
    pub fn window_format(&self, config: e::EGLConfig) -> Option<i32> {
        let st = self.host_attrib(config, e::EGL_SURFACE_TYPE)?;
        if st & e::EGL_PBUFFER_BIT == 0 {
            return None;
        }
        self.android_format(config)
            .filter(|&f| format::lookup(f).is_some())
    }
}

/// A color layout: R, G, B, A sizes in bits, and the component type (`float`: EGL_COLOR_COMPONENT_TYPE_FLOAT_EXT, else FIXED).
struct Layout {
    rgba: [i32; 4],
    float: bool,
    format: i32,
}

/// `PixelFormat` values with no atl-gralloc row (hardware/interfaces/graphics/common/aidl PixelFormat.aidl). A config mapped to one is listed by eglGetConfigs / eglChooseConfig, but no window gets it.
const RGB_565: i32 = 0x4;
const RGBA_FP16: i32 = 0x16;
const RGBA_1010102: i32 = 0x2B;

/// The Android formats a config can map to, by its color layout: a host config matching no row is not handed out.
/// Rows: the channel sizes of each format's packing. AOSP's getNativePixelFormat (egl_platform_entries.cpp) maps the 16-bit FIXED layout to RGBA_1010102 by a colour-depth test; no row here, so those configs are not listed.
const FORMATS: &[Layout] = &[
    // 4 x 8 bits: a 32-bit pixel with alpha
    Layout {
        rgba: [8, 8, 8, 8],
        float: false,
        format: RGBA_8888,
    },
    // RGBA_8888 without alpha: byte 3 is padding
    Layout {
        rgba: [8, 8, 8, 0],
        float: false,
        format: RGBX_8888,
    },
    // 5 + 6 + 5 bits in 16, no alpha
    Layout {
        rgba: [5, 6, 5, 0],
        float: false,
        format: RGB_565,
    },
    // four 16-bit half floats: a 64-bit pixel
    Layout {
        rgba: [16, 16, 16, 16],
        float: true,
        format: RGBA_FP16,
    },
    // 10 + 10 + 10 + 2 in 32 bits
    Layout {
        rgba: [10, 10, 10, 2],
        float: false,
        format: RGBA_1010102,
    },
];

/// The Android format of a color layout: its [`FORMATS`] row, if any.
fn format_of(rgba: [i32; 4], float: bool) -> Option<i32> {
    FORMATS
        .iter()
        .find(|l| l.rgba == rgba && l.float == float)
        .map(|l| l.format)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values from PixelFormat.aidl (RGBA_8888 0x1, RGBX_8888 0x2, RGB_565 0x4, RGBA_FP16 0x16, RGBA_1010102 0x2B) and the channel sizes of each packing; not from the table.
    #[test]
    fn each_packing_maps_to_its_format() {
        assert_eq!(format_of([8, 8, 8, 8], false), Some(0x1));
        assert_eq!(format_of([8, 8, 8, 0], false), Some(0x2));
        assert_eq!(format_of([5, 6, 5, 0], false), Some(0x4));
        assert_eq!(format_of([16, 16, 16, 16], true), Some(0x16));
        assert_eq!(format_of([10, 10, 10, 2], false), Some(0x2B));
    }

    /// Layouts that are no Android format: the host's 16-bit FIXED configs (ids 51-60 on the WSL llvmpipe host), RGBA_5551, R_8, and a float layout with FIXED sizes.
    #[test]
    fn other_layouts_map_to_none() {
        assert_eq!(format_of([16, 16, 16, 16], false), None);
        assert_eq!(format_of([5, 5, 5, 1], false), None);
        assert_eq!(format_of([8, 0, 0, 0], false), None);
        assert_eq!(format_of([8, 8, 8, 8], true), None);
    }
}

/// The EGL device of the node atld named (atl_gralloc::node): on the display tier exactly that node's device, else an error naming it; on the trial tier that node's device, else the software device (a node that only allocates, vgem).
/// Also the node's st_rdev: what every buffer this driver imports must carry, on every tier.
fn choose_display(host: &Arc<Host>) -> Result<(Display, String, DeviceNumber)> {
    let get = |name: &str| {
        property(name)?.with_context(|| {
            format!(
                "{name} is unset: no gralloc node in this process (atld names it with --gralloc)"
            )
        })
    };
    let node = get(node::RENDER_NODE_PROPERTY)?;
    let source = get(node::NODE_SOURCE_PROPERTY)?;
    let source = NodeSource::parse(&source)
        .with_context(|| format!("{}={source}: unknown", node::NODE_SOURCE_PROPERTY))?;
    let rdev = node::device_number(std::path::Path::new(&node))
        .with_context(|| format!("gralloc's node {node}"))?;
    if let Some((display, name)) = host.display_on_node(&node)? {
        return Ok((display, name, rdev));
    }
    match source {
        NodeSource::Display => {
            let listed: Vec<_> = host.devices()?.into_iter().map(|d| d.render_node).collect();
            bail!("gralloc's node {node} (display) has no EGL device; devices {listed:?}")
        }
        NodeSource::Trial => {
            let software = host
                .devices()?
                .into_iter()
                .find(Device::is_software)
                .with_context(|| {
                    format!("no EGL device on gralloc's node {node} and no software device")
                })?;
            Ok((software.display()?, "software".to_owned(), rdev))
        }
    }
}

/// bionic `__system_property_get` as the process's global scope has it (atl-bionic in an app process; the driver probe exports its own): the value, None when unset or empty.
fn property(name: &str) -> Result<Option<String>> {
    type Get = unsafe extern "C" fn(*const std::ffi::c_char, *mut std::ffi::c_char) -> i32;
    // bionic's PROP_VALUE_MAX (sys/system_properties.h)
    const PROP_VALUE_MAX: usize = 92;
    let this = libloading::os::unix::Library::this();
    // SAFETY: the symbol, where present, is bionic's `int __system_property_get(const char*, char*)`.
    let get: libloading::os::unix::Symbol<Get> = unsafe { this.get(b"__system_property_get\0") }
        .context("__system_property_get not in the process: not an app process atld started")?;
    let cname = CString::new(name)?;
    let mut value = [0 as std::ffi::c_char; PROP_VALUE_MAX];
    // SAFETY: a NUL-terminated name and a PROP_VALUE_MAX buffer, bionic's contract.
    let len = unsafe { get(cname.as_ptr(), value.as_mut_ptr()) };
    if len <= 0 {
        return Ok(None);
    }
    // SAFETY: bionic NUL-terminates the value within the buffer.
    Ok(Some(
        unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    ))
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
