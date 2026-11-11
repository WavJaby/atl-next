// SPDX-License-Identifier: MIT
//! The host's system EGL behind safe types: what the GLES driver for AOSP's libEGL fronts renders through (replan 2026-11-09: no GPU driver shipped, the host's glvnd EGL does the work).
//! The device is chosen by probing EGL devices (`EGL_EXT_device_enumeration` + `EGL_EXT_platform_device`), each named by its DRM render node; Mesa's software device has none and carries `EGL_MESA_device_software`.
//! Buffers come from our gralloc as dma-bufs and are imported as EGLImages (`EGL_EXT_image_dma_buf_import_modifiers`), bound to a texture (`GL_OES_EGL_image`) and rendered through an FBO.
//! GPU completion crosses as a sync_file (`EGL_ANDROID_native_fence_sync`), the fence AOSP's queueBuffer / dequeueBuffer carry.
//! Displays are never terminated: EGL hands out one handle per device, so a terminate would pull it from every user in the process.

pub mod ffi;

use std::ffi::{CStr, c_void};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use ffi::*;

/// The glvnd dispatch library every desktop distribution ships.
pub const HOST_EGL: &str = "libEGL.so.1";
/// What marks Mesa's software (llvmpipe / softpipe) EGL device.
pub const SOFTWARE_DEVICE_EXT: &str = "EGL_MESA_device_software";

/// The loaded host EGL. Every entry is resolved through `eglGetProcAddress`, never by symbol name (see the crate's Cargo.toml).
pub struct Host {
    fns: Fns,
    gpa: GetProcAddress,
    _lib: libloading::Library,
}

type GetProcAddress = unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void;

impl Host {
    pub fn load() -> Result<Arc<Self>> {
        // SAFETY: loading the host's glvnd libEGL runs only its initialisers, which have no preconditions on the caller.
        let lib = unsafe { libloading::Library::new(HOST_EGL) }
            .with_context(|| format!("dlopen {HOST_EGL}"))?;
        // SAFETY: eglGetProcAddress has this signature (EGL 1.5 §3.10).
        let gpa: GetProcAddress = *unsafe { lib.get::<GetProcAddress>(b"eglGetProcAddress\0") }
            .context("eglGetProcAddress")?;
        // SAFETY: `name` is a valid NUL-terminated string for the call's duration.
        let fns = Fns::resolve(|name| unsafe { gpa(name.as_ptr()) })
            .map_err(|n| anyhow!("host EGL lacks {n}"))?;
        Ok(Arc::new(Self {
            fns,
            gpa,
            _lib: lib,
        }))
    }

    /// The host's address for `name` (eglGetProcAddress): what the GLES driver forwards every entry it does not implement to. Null when the host has none.
    pub fn proc_address(&self, name: &CStr) -> *const c_void {
        // SAFETY: `name` is NUL-terminated and lives for the call; eglGetProcAddress needs no display or context.
        unsafe { (self.gpa)(name.as_ptr()) }
    }

    /// The resolved host entries, for callers that pass handles through to the host unchanged (the GLES driver).
    pub fn fns(&self) -> &Fns {
        &self.fns
    }

    fn error(&self) -> EGLint {
        // SAFETY: eglGetError takes no arguments and is valid on any thread.
        unsafe { (self.fns.eglGetError)() }
    }

    fn egl_fail(&self, what: &str) -> anyhow::Error {
        anyhow!("{what}: EGL error {:#x}", self.error())
    }

    /// The client extensions (`EGL_NO_DISPLAY`).
    pub fn client_extensions(&self) -> String {
        // SAFETY: EGL_NO_DISPLAY + EGL_EXTENSIONS is the client-extension query (EGL_EXT_client_extensions); the result is null or a static string.
        cstr(unsafe { (self.fns.eglQueryString)(std::ptr::null_mut(), EGL_EXTENSIONS) })
    }

    /// Every EGL device the host lists.
    pub fn devices(self: &Arc<Self>) -> Result<Vec<Device>> {
        let mut n = 0;
        // SAFETY: a null array with max 0 asks only for the count, written to `n`.
        let ok = unsafe { (self.fns.eglQueryDevicesEXT)(0, std::ptr::null_mut(), &mut n) };
        ensure!(ok != EGL_FALSE, self.egl_fail("eglQueryDevicesEXT"));
        let mut raw = vec![std::ptr::null_mut(); usize::try_from(n)?];
        // SAFETY: `raw` holds `n` slots; EGL writes at most `n` and the count back to `n`.
        let ok = unsafe { (self.fns.eglQueryDevicesEXT)(n, raw.as_mut_ptr(), &mut n) };
        ensure!(ok != EGL_FALSE, self.egl_fail("eglQueryDevicesEXT"));
        raw.truncate(usize::try_from(n)?);
        Ok(raw
            .into_iter()
            .map(|d| {
                // SAFETY: `d` is a device EGL just listed; each query returns null or a string EGL owns for the device's lifetime (the process).
                let q = |name| unsafe { (self.fns.eglQueryDeviceStringEXT)(d, name) };
                let opt = |p: *const std::ffi::c_char| (!p.is_null()).then(|| cstr(p));
                let render_node = opt(q(EGL_DRM_RENDER_NODE_FILE_EXT));
                let drm_file = opt(q(EGL_DRM_DEVICE_FILE_EXT));
                let extensions = cstr(q(EGL_EXTENSIONS));
                // the query of an unsupported name leaves an error behind; clear it so the next real call's error is its own
                self.error();
                Device {
                    host: self.clone(),
                    raw: d,
                    render_node,
                    drm_file,
                    extensions,
                }
            })
            .collect())
    }

    /// The initialised display of the first EGL device on `node` (its render node or primary node path) that initialises, with the device's name (render node, else primary node).
    /// Several devices may name one node and not all initialise: glvnd lists a Mesa device for the NVIDIA node that fails eglInitialize (laptop 2026-11-11). None: no device names the node; an error: some do, none initialises.
    pub fn display_on_node(self: &Arc<Self>, node: &str) -> Result<Option<(Display, String)>> {
        let mut failures = Vec::new();
        for d in self.devices()?.into_iter().filter(|d| {
            d.render_node.as_deref() == Some(node) || d.drm_file.as_deref() == Some(node)
        }) {
            match d.display() {
                Ok(display) => {
                    let name = d
                        .render_node
                        .or(d.drm_file)
                        .unwrap_or_else(|| node.to_owned());
                    return Ok(Some((display, name)));
                }
                Err(e) => failures.push(format!("{e:#}")),
            }
        }
        ensure!(
            failures.is_empty(),
            "no EGL device on {node} initialises: {}",
            failures.join("; ")
        );
        Ok(None)
    }

    /// What the desktop's Wayland-platform display renders on, under this process's environment: the render node of its EGL device (`EGL_EXT_device_query` -> `EGL_DRM_RENDER_NODE_FILE_EXT`), None when that device has none (WSLg's d3d12, a software device), and its GL_RENDERER.
    /// The native display is EGL_DEFAULT_DISPLAY: the vendor's Wayland platform connects to the default compositor itself (`WAYLAND_DISPLAY`), so no Wayland call is made here. Only the Wayland platform expresses the desktop's GPU choice; device enumeration order is glvnd's vendor order (gpu-selection S1).
    pub fn wayland_display_node(self: &Arc<Self>) -> Result<WaylandNode> {
        let f = &self.fns;
        let attribs = [EGL_NONE];
        // SAFETY: a null native display is EGL_DEFAULT_DISPLAY, which EGL_KHR_platform_wayland lets the implementation resolve; NONE-terminated attributes.
        let raw = unsafe {
            (f.eglGetPlatformDisplayEXT)(
                EGL_PLATFORM_WAYLAND_KHR,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        ensure!(
            !raw.is_null(),
            self.egl_fail("eglGetPlatformDisplayEXT(wayland)")
        );
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `raw` is a display EGL returned; the out-pointers are valid.
        let ok = unsafe { (f.eglInitialize)(raw, &mut major, &mut minor) };
        ensure!(ok != EGL_FALSE, self.egl_fail("eglInitialize(wayland)"));
        // EGLAttrib (intptr_t) in the generated alias's own spelling
        let mut device = 0;
        // SAFETY: an initialised display, EGL_DEVICE_EXT is the query EGL_EXT_device_query defines, `device` is a valid out-pointer.
        let ok = unsafe { (f.eglQueryDisplayAttribEXT)(raw, EGL_DEVICE_EXT, &mut device) };
        let render_node = if ok == EGL_FALSE || device == 0 {
            self.error();
            None
        } else {
            // SAFETY: `device` is the display's EGLDeviceEXT (an attribute holds the handle); the query returns null or a string EGL owns for the process.
            let p = unsafe {
                (f.eglQueryDeviceStringEXT)(device as EGLDevice, EGL_DRM_RENDER_NODE_FILE_EXT)
            };
            self.error();
            (!p.is_null()).then(|| cstr(p))
        };
        // SAFETY: binding the GLES API is valid on any thread.
        let ok = unsafe { (f.eglBindAPI)(EGL_OPENGL_ES_API) };
        ensure!(ok != EGL_FALSE, self.egl_fail("eglBindAPI(GLES)"));
        // SAFETY: an initialised display; each result is a string EGL owns.
        let q = |name| cstr(unsafe { (f.eglQueryString)(raw, name) });
        let display = Display {
            host: self.clone(),
            raw,
            version: (major, minor),
            vendor: q(EGL_VENDOR),
            extensions: q(EGL_EXTENSIONS),
        };
        let renderer = {
            let ctx = display.create_context()?;
            let gl = ctx.make_current()?;
            gl.string(GL_RENDERER)
        };
        Ok(WaylandNode {
            render_node,
            renderer,
            display,
        })
    }
}

/// The Wayland display's device, as [`Host::wayland_display_node`] found it, and the display itself (what the presenter imports with).
pub struct WaylandNode {
    pub render_node: Option<String>,
    pub renderer: String,
    pub display: Display,
}

/// One EGL device: a GPU (named by its DRM nodes) or Mesa's software rasteriser.
pub struct Device {
    host: Arc<Host>,
    raw: EGLDevice,
    pub render_node: Option<String>,
    pub drm_file: Option<String>,
    pub extensions: String,
}

impl Device {
    pub fn is_software(&self) -> bool {
        has_ext(&self.extensions, SOFTWARE_DEVICE_EXT)
    }

    /// The initialised display on this device, GLES bound.
    pub fn display(&self) -> Result<Display> {
        let f = &self.host.fns;
        let attribs = [EGL_NONE];
        // SAFETY: `raw` is a listed device, the platform EGL_EXT_platform_device takes it as the native display, the attribute list is NONE-terminated.
        let raw = unsafe {
            (f.eglGetPlatformDisplayEXT)(EGL_PLATFORM_DEVICE_EXT, self.raw, attribs.as_ptr())
        };
        ensure!(
            !raw.is_null(),
            self.host.egl_fail("eglGetPlatformDisplayEXT(device)")
        );
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `raw` is a display EGL returned; the version out-pointers are valid.
        let ok = unsafe { (f.eglInitialize)(raw, &mut major, &mut minor) };
        ensure!(ok != EGL_FALSE, self.host.egl_fail("eglInitialize"));
        // SAFETY: the API enum is a valid EGL client API.
        let ok = unsafe { (f.eglBindAPI)(EGL_OPENGL_ES_API) };
        ensure!(ok != EGL_FALSE, self.host.egl_fail("eglBindAPI(GLES)"));
        // SAFETY: `raw` is initialised; the result is a string EGL owns until terminate, which never happens.
        let q = |name| cstr(unsafe { (f.eglQueryString)(raw, name) });
        Ok(Display {
            host: self.host.clone(),
            raw,
            version: (major, minor),
            vendor: q(EGL_VENDOR),
            extensions: q(EGL_EXTENSIONS),
        })
    }
}

/// An initialised EGL display, GLES bound on the thread that made it.
pub struct Display {
    host: Arc<Host>,
    raw: EGLDisplay,
    pub version: (EGLint, EGLint),
    pub vendor: String,
    pub extensions: String,
}

/// One dma-buf plane as EGL_EXT_image_dma_buf_import names it.
pub struct DmabufPlane<'fd> {
    pub fd: BorrowedFd<'fd>,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

impl Display {
    pub fn has_extension(&self, name: &str) -> bool {
        has_ext(&self.extensions, name)
    }

    /// The host handle: the GLES driver hands it out as its own display, so the fronts' calls it passes through reach the host unchanged.
    pub fn raw(&self) -> EGLDisplay {
        self.raw
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    /// The modifiers the display imports `fourcc` with: (modifier, external_only).
    pub fn modifiers(&self, fourcc: u32) -> Result<Vec<(u64, bool)>> {
        let f = &self.host.fns;
        let fourcc = EGLint::from_ne_bytes(fourcc.to_ne_bytes());
        let mut n = 0;
        // SAFETY: null arrays with max 0 ask only for the count.
        let ok = unsafe {
            (f.eglQueryDmaBufModifiersEXT)(
                self.raw,
                fourcc,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut n,
            )
        };
        ensure!(
            ok != EGL_FALSE,
            self.host.egl_fail("eglQueryDmaBufModifiersEXT")
        );
        let len = usize::try_from(n)?;
        let (mut mods, mut ext) = (vec![0u64; len], vec![0 as EGLBoolean; len]);
        // SAFETY: both arrays hold `n` entries.
        let ok = unsafe {
            (f.eglQueryDmaBufModifiersEXT)(
                self.raw,
                fourcc,
                n,
                mods.as_mut_ptr(),
                ext.as_mut_ptr(),
                &mut n,
            )
        };
        ensure!(
            ok != EGL_FALSE,
            self.host.egl_fail("eglQueryDmaBufModifiersEXT")
        );
        Ok(mods
            .into_iter()
            .zip(ext.into_iter().map(|e| e != EGL_FALSE))
            .take(usize::try_from(n)?)
            .collect())
    }

    /// A GLES context: 3 if the driver gives it, else 2. Configless (`EGL_KHR_no_config_context`) where offered, since every target is an FBO.
    pub fn create_context(&self) -> Result<Context<'_>> {
        let f = &self.host.fns;
        let config = if self.has_extension("EGL_KHR_no_config_context") {
            std::ptr::null_mut()
        } else {
            let attribs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE];
            let (mut config, mut n) = (std::ptr::null_mut(), 0);
            // SAFETY: NONE-terminated attributes, one config slot.
            let ok =
                unsafe { (f.eglChooseConfig)(self.raw, attribs.as_ptr(), &mut config, 1, &mut n) };
            ensure!(
                ok != EGL_FALSE && n == 1,
                self.host.egl_fail("eglChooseConfig")
            );
            config
        };
        for major in [3, 2] {
            let attribs = [EGL_CONTEXT_MAJOR_VERSION, major, EGL_NONE];
            // SAFETY: `config` is a chosen config or EGL_NO_CONFIG_KHR (null) with the extension; NONE-terminated attributes; no share context.
            let raw = unsafe {
                (f.eglCreateContext)(self.raw, config, std::ptr::null_mut(), attribs.as_ptr())
            };
            if !raw.is_null() {
                return Ok(Context {
                    display: self,
                    raw,
                    major,
                });
            }
        }
        Err(self.host.egl_fail("eglCreateContext(GLES 3 / 2)"))
    }

    /// Imports one dma-buf plane as an EGLImage. EGL dups the fd; the caller keeps its own.
    pub fn import_dmabuf(&self, p: &DmabufPlane<'_>) -> Result<Image<'_>> {
        let i = |v: u32| EGLint::from_ne_bytes(v.to_ne_bytes());
        let m = |v: u64| {
            EGLint::from_ne_bytes(
                u32::try_from(v & 0xffff_ffff)
                    .expect("masked to 32 bits")
                    .to_ne_bytes(),
            )
        };
        let attribs = [
            EGL_WIDTH,
            i(p.width),
            EGL_HEIGHT,
            i(p.height),
            EGL_LINUX_DRM_FOURCC_EXT,
            i(p.fourcc),
            EGL_DMA_BUF_PLANE0_FD_EXT,
            p.fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            i(p.offset),
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            i(p.pitch),
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            m(p.modifier),
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            m(p.modifier >> 32),
            EGL_NONE,
        ];
        // SAFETY: EGL_LINUX_DMA_BUF_EXT takes no context and a null client buffer; the fd is open for the call (borrowed), the list is NONE-terminated.
        let raw = unsafe {
            (self.host.fns.eglCreateImageKHR)(
                self.raw,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        ensure!(
            !raw.is_null(),
            self.host.egl_fail("eglCreateImageKHR(dma-buf)")
        );
        Ok(Image { display: self, raw })
    }

    /// Flushes the current context and returns a sync_file that signals when its work so far is done (`EGL_ANDROID_native_fence_sync`).
    pub fn export_fence(&self) -> Result<OwnedFd> {
        let f = &self.host.fns;
        let attribs = [
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID,
            EGL_NO_NATIVE_FENCE_FD_ANDROID,
            EGL_NONE,
        ];
        // SAFETY: a native fence sync with no fd given makes a new fence for the current context's commands; NONE-terminated list.
        let sync = unsafe {
            (f.eglCreateSyncKHR)(self.raw, EGL_SYNC_NATIVE_FENCE_ANDROID, attribs.as_ptr())
        };
        ensure!(
            !sync.is_null(),
            self.host.egl_fail("eglCreateSyncKHR(native fence)")
        );
        // SAFETY: the fence gets its fd only once flushed (EGL_ANDROID_native_fence_sync); a context is current.
        unsafe { (f.glFlush)() };
        // SAFETY: `sync` is the native fence sync just made; the returned fd is a new one the caller owns.
        let fd = unsafe { (f.eglDupNativeFenceFDANDROID)(self.raw, sync) };
        let err = (fd < 0).then(|| self.host.egl_fail("eglDupNativeFenceFDANDROID"));
        // SAFETY: `sync` is ours and not used after this.
        unsafe { (f.eglDestroySyncKHR)(self.raw, sync) };
        if let Some(e) = err {
            return Err(e);
        }
        // SAFETY: `fd` is a fresh fd EGL dup'ed for us; nobody else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Makes the current context's later commands wait for `fence` on the GPU (eglWaitSyncKHR), then waits on the CPU too: the round trip of an acquire fence.
    pub fn wait_fence(&self, fence: OwnedFd) -> Result<()> {
        let f = &self.host.fns;
        let fd = fence.into_raw_fd();
        let attribs = [EGL_SYNC_NATIVE_FENCE_FD_ANDROID, fd, EGL_NONE];
        // SAFETY: on success EGL takes ownership of `fd` (the extension's contract); NONE-terminated list.
        let sync = unsafe {
            (f.eglCreateSyncKHR)(self.raw, EGL_SYNC_NATIVE_FENCE_ANDROID, attribs.as_ptr())
        };
        if sync.is_null() {
            let e = self.host.egl_fail("eglCreateSyncKHR(from fd)");
            // SAFETY: on failure the fd is still ours.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            return Err(e);
        }
        // SAFETY: `sync` is valid; flags 0 is the only value the extension allows.
        let gpu = unsafe { (f.eglWaitSyncKHR)(self.raw, sync, 0) };
        // SAFETY: `sync` is valid; flushing is what makes the wait finite.
        let cpu = unsafe {
            (f.eglClientWaitSyncKHR)(self.raw, sync, EGL_SYNC_FLUSH_COMMANDS_BIT, EGL_FOREVER)
        };
        let err = (gpu == 0).then(|| self.host.egl_fail("eglWaitSyncKHR"));
        // SAFETY: `sync` is ours, not used after this.
        unsafe { (f.eglDestroySyncKHR)(self.raw, sync) };
        if let Some(e) = err {
            return Err(e);
        }
        ensure!(
            cpu == EGL_CONDITION_SATISFIED,
            "eglClientWaitSyncKHR gave {cpu:#x}"
        );
        Ok(())
    }
}

/// An EGLImage on a display.
pub struct Image<'d> {
    display: &'d Display,
    raw: EGLImage,
}

impl Image<'_> {
    pub fn raw(&self) -> EGLImage {
        self.raw
    }

    /// Gives the image up to a caller that destroys it through the host (an `eglCreateImageKHR` the app owns).
    pub fn into_raw(self) -> EGLImage {
        let raw = self.raw;
        std::mem::forget(self);
        raw
    }
}

impl Drop for Image<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` is this display's image, destroyed once.
        unsafe { (self.display.host.fns.eglDestroyImageKHR)(self.display.raw, self.raw) };
    }
}

/// A GLES context, not current until [`Context::make_current`].
pub struct Context<'d> {
    display: &'d Display,
    raw: EGLContext,
    pub major: EGLint,
}

impl Context<'_> {
    /// Makes the context current on this thread with no surface (`EGL_KHR_surfaceless_context`); the GL calls go through the returned handle.
    pub fn make_current(&self) -> Result<Gl<'_>> {
        // SAFETY: display and context are valid; no surface is legal with EGL_KHR_surfaceless_context (both target hosts, run 2026-11-09).
        let ok = unsafe {
            (self.display.host.fns.eglMakeCurrent)(
                self.display.raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.raw,
            )
        };
        ensure!(
            ok != EGL_FALSE,
            self.display.host.egl_fail("eglMakeCurrent(surfaceless)")
        );
        Ok(Gl {
            fns: &self.display.host.fns,
            _thread: std::marker::PhantomData,
        })
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        let f = &self.display.host.fns;
        // SAFETY: releasing this thread's current context, then destroying ours once.
        unsafe {
            (f.eglMakeCurrent)(
                self.display.raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            (f.eglDestroyContext)(self.display.raw, self.raw);
        }
    }
}

/// GL calls on the context current on this thread. Not `Send` (the marker), so it stays on the thread that made it current.
pub struct Gl<'c> {
    fns: &'c Fns,
    _thread: std::marker::PhantomData<*const ()>,
}

/// A render target backed by an EGLImage: a texture, or a renderbuffer where the driver refuses the texture.
pub struct Texture<'g> {
    gl: &'g Gl<'g>,
    name: GLuint,
    pub renderbuffer: bool,
}

/// A framebuffer with one colour attachment.
pub struct Framebuffer<'g> {
    gl: &'g Gl<'g>,
    name: GLuint,
}

impl<'c> Gl<'c> {
    pub fn string(&self, name: GLenum) -> String {
        // SAFETY: a valid enum; the result is null or a static string.
        cstr(unsafe { (self.fns.glGetString)(name) }.cast())
    }

    pub fn check(&self, what: &str) -> Result<()> {
        // SAFETY: no arguments.
        let e = unsafe { (self.fns.glGetError)() };
        ensure!(e == GL_NO_ERROR, "{what}: GL error {e:#x}");
        Ok(())
    }

    /// A 2D texture whose storage is `image` (GL_OES_EGL_image), nearest filtering.
    pub fn texture_from_image<'g>(&'g self, image: &Image<'_>) -> Result<Texture<'g>> {
        let f = self.fns;
        let mut name = 0;
        // SAFETY: one name slot; then the new texture is bound and its storage set from a live image.
        unsafe {
            (f.glGenTextures)(1, &mut name);
            (f.glBindTexture)(GL_TEXTURE_2D, name);
            (f.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (f.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (f.glEGLImageTargetTexture2DOES)(GL_TEXTURE_2D, image.raw);
        }
        let t = Texture {
            gl: self,
            name,
            renderbuffer: false,
        };
        self.check("glEGLImageTargetTexture2DOES")?;
        Ok(t)
    }

    /// A renderbuffer whose storage is `image` (GL_OES_EGL_image): a render target where a driver will not make the image a 2D texture.
    pub fn renderbuffer_from_image<'g>(&'g self, image: &Image<'_>) -> Result<Texture<'g>> {
        let f = self.fns;
        let mut name = 0;
        // SAFETY: one name slot; then the new renderbuffer is bound and its storage set from a live image.
        unsafe {
            (f.glGenRenderbuffers)(1, &mut name);
            (f.glBindRenderbuffer)(GL_RENDERBUFFER, name);
            (f.glEGLImageTargetRenderbufferStorageOES)(GL_RENDERBUFFER, image.raw);
        }
        let t = Texture {
            gl: self,
            name,
            renderbuffer: true,
        };
        self.check("glEGLImageTargetRenderbufferStorageOES")?;
        Ok(t)
    }

    /// A framebuffer rendering into `target`, bound; fails unless complete.
    pub fn framebuffer<'g>(&'g self, target: &Texture<'_>) -> Result<Framebuffer<'g>> {
        let f = self.fns;
        let mut name = 0;
        // SAFETY: one name slot; the bound framebuffer gets a live texture / renderbuffer as attachment.
        let status = unsafe {
            (f.glGenFramebuffers)(1, &mut name);
            (f.glBindFramebuffer)(GL_FRAMEBUFFER, name);
            if target.renderbuffer {
                (f.glFramebufferRenderbuffer)(
                    GL_FRAMEBUFFER,
                    GL_COLOR_ATTACHMENT0,
                    GL_RENDERBUFFER,
                    target.name,
                );
            } else {
                (f.glFramebufferTexture2D)(
                    GL_FRAMEBUFFER,
                    GL_COLOR_ATTACHMENT0,
                    GL_TEXTURE_2D,
                    target.name,
                    0,
                );
            }
            (f.glCheckFramebufferStatus)(GL_FRAMEBUFFER)
        };
        let fb = Framebuffer { gl: self, name };
        if status != GL_FRAMEBUFFER_COMPLETE {
            bail!("framebuffer incomplete: {status:#x}");
        }
        Ok(fb)
    }

    pub fn bind(&self, fb: &Framebuffer<'_>) {
        // SAFETY: a live framebuffer name.
        unsafe { (self.fns.glBindFramebuffer)(GL_FRAMEBUFFER, fb.name) };
    }

    pub fn viewport(&self, w: i32, h: i32) {
        // SAFETY: plain state call.
        unsafe { (self.fns.glViewport)(0, 0, w, h) };
    }

    /// Fills a rectangle of the bound framebuffer (GL's bottom-left origin) with `rgba` through a scissored clear.
    pub fn fill(&self, x: i32, y: i32, w: i32, h: i32, rgba: [f32; 4]) {
        let f = self.fns;
        // SAFETY: plain state calls and a clear of the bound framebuffer.
        unsafe {
            (f.glEnable)(GL_SCISSOR_TEST);
            (f.glScissor)(x, y, w, h);
            (f.glClearColor)(rgba[0], rgba[1], rgba[2], rgba[3]);
            (f.glClear)(GL_COLOR_BUFFER_BIT);
            (f.glDisable)(GL_SCISSOR_TEST);
        }
    }

    /// The bound framebuffer's pixels as RGBA8, rows bottom-up (GL's order): for buffers whose layout the CPU cannot index (tiled modifiers).
    pub fn read_rgba(&self, w: i32, h: i32) -> Result<Vec<u8>> {
        let mut px = vec![0u8; usize::try_from(w)? * usize::try_from(h)? * 4];
        // SAFETY: `px` holds w x h RGBA8 pixels at the default pack alignment 4 (rows of 4w bytes need no padding).
        unsafe {
            (self.fns.glReadPixels)(
                0,
                0,
                w,
                h,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                px.as_mut_ptr().cast(),
            )
        };
        self.check("glReadPixels")?;
        Ok(px)
    }

    pub fn finish(&self) {
        // SAFETY: no arguments.
        unsafe { (self.fns.glFinish)() };
    }
}

impl Drop for Texture<'_> {
    fn drop(&mut self) {
        // SAFETY: our name, deleted once, on the context that made it.
        unsafe {
            if self.renderbuffer {
                (self.gl.fns.glDeleteRenderbuffers)(1, &self.name);
            } else {
                (self.gl.fns.glDeleteTextures)(1, &self.name);
            }
        }
    }
}

impl Drop for Framebuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: our name, deleted once, on the context that made it.
        unsafe { (self.gl.fns.glDeleteFramebuffers)(1, &self.name) };
    }
}

fn has_ext(list: &str, name: &str) -> bool {
    list.split_ascii_whitespace().any(|e| e == name)
}

fn cstr(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null strings EGL / GL return are NUL-terminated and live at least for this copy.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
