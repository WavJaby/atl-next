// SPDX-License-Identifier: MIT
//! ILP32 software rendering: `ANativeWindow_lock` / `ANativeWindow_unlockAndPost` (build.rs `ILP32_PROXY_SHIMS`). The host's locked buffer (`bits`) is host memory above 4 GiB the guest cannot address, so the guest draws into a window copy: lock copies the host buffer in (native_window.h: the buffer keeps the previous frame's pixels outside the dirty region, so a partial redraw needs them), unlockAndPost copies it back out, then posts.
//! Why a copy and not the host buffer mapped below 4 GiB: between lock and post the buffer is dequeued, owned by this producer alone, so the copy is never read concurrently; mapping it low would need every host allocator (gralloc, dmabuf heaps, Surface's own) to place its CPU mapping below 4 GiB, a change across the host stack. Cost: two frame-sized copies per locked frame.
//! One window per host ANativeWindow, grown as the buffer grows, kept for the next frame (a frame-rate lock reuses it). Never given back: the window object is reference-counted on the host, so no call says it ended.

use std::collections::HashMap;
use std::sync::Mutex;

use super::{bindings, convert, handles};

/// native_window.h `ANativeWindow_Buffer` in the host's (SysV x86_64) layout: int32 width, height, stride (pixels), format; void* bits; uint32 reserved[6].
#[repr(C)]
#[derive(Default)]
struct HostBuffer {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    bits: usize,
    reserved: [u32; 6],
}
// the hand layout convert.rs's test `WINDOW_BUFFER` states (host size 48, bits at 16)
const _: () = assert!(std::mem::size_of::<HostBuffer>() == 48);
const _: () = assert!(std::mem::offset_of!(HostBuffer, bits) == 16);

/// One host window's copy: guest-addressable `window` of `cap` bytes; while locked, the host buffer and the bytes it spans.
struct Copy {
    window: usize,
    cap: usize,
    locked: Option<(usize, usize)>,
}

static COPIES: Mutex<Option<HashMap<usize, Copy>>> = Mutex::new(None);

/// Bytes per pixel of a lockable format (native_window.h `ANativeWindow_LegacyFormat`, hardware_buffer.h `AHardwareBuffer_Format`), None for one the CPU path does not lay out as packed pixels.
fn pixel_bytes(format: i32) -> Option<usize> {
    Some(match format {
        // RGBA_8888, RGBX_8888, R10G10B10A2_UNORM
        1 | 2 | 0x2b => 4,
        // R8G8B8_UNORM
        3 => 3,
        // RGB_565
        4 => 2,
        // R16G16B16A16_FLOAT
        0x16 => 8,
        _ => return None,
    })
}

/// The bytes a locked buffer spans: `stride` pixels per row, `height` rows. None for an unknown format or a negative size.
fn span(stride: i32, height: i32, format: i32) -> Option<usize> {
    let rows = usize::try_from(height).ok()?;
    let row = usize::try_from(stride)
        .ok()?
        .checked_mul(pixel_bytes(format)?)?;
    rows.checked_mul(row)
}

/// The host's export of `name` (libnativewindow; libandroid re-exports the same function).
fn host(name: &str) -> usize {
    bindings::system_symbol("libnativewindow.so", name)
        .or_else(|| bindings::system_symbol("libandroid.so", name))
        .unwrap_or_else(|| super::unsupported(&format!("{name}: no host libnativewindow")))
}

fn refuse(what: &str) -> i32 {
    super::dispatch::ILP32_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::dispatch::report_refused(super::dispatch::REFUSED_HANDLE, what);
    -libc::EINVAL
}

/// `int32_t ANativeWindow_lock(ANativeWindow*, ANativeWindow_Buffer* outBuffer, ARect* inOutDirtyBounds)`, the arm guest's: `window` a handle, `out` the guest's record (44 bytes), `dirty` a guest `ARect` (four int32, one layout in both models) passed through. An unknown handle or an unlayable format answers -EINVAL, counted; the latter after the host buffer is posted unchanged (the NDK has no unlock without post).
pub(super) extern "C" fn lock(window: usize, out: usize, dirty: usize) -> i32 {
    let Some(win) = handles::to_host(window).filter(|&w| w != 0) else {
        return refuse(&format!("ANativeWindow_lock window = {window:#x}"));
    };
    let mut hb = HostBuffer::default();
    type Lock = unsafe extern "C" fn(usize, *mut HostBuffer, usize) -> i32;
    // SAFETY: libnativewindow's `ANativeWindow_lock`, this prototype; `dirty` is NULL or a guest ARect the host can read and write (below 4 GiB, host-addressable).
    let rc = unsafe {
        std::mem::transmute::<usize, Lock>(host("ANativeWindow_lock"))(win, &mut hb, dirty)
    };
    if rc != 0 {
        return rc;
    }
    let Some(len) = span(hb.stride, hb.height, hb.format) else {
        unlock_host(win);
        return refuse(&format!(
            "ANativeWindow_lock format {} stride {} height {}",
            hb.format, hb.stride, hb.height
        ));
    };
    let Some(at) = copy_in(win, hb.bits, len) else {
        unlock_host(win);
        return refuse(&format!("ANativeWindow_lock: no window for {len} bytes"));
    };
    hb.bits = at;
    if out != 0 {
        // SAFETY: `hb` is the host layout; `out` the guest's writable record (the classifier's `record:ANativeWindow_Buffer`); `at` is below 4 GiB, so no field is refused.
        unsafe {
            convert::to_guest(
                &bindings::qemu_arm::C_ANativeWindow_Buffer,
                (&hb as *const HostBuffer).cast(),
                out as *mut u8,
            )
        };
    }
    0
}

/// `int32_t ANativeWindow_unlockAndPost(ANativeWindow*)`, the arm guest's: the window copy back into the host buffer, then the host's post.
pub(super) extern "C" fn unlock_and_post(window: usize) -> i32 {
    let Some(win) = handles::to_host(window).filter(|&w| w != 0) else {
        return refuse(&format!("ANativeWindow_unlockAndPost window = {window:#x}"));
    };
    copy_out(win);
    unlock_host(win)
}

fn unlock_host(win: usize) -> i32 {
    type Unlock = unsafe extern "C" fn(usize) -> i32;
    // SAFETY: libnativewindow's `ANativeWindow_unlockAndPost`, this prototype, on a host window.
    unsafe { std::mem::transmute::<usize, Unlock>(host("ANativeWindow_unlockAndPost"))(win) }
}

/// Copies `len` bytes of host buffer `bits` into host window `win`'s guest window (grown when short), recording the lock. The window's address, None when no window memory is left.
fn copy_in(win: usize, bits: usize, len: usize) -> Option<usize> {
    let mut g = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    let c = match map.get_mut(&win) {
        Some(c) if c.cap >= len => c,
        old => {
            if let Some(o) = old {
                super::active().release(o.window, o.cap);
            }
            let window = super::active()
                .reserve(len.max(1), (libc::PROT_READ | libc::PROT_WRITE) as u32)
                .ok()?;
            map.insert(
                win,
                Copy {
                    window,
                    cap: len.max(1),
                    locked: None,
                },
            );
            map.get_mut(&win)?
        }
    };
    // SAFETY: `bits` is the host's locked buffer of at least `len` bytes (stride x height x pixel size); `c.window` holds `c.cap >= len`.
    unsafe { std::ptr::copy_nonoverlapping(bits as *const u8, c.window as *mut u8, len) };
    c.locked = Some((bits, len));
    Some(c.window)
}

/// Copies host window `win`'s guest window back into the host buffer it was locked from; nothing when it is not locked.
fn copy_out(win: usize) {
    let mut g = COPIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut().and_then(|m| m.get_mut(&win))
        && let Some((bits, len)) = c.locked.take()
    {
        // SAFETY: the buffer locked in `copy_in`, still locked (unlockAndPost has not run), `len` bytes; the window holds them.
        unsafe { std::ptr::copy_nonoverlapping(c.window as *const u8, bits as *mut u8, len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spans from native_window.h's formats: RGBA_8888 stride 64 x 32 rows = 8192; RGB_565 = 4096; an unknown format and a negative height refuse.
    #[test]
    fn spans_follow_the_formats() {
        assert_eq!(span(64, 32, 1), Some(64 * 32 * 4));
        assert_eq!(span(64, 32, 4), Some(64 * 32 * 2));
        assert_eq!(span(64, 32, 0x7fff), None);
        assert_eq!(span(64, -1, 1), None);
    }

    /// Lock copies the host pixels into the window; the guest's drawing reaches the host buffer at post; the window is reused for the next frame of the same size.
    #[test]
    fn the_window_copy_round_trips_pixels() {
        let win = 0x7f00_dead_0000usize;
        let mut host_buf = vec![0x11u8; 256];
        let at = copy_in(win, host_buf.as_mut_ptr() as usize, 256).unwrap();
        // SAFETY: the window holds 256 bytes.
        let w = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, 256) };
        assert!(w.iter().all(|&b| b == 0x11), "previous frame copied in");
        w[0] = 0x22;
        w[255] = 0x33;
        copy_out(win);
        assert_eq!(
            (host_buf[0], host_buf[1], host_buf[255]),
            (0x22, 0x11, 0x33)
        );
        // a second post without a lock copies nothing
        host_buf[0] = 0;
        copy_out(win);
        assert_eq!(host_buf[0], 0);
        assert_eq!(
            copy_in(win, host_buf.as_mut_ptr() as usize, 128),
            Some(at),
            "reused"
        );
        copy_out(win);
    }
}
