// SPDX-License-Identifier: MIT
//! The `ANativeWindow` / `ANativeWindowBuffer` a window surface draws into, behind safe types: the calls `system/window.h`'s inline helpers make (what Mesa's platform_android uses too), on the generated layout.
//! A window is held with `incRef` for the surface's life; a dequeued buffer with `incRef` until queued or cancelled (window.h: the producer owns a dequeued buffer until it gives it back).

use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::ptr::NonNull;

use crate::abi::window::{ANativeWindow, ANativeWindowBuffer, android_native_base_t};

/// A window, referenced (`common.incRef`) while this lives.
pub struct Window(NonNull<ANativeWindow>);

// SAFETY: an ANativeWindow is called from any thread (libgui's Surface serialises its hooks on its own mutex; window.h requires the hooks to be thread-safe).
unsafe impl Send for Window {}

/// A negative errno from a window hook.
pub type Status = i32;

fn status(r: i32) -> Result<i32, Status> {
    if r < 0 { Err(r) } else { Ok(r) }
}

impl Window {
    /// Takes a reference on `raw`. `None` for a null window or one without the hooks window.h requires.
    /// # Safety
    /// `raw` is null or a live `ANativeWindow` (the one eglCreateWindowSurface received).
    pub unsafe fn acquire(raw: *mut ANativeWindow) -> Option<Self> {
        let w = NonNull::new(raw)?;
        // SAFETY: the caller guarantees a live window; the fields read are plain data.
        let ok = unsafe {
            let a = w.as_ref();
            a.query.is_some()
                && a.perform.is_some()
                && a.dequeueBuffer.is_some()
                && a.queueBuffer.is_some()
                && a.cancelBuffer.is_some()
        };
        if !ok {
            return None;
        }
        // SAFETY: live window; `common` is its first member (window.h), incRef takes it.
        unsafe { inc_ref(&raw mut (*raw).common) };
        Some(Self(w))
    }

    fn raw(&self) -> *mut ANativeWindow {
        self.0.as_ptr()
    }

    pub fn query(&self, what: u32) -> Result<i32, Status> {
        let mut v = 0;
        // SAFETY: live window (held); `query` checked present at acquire; `v` is a valid out-pointer.
        let r = unsafe { ((*self.raw()).query.expect("checked at acquire"))(self.raw(), what as i32, &mut v) };
        status(r).map(|_| v)
    }

    /// `perform(op, int)`: the window.h helpers that take one `int` (`native_window_set_buffers_transform`).
    pub fn perform_int(&self, op: u32, arg: i32) -> Result<(), Status> {
        // SAFETY: live window; `perform` is variadic and the op's argument is one int (window.h).
        let r = unsafe { ((*self.raw()).perform.expect("checked at acquire"))(self.raw(), op as i32, arg) };
        status(r).map(drop)
    }

    /// `perform(op, uint64_t)`: `native_window_set_usage`.
    pub fn perform_u64(&self, op: u32, arg: u64) -> Result<(), Status> {
        // SAFETY: as perform_int; the op's argument is one uint64_t.
        let r = unsafe { ((*self.raw()).perform.expect("checked at acquire"))(self.raw(), op as i32, arg) };
        status(r).map(drop)
    }

    pub fn set_swap_interval(&self, interval: i32) -> Result<(), Status> {
        // SAFETY: live window; setSwapInterval may be null on a minimal window (then a no-op).
        match unsafe { (*self.raw()).setSwapInterval } {
            // SAFETY: the hook of this live window.
            Some(f) => status(unsafe { f(self.raw(), interval) }).map(drop),
            None => Ok(()),
        }
    }

    /// The next buffer and the fence to wait on before writing it (`None`: ready now).
    pub fn dequeue(&self) -> Result<(Buffer, Option<OwnedFd>), Status> {
        let mut buf = std::ptr::null_mut();
        let mut fence = -1;
        // SAFETY: live window; both out-pointers valid.
        let r = unsafe { ((*self.raw()).dequeueBuffer.expect("checked at acquire"))(self.raw(), &mut buf, &mut fence) };
        status(r)?;
        // SAFETY: on success `fence` is -1 or a new fd the caller owns (window.h dequeueBuffer).
        let fence = (fence >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fence) });
        // SAFETY: a successful dequeue gives a live buffer the producer owns until queue / cancel.
        let b = unsafe { Buffer::acquire(buf) }.ok_or(-EINVAL)?;
        Ok((b, fence))
    }

    /// Hands `buf` to the consumer with `fence` (signalled when rendering is done; `None`: done now). The window takes the fence.
    pub fn queue(&self, buf: Buffer, fence: Option<OwnedFd>) -> Result<(), Status> {
        let fd = fence.map_or(-1, IntoRawFd::into_raw_fd);
        // SAFETY: live window; `buf` was dequeued from it; the window owns `fd` from here on, also on failure (window.h).
        let r = unsafe { ((*self.raw()).queueBuffer.expect("checked at acquire"))(self.raw(), buf.raw(), fd) };
        status(r).map(drop)
    }

    /// Gives `buf` back unused.
    pub fn cancel(&self, buf: Buffer) {
        // SAFETY: live window; `buf` was dequeued from it; no fence.
        unsafe { ((*self.raw()).cancelBuffer.expect("checked at acquire"))(self.raw(), buf.raw(), -1) };
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the reference taken at acquire, dropped once.
        unsafe { dec_ref(&raw mut (*self.raw()).common) };
    }
}

/// Linux EINVAL (asm-generic/errno-base.h, every arches.toml host): what a dequeue that gave a null buffer answers.
const EINVAL: i32 = 22;

/// A window buffer, referenced while this lives.
pub struct Buffer(NonNull<ANativeWindowBuffer>);

// SAFETY: as Window: the buffer's refcount is atomic (libui GraphicBuffer / RefBase) and the fields read are immutable after allocation.
unsafe impl Send for Buffer {}

/// What the driver needs of a buffer's `native_handle_t`.
pub struct Handle<'b> {
    pub fds: &'b [i32],
    pub ints: &'b [i32],
}

impl Buffer {
    /// # Safety
    /// `raw` is null or a live `ANativeWindowBuffer`.
    pub unsafe fn acquire(raw: *mut ANativeWindowBuffer) -> Option<Self> {
        let b = NonNull::new(raw)?;
        // SAFETY: live buffer; `common` is its first member (nativebase.h).
        unsafe { inc_ref(&raw mut (*raw).common) };
        Some(Self(b))
    }

    pub fn raw(&self) -> *mut ANativeWindowBuffer {
        self.0.as_ptr()
    }

    pub fn width(&self) -> i32 {
        // SAFETY: live buffer (held), plain field.
        unsafe { self.0.as_ref().width }
    }

    pub fn height(&self) -> i32 {
        // SAFETY: as width.
        unsafe { self.0.as_ref().height }
    }

    /// The buffer's handle; `None` when it has none or its counts are negative.
    pub fn handle(&self) -> Option<Handle<'_>> {
        // SAFETY: live buffer; `handle` is null or a native_handle_t that lives as long as the buffer (nativebase.h).
        let h = unsafe { self.0.as_ref().handle.as_ref() }?;
        let fds = usize::try_from(h.numFds).ok()?;
        let ints = usize::try_from(h.numInts).ok()?;
        // SAFETY: `data` holds numFds fds then numInts ints (cutils/native_handle.h), all within the handle's allocation.
        let all = unsafe { std::slice::from_raw_parts(h.data.as_ptr(), fds + ints) };
        let (fds, ints) = all.split_at(fds);
        Some(Handle { fds, ints })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the reference taken at acquire, dropped once.
        unsafe { dec_ref(&raw mut (*self.raw()).common) };
    }
}

/// # Safety
/// `base` is the live object's `android_native_base_t`.
unsafe fn inc_ref(base: *mut android_native_base_t) {
    // SAFETY: per the caller; a null hook means the object is not refcounted.
    if let Some(f) = unsafe { (*base).incRef } {
        // SAFETY: the object's own hook on itself.
        unsafe { f(base) };
    }
}

/// # Safety
/// As [`inc_ref`], with a reference this caller took.
unsafe fn dec_ref(base: *mut android_native_base_t) {
    // SAFETY: per the caller.
    if let Some(f) = unsafe { (*base).decRef } {
        // SAFETY: the object's own hook on itself, releasing our reference.
        unsafe { f(base) };
    }
}
