// SPDX-License-Identifier: MIT
//! Framebuffer 0 = the window surface's FBO. hwui and GL apps draw to the window by binding framebuffer 0 and read the binding back to tell "the window" from their own FBOs (hwui SkiaOpenGLPipeline: `fFBOID = 0`, GL_FRAMEBUFFER_BINDING);
//! on the host that name is the surfaceless context's empty default framebuffer. So eglGetProcAddress answers these entries with wrappers that translate between 0 and the FBO while a window surface is current on the thread, and the host's own entries otherwise:
//! bind 0, binding queries, invalidate / discard naming GL_COLOR / GL_DEPTH / GL_STENCIL, GL_BACK in draw / read buffers, delete / isFramebuffer of the FBO's name. Every other GL entry is the host's directly.
//! Not wrapped (no known caller): glGetIntegeri_v / glGetInteger64v / glGetBooleanv / glGetFloatv of a binding, glGetFramebufferAttachmentParameteriv on framebuffer 0, GLES 1's OES framebuffer entries.

#![allow(
    non_snake_case,
    reason = "each wrapper carries the GL entry's name it stands for"
)]

use std::cell::Cell;

use crate::abi::gles as g;
use crate::driver;

thread_local! {
    /// The FBO standing for framebuffer 0 on this thread (0: no window surface current).
    static FBO: Cell<u32> = const { Cell::new(0) };
    /// (surface handle, context) current on this thread when the draw surface is a window surface.
    static CURRENT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

pub fn set_current(current: Option<(usize, usize)>, fbo: u32) {
    CURRENT.with(|c| c.set(current));
    FBO.with(|f| f.set(fbo));
}

pub fn current() -> Option<(usize, usize)> {
    CURRENT.with(Cell::get)
}

/// The surface FBO now standing for framebuffer 0 (after a swap bound the next buffer).
pub fn set_fbo(fbo: u32) {
    FBO.with(|f| f.set(fbo));
}

fn fbo() -> u32 {
    FBO.with(Cell::get)
}

/// The host's binding for `target`'s read or draw side.
fn bound(target: u32) -> u32 {
    let Some(d) = driver::get() else { return 0 };
    let pname = if target == g::GL_READ_FRAMEBUFFER as u32 {
        g::GL_READ_FRAMEBUFFER_BINDING
    } else {
        g::GL_FRAMEBUFFER_BINDING
    };
    let mut v = 0;
    // SAFETY: a binding query into a valid out-pointer on the current context.
    unsafe { (d.gl.glGetIntegerv)(pname as u32, &mut v) };
    v as u32
}

/// Default-framebuffer attachment names (GL_COLOR, GL_DEPTH, GL_STENCIL; GL_COLOR_EXT etc. share the values) as an FBO's; others unchanged.
fn attachment(a: u32) -> u32 {
    match a as i32 {
        g::GL_COLOR => g::GL_COLOR_ATTACHMENT0 as u32,
        g::GL_DEPTH => g::GL_DEPTH_ATTACHMENT as u32,
        g::GL_STENCIL => g::GL_STENCIL_ATTACHMENT as u32,
        _ => a,
    }
}

/// `atts[..n]` mapped when `target`'s binding is the surface FBO; `None`: pass the caller's array unchanged.
///
/// # Safety
/// `atts` points to `n` enums (the GL entry's contract).
unsafe fn mapped(target: u32, n: i32, atts: *const u32) -> Option<Vec<u32>> {
    let f = fbo();
    if f == 0 || bound(target) != f || atts.is_null() {
        return None;
    }
    let n = usize::try_from(n).ok()?;
    // SAFETY: per the caller.
    let src = unsafe { std::slice::from_raw_parts(atts, n) };
    Some(src.iter().map(|&a| attachment(a)).collect())
}

pub unsafe extern "C" fn glBindFramebuffer(target: u32, framebuffer: u32) {
    let Some(d) = driver::get() else { return };
    let f = fbo();
    let name = if framebuffer == 0 && f != 0 {
        f
    } else {
        framebuffer
    };
    // SAFETY: the host entry with the caller's arguments, name translated.
    unsafe { (d.gl.glBindFramebuffer)(target, name) };
}

pub unsafe extern "C" fn glGetIntegerv(pname: u32, data: *mut i32) {
    let Some(d) = driver::get() else { return };
    // SAFETY: the host entry with the caller's arguments.
    unsafe { (d.gl.glGetIntegerv)(pname, data) };
    let f = fbo();
    let binding =
        pname == g::GL_FRAMEBUFFER_BINDING as u32 || pname == g::GL_READ_FRAMEBUFFER_BINDING as u32;
    // SAFETY: a binding query wrote one GLint to the caller's pointer.
    if binding && f != 0 && !data.is_null() && unsafe { *data } == f as i32 {
        // SAFETY: as above.
        unsafe { *data = 0 };
    }
}

pub unsafe extern "C" fn glInvalidateFramebuffer(target: u32, n: i32, atts: *const u32) {
    let Some(d) = driver::get() else { return };
    // SAFETY: the caller passes `n` enums.
    match unsafe { mapped(target, n, atts) } {
        // SAFETY: the host entry; `v` holds `n` enums.
        Some(v) => unsafe { (d.gl.glInvalidateFramebuffer)(target, n, v.as_ptr()) },
        // SAFETY: the host entry with the caller's arguments.
        None => unsafe { (d.gl.glInvalidateFramebuffer)(target, n, atts) },
    }
}

pub unsafe extern "C" fn glInvalidateSubFramebuffer(
    target: u32,
    n: i32,
    atts: *const u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let Some(d) = driver::get() else { return };
    // SAFETY: the caller passes `n` enums.
    match unsafe { mapped(target, n, atts) } {
        // SAFETY: the host entry; `v` holds `n` enums.
        Some(v) => unsafe { (d.gl.glInvalidateSubFramebuffer)(target, n, v.as_ptr(), x, y, w, h) },
        // SAFETY: the host entry with the caller's arguments.
        None => unsafe { (d.gl.glInvalidateSubFramebuffer)(target, n, atts, x, y, w, h) },
    }
}

pub unsafe extern "C" fn glDiscardFramebufferEXT(target: u32, n: i32, atts: *const u32) {
    let Some(d) = driver::get() else { return };
    // SAFETY: the caller passes `n` enums.
    match unsafe { mapped(target, n, atts) } {
        // SAFETY: the host entry; `v` holds `n` enums.
        Some(v) => unsafe { (d.gl.glDiscardFramebufferEXT)(target, n, v.as_ptr()) },
        // SAFETY: the host entry with the caller's arguments.
        None => unsafe { (d.gl.glDiscardFramebufferEXT)(target, n, atts) },
    }
}

pub unsafe extern "C" fn glDrawBuffers(n: i32, bufs: *const u32) {
    let Some(d) = driver::get() else { return };
    let f = fbo();
    if f != 0 && n == 1 && !bufs.is_null() && bound(g::GL_DRAW_FRAMEBUFFER as u32) == f {
        // SAFETY: `n` == 1 enum at `bufs`.
        if unsafe { *bufs } == g::GL_BACK as u32 {
            let att = g::GL_COLOR_ATTACHMENT0 as u32;
            // SAFETY: the host entry with one enum.
            unsafe { (d.gl.glDrawBuffers)(1, &att) };
            return;
        }
    }
    // SAFETY: the host entry with the caller's arguments.
    unsafe { (d.gl.glDrawBuffers)(n, bufs) };
}

pub unsafe extern "C" fn glReadBuffer(src: u32) {
    let Some(d) = driver::get() else { return };
    let f = fbo();
    let src = if f != 0 && src == g::GL_BACK as u32 && bound(g::GL_READ_FRAMEBUFFER as u32) == f {
        g::GL_COLOR_ATTACHMENT0 as u32
    } else {
        src
    };
    // SAFETY: the host entry, enum translated.
    unsafe { (d.gl.glReadBuffer)(src) };
}

pub unsafe extern "C" fn glDeleteFramebuffers(n: i32, names: *const u32) {
    let Some(d) = driver::get() else { return };
    let f = fbo();
    let Ok(len) = usize::try_from(n) else {
        // SAFETY: the host entry reports the negative count itself (GL_INVALID_VALUE).
        unsafe { (d.gl.glDeleteFramebuffers)(n, names) };
        return;
    };
    if f == 0 || names.is_null() {
        // SAFETY: the host entry with the caller's arguments.
        unsafe { (d.gl.glDeleteFramebuffers)(n, names) };
        return;
    }
    // SAFETY: the caller passes `n` names.
    let kept: Vec<u32> = unsafe { std::slice::from_raw_parts(names, len) }
        .iter()
        .copied()
        .filter(|&x| x != f)
        .collect();
    // SAFETY: the host entry with the kept names.
    unsafe { (d.gl.glDeleteFramebuffers)(kept.len() as i32, kept.as_ptr()) };
}

pub unsafe extern "C" fn glIsFramebuffer(framebuffer: u32) -> u8 {
    let Some(d) = driver::get() else { return 0 };
    let f = fbo();
    if f != 0 && framebuffer == f {
        return g::GL_FALSE as u8;
    }
    // SAFETY: the host entry with the caller's argument.
    unsafe { (d.gl.glIsFramebuffer)(framebuffer) }
}

// each wrapper has the header's signature exactly (bridge control 1)
const _: g::glBindFramebuffer = glBindFramebuffer;
const _: g::glGetIntegerv = glGetIntegerv;
const _: g::glInvalidateFramebuffer = glInvalidateFramebuffer;
const _: g::glInvalidateSubFramebuffer = glInvalidateSubFramebuffer;
const _: g::glDiscardFramebufferEXT = glDiscardFramebufferEXT;
const _: g::glDrawBuffers = glDrawBuffers;
const _: g::glReadBuffer = glReadBuffer;
const _: g::glDeleteFramebuffers = glDeleteFramebuffers;
const _: g::glIsFramebuffer = glIsFramebuffer;

/// The wrapper for a GL name, as eglGetProcAddress returns it.
pub fn wrapper(name: &[u8]) -> Option<*const ()> {
    Some(match name {
        b"glBindFramebuffer" => glBindFramebuffer as *const (),
        b"glGetIntegerv" => glGetIntegerv as *const (),
        b"glInvalidateFramebuffer" => glInvalidateFramebuffer as *const (),
        b"glInvalidateSubFramebuffer" => glInvalidateSubFramebuffer as *const (),
        b"glDiscardFramebufferEXT" => glDiscardFramebufferEXT as *const (),
        b"glDrawBuffers" => glDrawBuffers as *const (),
        b"glReadBuffer" => glReadBuffer as *const (),
        b"glDeleteFramebuffers" => glDeleteFramebuffers as *const (),
        b"glIsFramebuffer" => glIsFramebuffer as *const (),
        _ => return None,
    })
}
