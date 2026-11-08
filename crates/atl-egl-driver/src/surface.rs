// SPDX-License-Identifier: MIT
//! A window surface: the behaviour of Mesa's platform_android (dequeue on first use, render, queue on swap), over host EGL 1.5 + `EGL_EXT_image_dma_buf_import` + `GL_OES_EGL_image` instead of Mesa's internal `__DRIimage`.
//! Each dequeued buffer is imported once (cached by its gralloc id, unique among live buffers) and drawn through an FBO per context (FBOs are not shared between contexts), with one depth/stencil renderbuffer per context when the config asks for one.
//! Swap: native fence (or glFinish where the host has none) -> queueBuffer, then the next buffer is dequeued and bound at once, so framebuffer 0 stays drawable after the swap as EGL requires.
//! Orientation: each FBO renders top-down (`GL_MESA_framebuffer_flip_y`, as Mesa's platform_android does for its window surfaces), so the buffer is stored as its consumer reads it; the driver sets no buffer transform and the app's own (`ANativeWindow_setBuffersTransform`) passes through.
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
use crate::driver::Driver;
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

pub struct WindowSurface {
    window: Window,
    pub config: e::EGLConfig,
    depth_stencil: bool,
    pub width: i32,
    pub height: i32,
    images: HashMap<u64, OwnedImage>,
    targets: HashMap<(usize, u64), Target>,
    depth: HashMap<usize, (u32, i32, i32)>,
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
        // Mesa's platform_android asks for GRALLOC_USAGE_HW_RENDER the same way (window.h native_window_set_usage)
        window
            .perform_u64(w::NATIVE_WINDOW_SET_USAGE64, usage::GPU_RENDER_TARGET)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        let width = window
            .query(w::NATIVE_WINDOW_WIDTH)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        let height = window
            .query(w::NATIVE_WINDOW_HEIGHT)
            .map_err(|_| e::EGL_BAD_NATIVE_WINDOW)?;
        Ok(Self {
            window,
            config,
            depth_stencil,
            width,
            height,
            images: HashMap::new(),
            targets: HashMap::new(),
            depth: HashMap::new(),
            current: None,
        })
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
        let fbo = match self.targets.get(&(ctx, id)) {
            Some(t) => t.fbo,
            None => {
                let depth = self.depth_rb(d, ctx);
                let image = self.images[&id].0.raw();
                let t = target(d, image, depth)?;
                let fbo = t.fbo;
                self.targets.insert((ctx, id), t);
                fbo
            }
        };
        // SAFETY: `fbo` is a framebuffer of the context current on this thread.
        unsafe { (d.gl.glBindFramebuffer)(g::GL_FRAMEBUFFER as u32, fbo) };
        Ok(fbo)
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
        let fence = if d.native_fence {
            Some(d.display.export_fence().map_err(|_| e::EGL_BAD_ALLOC)?)
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
            if let Some((rb, _, _)) = self.depth.remove(&ctx) {
                // SAFETY: a name of the current context.
                unsafe { (d.gl.glDeleteRenderbuffers)(1, &rb) };
            }
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
        .map_err(|_| e::EGL_BAD_ALLOC)
}

/// A texture on `image` and an FBO with it as colour (plus `depth` as depth and stencil when non-zero), in the current context; the app's texture binding is restored.
fn target(d: &'static Driver, image: e::EGLImageKHR, depth: u32) -> Result<Target, EglError> {
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
        // top-down rendering (driver.rs FLIP_Y, checked at load): row 0 of the buffer is the window's top row
        (gl.glFramebufferParameteriMESA)(
            g::GL_FRAMEBUFFER as u32,
            g::GL_FRAMEBUFFER_FLIP_Y_MESA as u32,
            1,
        );
        if depth != 0 {
            for att in [g::GL_DEPTH_ATTACHMENT, g::GL_STENCIL_ATTACHMENT] {
                (gl.glFramebufferRenderbuffer)(
                    g::GL_FRAMEBUFFER as u32,
                    att as u32,
                    g::GL_RENDERBUFFER as u32,
                    depth,
                );
            }
        }
        (gl.glCheckFramebufferStatus)(g::GL_FRAMEBUFFER as u32)
    };
    let t = Target { fbo, texture };
    if status != g::GL_FRAMEBUFFER_COMPLETE as u32 {
        delete_target(d, &t);
        return Err(e::EGL_BAD_ALLOC);
    }
    Ok(t)
}

fn delete_target(d: &'static Driver, t: &Target) {
    // SAFETY: names of the current context, deleted once.
    unsafe {
        (d.gl.glDeleteFramebuffers)(1, &t.fbo);
        (d.gl.glDeleteTextures)(1, &t.texture);
    }
}
