// SPDX-License-Identifier: MIT
//! The ping hold (display-thread plan D3): a compositor's `xdg_wm_base.ping` is answered only after the app's main Looper ran a liveness token, so a blocked main thread answers none and the desktop shows its own "not responding" dialog, whatever thread the toolkit runs on.
//! GDK pongs inside its own ping listener and has no API to defer it (GTK 4.14.5 and 4.18.6 `gdk/wayland/gdkdisplay-wayland.c` `xdg_wm_base_ping`). So this module exports libwayland-client's `wl_proxy_add_listener` (public ABI, not variadic): libgtk's call binds here, because the module precedes libwayland-client in its RTLD_LOCAL group and libwayland-client is not in the process's global scope.
//! Every call is forwarded to the real function (`dlsym(RTLD_NEXT)`); for the first `xdg_wm_base` proxy only, GDK's listener and data are kept and a one-entry listener installed whose `ping` holds the serial.
//! A held ping with no token in flight posts one through the liveness poster ([`prepare`]); the token, on the app's Looper thread, invokes the toolkit thread, which runs GDK's own handler for every serial held before that token was posted, in order. Serials held while a token is in flight wait for the next token. No timer and no threshold: the answer follows a Looper pass that began after the ping.
//! Capability probe, not an environment name: [`report`] logs once whether GDK's listener was seen during attach (`display: ping hold armed` / `not armed (<reason>)`); not armed = GDK pongs at once. Counters `pings_held`, `pings_answered` (count.rs, every hit).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Mutex, MutexGuard, Once, OnceLock};

use atl_platform_ui_api::LooperPoster;
use gtk4::glib;

use crate::{count, mirror};

/// `wl_proxy_add_listener(proxy, implementation, data)`.
type AddListener = unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int;
/// `wl_proxy_get_class(proxy)`: the proxy's interface name.
type GetClass = unsafe extern "C" fn(*mut c_void) -> *const c_char;
/// `xdg_wm_base_listener.ping(data, xdg_wm_base, serial)`: the interface's one event in every version.
type Ping = unsafe extern "C" fn(*mut c_void, *mut c_void, u32);

/// The interface whose ping is held (xdg-shell.xml).
const WM_BASE: &CStr = c"xdg_wm_base";

/// The deliberate break (`xtask test platform-ui --break-ping-hold`): GDK's listener registered unwrapped, so GDK pongs at once and gate 4's blocked check must fail.
const HOLD: bool = !cfg!(feature = "break-ping-hold");

/// libwayland-client's functions, the next definitions after this module's.
struct Real {
    add_listener: AddListener,
    get_class: GetClass,
}

static REAL: OnceLock<Option<Real>> = OnceLock::new();

fn resolve() -> Option<Real> {
    // SAFETY: RTLD_NEXT from this object and NUL-terminated names; each result is null or that function's address
    let (add, class) = unsafe {
        (
            libc::dlsym(libc::RTLD_NEXT, c"wl_proxy_add_listener".as_ptr()),
            libc::dlsym(libc::RTLD_NEXT, c"wl_proxy_get_class".as_ptr()),
        )
    };
    if add.is_null() || class.is_null() {
        return None;
    }
    // SAFETY: the addresses of libwayland-client's functions of these names, whose C types these are (wayland-client-core.h)
    unsafe {
        Some(Real {
            add_listener: std::mem::transmute::<*mut c_void, AddListener>(add),
            get_class: std::mem::transmute::<*mut c_void, GetClass>(class),
        })
    }
}

fn real() -> &'static Real {
    match REAL.get_or_init(resolve) {
        Some(r) => r,
        None => {
            // nothing to forward to: every caller's listener would be lost, so stop here, loudly
            tracing::error!(
                "atl-platform-ui: display: libwayland-client's wl_proxy_add_listener not found after the module"
            );
            std::process::abort()
        }
    }
}

/// A pointer GDK owns, used only on the toolkit thread (the one that dispatches its Wayland connection).
#[derive(Clone, Copy)]
struct GdkPtr(*mut c_void);
// SAFETY: only read and passed back to GDK on the toolkit thread (`answer` runs there through the toolkit's main context).
unsafe impl Send for GdkPtr {}
// SAFETY: as Send.
unsafe impl Sync for GdkPtr {}

/// GDK's ping listener, as it registered it.
struct Gdk {
    ping: Ping,
    data: GdkPtr,
    wm_base: GdkPtr,
}

static POSTER: OnceLock<Box<dyn LooperPoster>> = OnceLock::new();
static GDK: OnceLock<Gdk> = OnceLock::new();

#[derive(Default)]
struct Pending {
    /// Serials not yet answered, in arrival order.
    held: Vec<u32>,
    /// A token is in flight; it answers this many of `held`'s first serials.
    in_flight: Option<usize>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    held: Vec::new(),
    in_flight: None,
});

fn pending() -> MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Stores the liveness poster. Called in attach before GTK init: GDK binds `xdg_wm_base` while opening the display, and the hold claims it only with a poster in place. A second session keeps the first poster.
pub fn prepare(liveness: Box<dyn LooperPoster>) {
    let _ = POSTER.set(liveness);
}

static REPORTED: Once = Once::new();

/// Logs once per process, after GTK init, whether the hold is armed.
pub fn report() {
    REPORTED.call_once(|| {
        if !HOLD {
            tracing::warn!(
                "atl-platform-ui: display: ping hold not armed (break-ping-hold: GDK's listener registered unwrapped)"
            );
        } else if GDK.get().is_some() {
            tracing::info!("atl-platform-ui: display: ping hold armed");
        } else {
            tracing::warn!(
                "atl-platform-ui: display: ping hold not armed (no xdg_wm_base listener seen during attach): GDK answers pings at once"
            );
        }
    });
}

/// libwayland-client's `wl_proxy_add_listener`, which libgtk binds here (module docs). Called on the thread that owns `proxy`'s queue.
///
/// # Safety
/// libwayland's contract: `proxy` is a live proxy, `implementation` its listener (an array of fn pointers in event order) and `data` the listener's data, kept by libwayland until the proxy is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wl_proxy_add_listener(
    proxy: *mut c_void,
    implementation: *const c_void,
    data: *mut c_void,
) -> c_int {
    let real = real();
    // SAFETY: the caller's contract (above); the hold is claimed for one proxy only
    unsafe {
        if HOLD
            && POSTER.get().is_some()
            && GDK.get().is_none()
            && is_wm_base(real, proxy)
            && !implementation.is_null()
            && let Some(ping) = *implementation.cast::<Option<Ping>>()
            && GDK
                .set(Gdk {
                    ping,
                    data: GdkPtr(data),
                    wm_base: GdkPtr(proxy),
                })
                .is_ok()
        {
            return (real.add_listener)(proxy, HOLD_LISTENER.as_ptr().cast(), data);
        }
        (real.add_listener)(proxy, implementation, data)
    }
}

/// Whether `proxy`'s interface is `xdg_wm_base`.
///
/// # Safety
/// `proxy` is a live proxy.
unsafe fn is_wm_base(real: &Real, proxy: *mut c_void) -> bool {
    // SAFETY: a live proxy (caller); the result is its interface's static name or null
    let name = unsafe { (real.get_class)(proxy) };
    // SAFETY: non-null: a NUL-terminated string libwayland keeps for the interface's life
    !name.is_null() && unsafe { CStr::from_ptr(name) } == WM_BASE
}

/// The listener the held proxy gets in place of GDK's: one event, `ping`.
static HOLD_LISTENER: [Ping; 1] = [held_ping];

/// The held proxy's `ping`, on the toolkit thread inside its Wayland dispatch: holds the serial and posts a token if none is in flight.
unsafe extern "C" fn held_ping(_data: *mut c_void, _wm_base: *mut c_void, serial: u32) {
    count::each("pings_held");
    let post = {
        let mut p = pending();
        p.held.push(serial);
        if p.in_flight.is_none() {
            p.in_flight = Some(p.held.len());
            true
        } else {
            false
        }
    };
    if post {
        post_token();
    }
}

/// Posts the liveness token to the app's Looper. Its closure runs there and only invokes the toolkit thread; the toolkit never waits for it (display-thread B1).
fn post_token() {
    if let Some(poster) = POSTER.get() {
        poster.post(Box::new(|| glib::MainContext::default().invoke(answer)));
    }
}

/// On the toolkit thread, once a token ran on the app's Looper: GDK's own handler for each serial that token covers, in order; a token for the rest, if any.
fn answer() {
    let Some(gdk) = GDK.get() else {
        return;
    };
    let (serials, more) = {
        let mut p = pending();
        let n = p.in_flight.take().unwrap_or(0).min(p.held.len());
        let serials: Vec<u32> = p.held.drain(..n).collect();
        let more = !p.held.is_empty();
        if more {
            p.in_flight = Some(p.held.len());
        }
        (serials, more)
    };
    for serial in serials {
        // SAFETY: GDK's listener with the data and proxy it registered, called on the toolkit thread as libwayland would call it
        unsafe { (gdk.ping)(gdk.data.0, gdk.wm_base.0, serial) };
        count::each("pings_answered");
    }
    // the pong is queued on the connection; GDK flushes in its source's prepare, which a call outside a mirror pass never reaches (a no-op inside a pass: its sync prepares)
    mirror::resync();
    if more {
        post_token();
    }
}
