// SPDX-License-Identifier: MIT
//! A window surface: the behaviour of Mesa's platform_android (dequeue on first use, render, queue on swap), over host EGL 1.5 + `EGL_EXT_image_dma_buf_import` + `GL_OES_EGL_image` instead of Mesa's internal `__DRIimage`.
//! Each dequeued buffer is imported once (cached by its gralloc id, unique among live buffers) and drawn through an FBO per context (FBOs are not shared between contexts), with one depth/stencil renderbuffer per context when the config asks for one.
//! Swap: native fence (or glFinish where the host has none) -> queueBuffer, then the next buffer is dequeued and bound at once, so framebuffer 0 stays drawable after the swap as EGL requires.
//! Orientation (driver.rs `Orientation`): the buffer is stored top-down, as its consumer reads it; the driver sets no buffer transform and the app's own (`ANativeWindow_setBuffersTransform`) passes through.
//! - `FlipY` tier: framebuffer 0 is the buffer's FBO, rendering top-down (`GL_MESA_framebuffer_flip_y`, as Mesa's platform_android does for its window surfaces).
//! - `Blit` tier: framebuffer 0 is a per-context back FBO (colour + depth/stencil, rendering bottom-up as GL does); swap blits it into the buffer's colour-only FBO with y inverted. Cost: one full-surface copy per frame + one colour renderbuffer per context.
//!
//! Damage rects the fronts convert assuming top-down buffers are then right as they are.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};

use atl_egl_host::{DmabufPlane, Image};
use atl_gralloc::handle::{FD_DMABUF, Params};
use atl_gralloc::usage;
use rustix::event::{PollFd, PollFlags, poll};

use crate::abi::egl as e;
use crate::abi::gles as g;
use crate::abi::window as w;
use crate::driver::{Driver, Orientation};
use crate::window::{Buffer, Window};

/// A host image the surface owns.
struct OwnedImage(Image<'static>);

// SAFETY: an EGLImage is a display-level object, valid on any thread (EGL_KHR_image_base); the display it borrows is the process-lifetime driver display.
unsafe impl Send for OwnedImage {}

/// The colour target of one buffer in one context.
struct Target {
    fbo: u32,
    texture: u32,
}

/// `Blit` tier: a context's framebuffer 0 at one size.
struct Back {
    fbo: u32,
    color: u32,
    width: i32,
    height: i32,
}

pub struct WindowSurface {
    window: Window,
    /// This surface connected the window to EGL, so its teardown is ours (Drop); false when a front connected it first.
    connected_here: bool,
    pub config: e::EGLConfig,
    depth_stencil: bool,
    pub width: i32,
    pub height: i32,
    images: HashMap<u64, OwnedImage>,
    targets: HashMap<(usize, u64), Target>,
    depth: HashMap<usize, (u32, i32, i32)>,
    backs: HashMap<usize, Back>,
    current: Option<(Buffer, u64)>,
}

// SAFETY: EGLConfig is a display-level handle; the window and buffers are Send; the GL names are plain integers used only with their context current.
unsafe impl Send for WindowSurface {}

/// An EGL error code.
pub type EglError = i32;

impl WindowSurface {
    pub fn new(d: &Driver, window: Window, config: e::EGLConfig) -> Result<Self, EglError> {
        let bits = |a| d.host_attrib(config, a).unwrap_or(0);
        let depth_stencil = bits(e::EGL_DEPTH_SIZE) > 0 || bits(e::EGL_STENCIL_SIZE) > 0;
        let connected_here = connect(&window)?;
        // from here on a failure drops `s`, which gives back a connection this surface made (Drop)
        let mut s = Self {
            window,
            connected_here,
            config,
            depth_stencil,
            width: 0,
            height: 0,
            images: HashMap::new(),
            targets: HashMap::new(),
            depth: HashMap::new(),
            backs: HashMap::new(),
            current: None,
        };
        if connected_here {
            // the front sets the buffers format to the config's on create (egl_platform_entries.cpp); the producer's own format may differ (Happy Jump's SurfaceView asked RGB_565, which atl-gralloc has no row for)
            let format = d.window_format(config).ok_or(e::EGL_BAD_MATCH)?;
            s.window
                .set_buffers_format(format)
                .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
            log(&format!("window format {format} (the config's)"));
        }
        // Mesa's platform_android asks for GRALLOC_USAGE_HW_RENDER the same way (window.h native_window_set_usage)
        s.window
            .perform_u64(w::NATIVE_WINDOW_SET_USAGE64, usage::GPU_RENDER_TARGET)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        s.width = s
            .window
            .query(w::NATIVE_WINDOW_WIDTH)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        s.height = s
            .window
            .query(w::NATIVE_WINDOW_HEIGHT)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        Ok(s)
    }

    pub fn set_swap_interval(&self, interval: i32) {
        // a window that refuses an interval keeps its own: EGL clamps silently too
        let _ = self.window.set_swap_interval(interval);
    }

    /// Dequeues a buffer if none is held, waits for its fence, imports it once.
    fn ensure_buffer(&mut self, d: &'static Driver) -> Result<u64, EglError> {
        if let Some((_, id)) = &self.current {
            return Ok(*id);
        }
        let (buf, fence) = self
            .window
            .dequeue()
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        if let Some(f) = fence {
            wait(&f)?;
        }
        let h = buf.handle().ok_or(e::EGL_BAD_NATIVE_WINDOW)?;
        let p = Params::from_handle(h.fds.len(), h.ints).map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        if let std::collections::hash_map::Entry::Vacant(slot) = self.images.entry(p.id) {
            slot.insert(OwnedImage(import(d, &p, h.fds[FD_DMABUF])?));
        }
        (self.width, self.height) = (buf.width(), buf.height());
        self.current = Some((buf, p.id));
        Ok(p.id)
    }

    /// Makes the held buffer (dequeued now if needed) the draw target of the current context `ctx`; returns the FBO now standing for framebuffer 0.
    pub fn bind(&mut self, d: &'static Driver, ctx: usize) -> Result<u32, EglError> {
        let id = self.ensure_buffer(d)?;
        let fbo = match d.orientation {
            Orientation::FlipY(_) => self.buffer_fbo(d, ctx, id)?,
            Orientation::Blit => self.back(d, ctx)?,
        };
        // SAFETY: `fbo` is a framebuffer of the context current on this thread.
        unsafe { (d.gl.glBindFramebuffer)(g::GL_FRAMEBUFFER as u32, fbo) };
        Ok(fbo)
    }

    /// The FBO on buffer `id` in `ctx`, made once: in the `FlipY` tier framebuffer 0 itself (flip-y + depth/stencil), in `Blit` the copy's colour-only destination.
    fn buffer_fbo(&mut self, d: &'static Driver, ctx: usize, id: u64) -> Result<u32, EglError> {
        if let Some(t) = self.targets.get(&(ctx, id)) {
            return Ok(t.fbo);
        }
        let flip = match d.orientation {
            Orientation::FlipY(f) => Some(f),
            Orientation::Blit => None,
        };
        let depth = if flip.is_some() {
            self.depth_rb(d, ctx)
        } else {
            0
        };
        let image = self.images[&id].0.raw();
        let t = target(d, image, depth, flip)?;
        let fbo = t.fbo;
        self.targets.insert((ctx, id), t);
        Ok(fbo)
    }

    /// `Blit` tier: the context's back FBO at the current size (remade on a resize), with the depth/stencil renderbuffer when the config has one.
    fn back(&mut self, d: &'static Driver, ctx: usize) -> Result<u32, EglError> {
        let (w, h) = (self.width, self.height);
        if let Some(b) = self.backs.get(&ctx) {
            if (b.width, b.height) == (w, h) {
                return Ok(b.fbo);
            }
            let b = self.backs.remove(&ctx).expect("present");
            delete_back(d, &b);
        }
        let depth = self.depth_rb(d, ctx);
        let gl = &d.gl;
        let (mut fbo, mut color, mut prev) = (0, 0, 0);
        // SAFETY: the current context; name slots; the renderbuffer binding restored (the app's state), the FBO left bound (the caller binds next).
        let status = unsafe {
            (gl.glGetIntegerv)(g::GL_RENDERBUFFER_BINDING as u32, &mut prev);
            (gl.glGenRenderbuffers)(1, &mut color);
            (gl.glBindRenderbuffer)(g::GL_RENDERBUFFER as u32, color);
            // both window formats (RGBA_8888, RGBX_8888) are 8-bit normalized: a blit between them converts nothing but the ignored X byte
            (gl.glRenderbufferStorage)(g::GL_RENDERBUFFER as u32, g::GL_RGBA8 as u32, w, h);
            (gl.glBindRenderbuffer)(g::GL_RENDERBUFFER as u32, prev as u32);
            (gl.glGenFramebuffers)(1, &mut fbo);
            (gl.glBindFramebuffer)(g::GL_FRAMEBUFFER as u32, fbo);
            (gl.glFramebufferRenderbuffer)(
                g::GL_FRAMEBUFFER as u32,
                g::GL_COLOR_ATTACHMENT0 as u32,
                g::GL_RENDERBUFFER as u32,
                color,
            );
            attach_depth(d, depth);
            (gl.glCheckFramebufferStatus)(g::GL_FRAMEBUFFER as u32)
        };
        let b = Back {
            fbo,
            color,
            width: w,
            height: h,
        };
        if status != g::GL_FRAMEBUFFER_COMPLETE as u32 {
            log(&format!(
                "back: framebuffer incomplete ({status:#x}), EGL_BAD_ALLOC"
            ));
            delete_back(d, &b);
            return Err(e::EGL_BAD_ALLOC);
        }
        self.backs.insert(ctx, b);
        Ok(fbo)
    }

    /// `Blit` tier, at swap: the back FBO into the held buffer, y inverted, so the buffer's row 0 is GL's top row. Leaves both framebuffer bindings to the caller's rebind.
    fn flip_into_buffer(&mut self, d: &'static Driver, ctx: usize) -> Result<(), EglError> {
        let id = self.ensure_buffer(d)?;
        let src = self.back(d, ctx)?;
        let dst = self.buffer_fbo(d, ctx, id)?;
        let (w, h) = (self.width, self.height);
        // the `no_blit_flip` break (build.rs `BREAKS`): the copy keeps GL's bottom-up rows
        let (y0, y1) = if cfg!(atl_egl_no_blit_flip) {
            (0, h)
        } else {
            (h, 0)
        };
        let gl = &d.gl;
        // SAFETY: the current context; both FBOs are its own and complete; the app's scissor test (the one fragment state a blit obeys) is restored.
        unsafe {
            let scissor = (gl.glIsEnabled)(g::GL_SCISSOR_TEST as u32) != 0;
            if scissor {
                (gl.glDisable)(g::GL_SCISSOR_TEST as u32);
            }
            (gl.glBindFramebuffer)(g::GL_READ_FRAMEBUFFER as u32, src);
            (gl.glBindFramebuffer)(g::GL_DRAW_FRAMEBUFFER as u32, dst);
            (gl.glBlitFramebuffer)(
                0,
                0,
                w,
                h,
                0,
                y0,
                w,
                y1,
                g::GL_COLOR_BUFFER_BIT as u32,
                g::GL_NEAREST as u32,
            );
            if scissor {
                (gl.glEnable)(g::GL_SCISSOR_TEST as u32);
            }
        }
        Ok(())
    }

    /// The context's depth/stencil renderbuffer at the current size; 0 when the config has neither.
    fn depth_rb(&mut self, d: &'static Driver, ctx: usize) -> u32 {
        if !self.depth_stencil {
            return 0;
        }
        let (w, h) = (self.width, self.height);
        if let Some(&(rb, rw, rh)) = self.depth.get(&ctx) {
            if (rw, rh) == (w, h) {
                return rb;
            }
            // a resize: the old size's targets in this context go with it
            // SAFETY: names of the current context.
            unsafe { (d.gl.glDeleteRenderbuffers)(1, &rb) };
            let stale: Vec<_> = self
                .targets
                .keys()
                .filter(|k| k.0 == ctx)
                .copied()
                .collect();
            for k in stale {
                let t = self.targets.remove(&k).expect("listed");
                delete_target(d, &t);
            }
        }
        let mut rb = 0;
        let mut prev = 0;
        // SAFETY: the current context; one name slot; storage for the new renderbuffer; the previous binding restored (the app's state).
        unsafe {
            (d.gl.glGetIntegerv)(g::GL_RENDERBUFFER_BINDING as u32, &mut prev);
            (d.gl.glGenRenderbuffers)(1, &mut rb);
            (d.gl.glBindRenderbuffer)(g::GL_RENDERBUFFER as u32, rb);
            (d.gl.glRenderbufferStorage)(
                g::GL_RENDERBUFFER as u32,
                g::GL_DEPTH24_STENCIL8 as u32,
                w,
                h,
            );
            (d.gl.glBindRenderbuffer)(g::GL_RENDERBUFFER as u32, prev as u32);
        }
        self.depth.insert(ctx, (rb, w, h));
        rb
    }

    /// eglSwapBuffers on this surface, current in `ctx`.
    pub fn swap(&mut self, d: &'static Driver, ctx: usize) -> Result<u32, EglError> {
        if self.current.is_none() {
            // a swap with nothing drawn still presents a buffer (EGL: contents undefined)
            self.ensure_buffer(d)?;
        }
        if let Orientation::Blit = d.orientation {
            self.flip_into_buffer(d, ctx)?;
        }
        let fence = if d.native_fence {
            Some(d.display.export_fence().map_err(|err| {
                log(&format!(
                    "swap: native fence export failed ({err}), EGL_BAD_ALLOC"
                ));
                e::EGL_BAD_ALLOC
            })?)
        } else {
            // SAFETY: a context is current on this thread.
            unsafe { (d.gl.glFinish)() };
            None
        };
        let (buf, _) = self.current.take().expect("ensured above");
        self.window
            .queue(buf, fence)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        self.bind(d, ctx)
    }

    /// Gives everything back: the held buffer to the window, the GL names of `current_ctx` (the context current on this thread, if any; other contexts' names go with their context), the images.
    pub fn destroy(mut self, d: &'static Driver, current_ctx: Option<usize>) {
        if let Some((buf, _)) = self.current.take() {
            self.window.cancel(buf);
        }
        if let Some(ctx) = current_ctx {
            for ((c, _), t) in self.targets.drain() {
                if c == ctx {
                    delete_target(d, &t);
                }
            }
            if let Some(b) = self.backs.remove(&ctx) {
                delete_back(d, &b);
            }
            if let Some((rb, _, _)) = self.depth.remove(&ctx) {
                // SAFETY: a name of the current context.
                unsafe { (d.gl.glDeleteRenderbuffers)(1, &rb) };
            }
        }
    }
}

/// Connects the window to the EGL API unless a front already did. The lowest layer that dequeues owns the connect (as libvulkan's swapchain does under Vulkan; the front skips it under ANGLE, egl_platform_entries.cpp).
/// `Ok(true)`: this surface connected it, so its teardown is ours. `Ok(false)`: a front did. A failed connect fails the create with EGL_BAD_ALLOC, the front's error for it.
#[cfg(not(atl_egl_no_window_connect))]
fn connect(window: &Window) -> Result<bool, EglError> {
    match window.connect_egl() {
        Ok(true) => {
            log("window connect: driver owns");
            Ok(true)
        }
        Ok(false) => {
            log("window connect: front owns (already connected)");
            Ok(false)
        }
        Err(r) => {
            log(&format!("window connect: failed ({r}), EGL_BAD_ALLOC"));
            Err(e::EGL_BAD_ALLOC)
        }
    }
}

/// The `no_window_connect` break (build.rs `BREAKS`): nothing connects the window, so its dequeue has no connected producer, as before this connect existed.
#[cfg(atl_egl_no_window_connect)]
fn connect(_: &Window) -> Result<bool, EglError> {
    log("window connect: skipped (break no_window_connect)");
    Ok(false)
}

/// One stderr line per window event (the app's log), so a run shows which path ran.
#[allow(
    clippy::print_stderr,
    reason = "one line per window connect or teardown, which path ran"
)]
fn log(line: &str) {
    eprintln!("libGLES_atl: {line}");
}

impl Drop for WindowSurface {
    /// Runs after `destroy` cancelled the held buffer, and also when a create fails after the connect: format 0, then disconnect (egl_object.cpp's order).
    fn drop(&mut self) {
        if self.connected_here && self.window.disconnect_egl().is_err() {
            log("window disconnect failed");
        }
    }
}

/// Waits on the CPU for a sync_file: a dequeue's release fence.
fn wait(fence: &OwnedFd) -> Result<(), EglError> {
    let mut fds = [PollFd::new(fence, PollFlags::IN)];
    loop {
        match poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Err(e::EGL_BAD_NATIVE_WINDOW),
        }
    }
}

/// Imports one gralloc buffer (the handle's dma-buf, single plane) as a host EGLImage.
pub fn import(d: &'static Driver, p: &Params, fd: i32) -> Result<Image<'static>, EglError> {
    // SAFETY: `fd` is the handle's dma-buf, open while the buffer lives, which outlasts this call (the caller holds the buffer).
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    d.display
        .import_dmabuf(&DmabufPlane {
            fd: fd.as_fd(),
            fourcc: p.fourcc,
            width: p.width,
            height: p.height,
            offset: 0,
            pitch: p.pitch,
            modifier: p.modifier,
        })
        .map_err(|err| {
            log(&format!(
                "import: dma-buf {}x{} fourcc {:#x} pitch {} modifier {:#x} refused ({err}), EGL_BAD_ALLOC",
                p.width, p.height, p.fourcc, p.pitch, p.modifier
            ));
            e::EGL_BAD_ALLOC
        })
}

/// `depth` as depth and stencil of the bound FBO, when non-zero.
/// SAFETY: a context is current with the FBO bound; `depth` is 0 or its renderbuffer.
unsafe fn attach_depth(d: &'static Driver, depth: u32) {
    if depth == 0 {
        return;
    }
    for att in [g::GL_DEPTH_ATTACHMENT, g::GL_STENCIL_ATTACHMENT] {
        // SAFETY: the caller's.
        unsafe {
            (d.gl.glFramebufferRenderbuffer)(
                g::GL_FRAMEBUFFER as u32,
                att as u32,
                g::GL_RENDERBUFFER as u32,
                depth,
            );
        }
    }
}

/// A texture on `image` and an FBO with it as colour (plus `depth` as depth and stencil when non-zero, top-down through `flip` when given), in the current context; the app's texture binding is restored.
fn target(
    d: &'static Driver,
    image: e::EGLImageKHR,
    depth: u32,
    flip: Option<g::glFramebufferParameteriMESA>,
) -> Result<Target, EglError> {
    let gl = &d.gl;
    let (mut texture, mut fbo, mut prev_tex) = (0, 0, 0);
    // SAFETY: the current context; name slots; the image is live (the surface owns it); bindings restored except the FBO, which the caller binds next.
    let status = unsafe {
        (gl.glGetIntegerv)(g::GL_TEXTURE_BINDING_2D as u32, &mut prev_tex);
        (gl.glGenTextures)(1, &mut texture);
        (gl.glBindTexture)(g::GL_TEXTURE_2D as u32, texture);
        (gl.glTexParameteri)(
            g::GL_TEXTURE_2D as u32,
            g::GL_TEXTURE_MIN_FILTER as u32,
            g::GL_NEAREST,
        );
        (gl.glTexParameteri)(
            g::GL_TEXTURE_2D as u32,
            g::GL_TEXTURE_MAG_FILTER as u32,
            g::GL_NEAREST,
        );
        (gl.glEGLImageTargetTexture2DOES)(g::GL_TEXTURE_2D as u32, image);
        (gl.glBindTexture)(g::GL_TEXTURE_2D as u32, prev_tex as u32);
        (gl.glGenFramebuffers)(1, &mut fbo);
        (gl.glBindFramebuffer)(g::GL_FRAMEBUFFER as u32, fbo);
        (gl.glFramebufferTexture2D)(
            g::GL_FRAMEBUFFER as u32,
            g::GL_COLOR_ATTACHMENT0 as u32,
            g::GL_TEXTURE_2D as u32,
            texture,
            0,
        );
        if let Some(set) = flip {
            // FlipY tier: row 0 of the buffer is the window's top row
            set(
                g::GL_FRAMEBUFFER as u32,
                g::GL_FRAMEBUFFER_FLIP_Y_MESA as u32,
                1,
            );
        }
        attach_depth(d, depth);
        (gl.glCheckFramebufferStatus)(g::GL_FRAMEBUFFER as u32)
    };
    let t = Target { fbo, texture };
    if status != g::GL_FRAMEBUFFER_COMPLETE as u32 {
        log(&format!(
            "target: buffer framebuffer incomplete ({status:#x}), EGL_BAD_ALLOC"
        ));
        delete_target(d, &t);
        return Err(e::EGL_BAD_ALLOC);
    }
    Ok(t)
}

fn delete_back(d: &'static Driver, b: &Back) {
    // SAFETY: names of the current context, deleted once.
    unsafe {
        (d.gl.glDeleteFramebuffers)(1, &b.fbo);
        (d.gl.glDeleteRenderbuffers)(1, &b.color);
    }
}

fn delete_target(d: &'static Driver, t: &Target) {
    // SAFETY: names of the current context, deleted once.
    unsafe {
        (d.gl.glDeleteFramebuffers)(1, &t.fbo);
        (d.gl.glDeleteTextures)(1, &t.texture);
    }
}
