// SPDX-License-Identifier: MIT
//! A roundtrip on the window surface's own event queue: `Window::on_placed`'s answer to the first commit. GDK reads its `wl_surface` and xdg objects' events into a queue of its own, not the default one
//! (WAYLAND_DEBUG shows them untagged), so `gdk_display_sync` (a default-queue roundtrip) returned with the surface's `enter` read but not yet dispatched. A sync on the surface's queue is ordered with
//! that queue's events: its `done` is dispatched after every event the compositor sent before it, the enter of a commit it handled first included.
//! libwayland-client is reached as `ping` reaches it (`dlsym(RTLD_NEXT)`: it is not in the process's global scope); `wl_proxy_get_queue` is libwayland 1.23 (wayland-client-core.h).

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;

use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::gdk;
use gtk4::prelude::*;

/// `wl_proxy_get_queue(proxy)`.
type GetQueue = unsafe extern "C" fn(*const c_void) -> *mut c_void;
/// `wl_display_roundtrip_queue(display, queue)`: -1 on error.
type RoundtripQueue = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

struct Real {
    get_queue: GetQueue,
    roundtrip_queue: RoundtripQueue,
}

static REAL: OnceLock<Option<Real>> = OnceLock::new();

fn resolve() -> Option<Real> {
    // SAFETY: RTLD_NEXT from this object and NUL-terminated names; each result is null or that function's address
    let (get, round) = unsafe {
        (
            libc::dlsym(libc::RTLD_NEXT, c"wl_proxy_get_queue".as_ptr()),
            libc::dlsym(libc::RTLD_NEXT, c"wl_display_roundtrip_queue".as_ptr()),
        )
    };
    if get.is_null() || round.is_null() {
        return None;
    }
    // SAFETY: the addresses of libwayland-client's functions of these names, whose C types these are (wayland-client-core.h)
    unsafe {
        Some(Real {
            get_queue: std::mem::transmute::<*mut c_void, GetQueue>(get),
            roundtrip_queue: std::mem::transmute::<*mut c_void, RoundtripQueue>(round),
        })
    }
}

/// A roundtrip on `surface`'s queue (null queue = the default one): every event the compositor sent before answering is dispatched when this returns. Err names why none was made.
pub(crate) fn roundtrip(surface: &gdk::Surface) -> Result<(), String> {
    let real = REAL
        .get_or_init(resolve)
        .as_ref()
        .ok_or("libwayland-client 1.23 functions not found")?;
    let wayland = surface
        .downcast_ref::<gdk4_wayland::WaylandSurface>()
        .ok_or("the window's surface is not a Wayland surface")?;
    let wl_surface = wayland
        .wl_surface()
        .ok_or("the Wayland surface has no wl_surface")?;
    let backend = wayland_client::Proxy::backend(&wl_surface)
        .upgrade()
        .ok_or("GDK's Wayland connection is gone")?;
    let display = backend.display_ptr().cast::<c_void>();
    let proxy = wayland_client::Proxy::id(&wl_surface)
        .as_ptr()
        .cast::<c_void>();
    // SAFETY: a live wl_proxy and GDK's live wl_display, on the display thread that dispatches GDK's queues
    let rc = unsafe {
        let queue = (real.get_queue)(proxy);
        (real.roundtrip_queue)(display, queue)
    };
    if rc < 0 {
        return Err(format!("wl_display_roundtrip_queue: {rc}"));
    }
    Ok(())
}
