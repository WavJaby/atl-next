// SPDX-License-Identifier: MIT
//! A transparent Wayland proxy for `xtask test platform-ui`'s ping probe.
//!
//! Clients connect to the proxy's socket; every byte and every passed fd is forwarded unchanged to the real compositor and back. On the way it decodes the few messages it needs, whole messages only (8-byte header: object id, `size << 16 | opcode`, native endian):
//! * `wl_display.get_registry` and `wl_registry.bind` -> which object id is which interface (`xdg_wm_base`, `wl_shm`, ...);
//! * `xdg_wm_base.pong(serial)` (request opcode 3) -> [`Event::Pong`], `wl_shm.create_pool` (opcode 0) -> [`Event::ShmPool`];
//! * `xdg_wm_base.ping(serial)` from the compositor (event opcode 0) -> [`Event::Ping`] with `injected: false`;
//! * `xdg_toplevel.set_app_id(app_id)` of the window's toplevel (request opcode 3) -> [`Event::AppId`], its `set_minimized` (opcode 13) -> [`Event::Minimized`];
//! * the window's first `wl_surface.commit` (xdg-shell's initial one, no buffer) -> [`Event::FirstCommit`].
//!
//! Injections go to the connection that bound `xdg_wm_base` (the window's); an app's other connections forward only.
//! [`Proxy::inject_ping`] writes a synthetic `xdg_wm_base.ping(serial)` to the client between two forwarded messages, which is what a compositor's ping is on the wire. A client whose main loop is blocked reads nothing, so no `pong` comes back; a client iterating answers at once.
//! [`Proxy::inject_pointer`] writes `wl_pointer` events (enter, leave, motion, button, wheel and finger scrolls, each then `frame`; scrolls honour the bound `wl_seat` version) on the pointer `wl_seat.get_pointer` created, at the surface of the first `xdg_surface` (the window); the client sees them as the compositor's pointer input.
//! [`Proxy::inject_touch`] writes `wl_touch` events on a touch that is the proxy's alone: the gates' compositor has no touch device and wlroots has no virtual-touch protocol, so the proxy adds TOUCH to `wl_seat.capabilities`, turns the client's `get_touch` into a `wl_display.sync` (which takes the new id in order), drops that callback's `done` and `delete_id`, and absorbs the client's requests on the touch.
//! [`Proxy::inject_configure`] writes an `xdg_toplevel.configure` and `xdg_surface.configure` of the window the same way; the client's `ack_configure` is [`Event::Acked`].
//! That tests the client's side of the protocol exactly (GDK answers pings from its Wayland source, on the thread whose Looper polls the main context), not the compositor's reaction to a missing pong, which stays the compositor's business.
//!
//! Forwarding keeps libwayland's fd semantics: fds are a FIFO attached to bytes, so each batch of complete messages goes out with all fds received so far (at most [`MAX_FDS_PER_SEND`] per `sendmsg`).

use std::collections::HashMap;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::io::{read, write};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

/// Message header: object id and `size << 16 | opcode`.
const HEADER_BYTES: usize = 8;
/// `wl_display` is object 1; its request 1 is `get_registry(new_id)`.
const WL_DISPLAY_ID: u32 = 1;
const WL_DISPLAY_GET_REGISTRY: u16 = 1;
/// `wl_registry.bind(name, interface, version, new_id)`, request 0.
const WL_REGISTRY_BIND: u16 = 0;
/// `xdg_wm_base`: request `pong(serial)` is opcode 3, event `ping(serial)` is opcode 0.
const XDG_WM_BASE_PONG: u16 = 3;
const XDG_WM_BASE_PING: u16 = 0;
const XDG_WM_BASE_GET_XDG_SURFACE: u16 = 2;
const XDG_SURFACE: &str = "xdg_surface";
const XDG_SURFACE_ACK_CONFIGURE: u16 = 4;
const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
const XDG_SURFACE_SET_WINDOW_GEOMETRY: u16 = 3;
const XDG_TOPLEVEL: &str = "xdg_toplevel";
/// `xdg_toplevel.set_app_id(app_id: string)`, request 3 (xdg-shell.xml).
const XDG_TOPLEVEL_SET_APP_ID: u16 = 3;
/// `xdg_toplevel.set_minimized()`, request 13 (xdg-shell.xml: destroy 0 ... unset_fullscreen 12, set_minimized 13).
const XDG_TOPLEVEL_SET_MINIMIZED: u16 = 13;
/// `wl_surface.commit` request 6 (wayland.xml).
const WL_SURFACE_COMMIT: u16 = 6;
/// The `configure` events of xdg_toplevel and xdg_surface are both opcode 0 (their requests are other opcodes).
const XDG_TOPLEVEL_CONFIGURE: u16 = 0;
const XDG_SURFACE_CONFIGURE: u16 = 0;
/// xdg-shell's `activated` window state: the injected configure says the window has focus, as the compositor's do for the focused window.
const XDG_STATE_ACTIVATED: u32 = 4;
/// xdg-shell `xdg_toplevel.state` values for [`Proxy::inject_configure`]'s `states` (xdg-shell.xml, stable: maximized = 1, suspended = 9 since version 6).
pub const XDG_STATE_MAXIMIZED: u32 = 1;
pub const XDG_STATE_SUSPENDED: u32 = 9;
const WL_SEAT: &str = "wl_seat";
const WL_SEAT_GET_POINTER: u16 = 0;
const WL_SEAT_GET_KEYBOARD: u16 = 1;
const WL_SEAT_GET_TOUCH: u16 = 2;
const WL_KEYBOARD_KEY: u16 = 3;
/// `wl_seat.capabilities(caps)`, event 0; the touch bit (wayland.xml `wl_seat.capability`).
const WL_SEAT_CAPABILITIES: u16 = 0;
const SEAT_CAP_TOUCH: u32 = 4;
/// `wl_display.sync(new_id)` request 0, `delete_id(id)` event 1.
const WL_DISPLAY_SYNC: u16 = 0;
const WL_DISPLAY_DELETE_ID: u16 = 1;
/// `wl_touch` events (wayland.xml): down, up, motion, frame.
const WL_TOUCH_DOWN: u16 = 0;
const WL_TOUCH_UP: u16 = 1;
const WL_TOUCH_MOTION: u16 = 2;
const WL_TOUCH_FRAME: u16 = 3;
const WL_POINTER: &str = "wl_pointer";
/// `wl_pointer` events (wayland.xml), with the version that introduced each one past v1.
const WL_POINTER_ENTER: u16 = 0;
const WL_POINTER_LEAVE: u16 = 1;
const WL_POINTER_MOTION: u16 = 2;
const WL_POINTER_BUTTON: u16 = 3;
const WL_POINTER_AXIS: u16 = 4;
const WL_POINTER_FRAME: u16 = 5;
const WL_POINTER_AXIS_SOURCE: u16 = 6;
const WL_POINTER_AXIS_STOP: u16 = 7;
const WL_POINTER_AXIS_DISCRETE: u16 = 8;
const WL_POINTER_AXIS_VALUE120: u16 = 9;
/// frame, axis_source, axis_stop, axis_discrete: v5; axis_value120 (replacing axis_discrete): v8.
const POINTER_V_FRAME: u32 = 5;
const POINTER_V_VALUE120: u32 = 8;
/// `wl_pointer.axis` axes and `axis_source` values (wayland.xml).
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;
const AXIS_SOURCE_WHEEL: u32 = 0;
const AXIS_SOURCE_FINGER: u32 = 1;
/// The `axis` value of one wheel notch: libinput's 15 degrees per detent, which compositors forward as the axis value.
const WHEEL_NOTCH_VALUE: f64 = 15.0;
/// `axis_value120` units per notch (wayland.xml: "a value of 120 is one logical scroll step").
const VALUE120_PER_NOTCH: f64 = 120.0;
/// wl_fixed is 24.8 signed.
const FIXED_ONE: f64 = 256.0;
/// `wl_shm.create_pool(new_id, fd, size)`, request 0.
const WL_SHM_CREATE_POOL: u16 = 0;
/// libwayland's `MAX_FDS_OUT`: fds one `sendmsg` carries (wayland connection.c).
const MAX_FDS_PER_SEND: usize = 28;
/// Ancillary buffer for one `SCM_RIGHTS` message of `MAX_FDS_PER_SEND` fds.
const FD_CONTROL_BYTES: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_PER_SEND));
/// Read size per `recvmsg`; Wayland traffic between a client and its compositor is small.
const READ_BYTES: usize = 64 * 1024;
const WM_BASE: &str = "xdg_wm_base";
const SHM: &str = "wl_shm";

/// What the proxy saw, in order, stamped with the time it saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The client bound a global (`wl_registry.bind`).
    Bound(String),
    /// `xdg_wm_base.ping`: injected by [`Proxy::inject_ping`], or sent by the real compositor.
    Ping { serial: u32, injected: bool },
    /// The client's `xdg_wm_base.pong`.
    Pong { serial: u32 },
    /// The client created a `wl_shm` pool (shared-memory buffers are in use).
    ShmPool,
    /// The client acknowledged a configure (`xdg_surface.ack_configure`): the window is set up.
    Acked { serial: u32 },
    /// `xdg_surface.set_window_geometry` of the window: the part of its surface that is the window, shadow excluded.
    Geometry {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// The client's `xdg_toplevel.set_app_id` on the window: the id the compositor sees the window by. Every one is sent, in order.
    AppId { app_id: String },
    /// The client's `xdg_toplevel.set_minimized` on the window: it asked the desktop to minimize it.
    Minimized,
    /// The window's first `wl_surface.commit`: the initial one, which carries no buffer; the desktop matches the window's rules on it, before any frame.
    FirstCommit,
    /// [`Proxy::inject_pointer`]'s action was written to the client (a dropped action reports nothing).
    PointerInjected,
    /// [`Proxy::inject_configure`]'s configure was written to the client; its `ack_configure` with the same serial comes back as [`Event::Acked`].
    ConfigureInjected { serial: u32 },
    /// The client asked for a `wl_touch` (`wl_seat.get_touch`); it is the proxy's (the compositor never sees it), and [`Proxy::inject_touch`] writes on it.
    TouchCreated,
    /// [`Proxy::inject_touch`]'s action was written to the client.
    TouchInjected,
    /// A client connection closed.
    Closed,
}

/// Object ids of one connection and the interface each was bound as.
#[derive(Default)]
struct Ids {
    registry: Option<u32>,
    interfaces: HashMap<u32, String>,
    /// The client's `wl_pointer` (from `wl_seat.get_pointer`).
    pointer: Option<u32>,
    /// The client's `wl_keyboard` (from `wl_seat.get_keyboard`): the compositor's, [`Proxy::inject_key`] writes on it.
    keyboard: Option<u32>,
    /// The client's `wl_touch`, the proxy's own (see [`Proxy::inject_touch`]).
    touch: Option<u32>,
    /// The version the client bound `wl_seat` at, which its `wl_pointer` inherits: events newer than it must not be sent.
    seat_version: Option<u32>,
    /// The `wl_surface` of the first `xdg_surface`: the window.
    surface: Option<u32>,
    /// The window's first commit was reported.
    first_commit: bool,
    /// The window's shell roles: its first `xdg_surface` and that surface's `xdg_toplevel`, which [`Proxy::inject_configure`] addresses.
    xdg_surface: Option<u32>,
    toplevel: Option<u32>,
    /// The serials [`Proxy::inject_configure`] wrote: the client's acks of them are absorbed (see `client_to_server`).
    injected: Vec<u32>,
}

impl Ids {
    fn id_of(&self, interface: &str) -> Option<u32> {
        self.interfaces
            .iter()
            .find_map(|(id, name)| (name == interface).then_some(*id))
    }
}

fn word(body: &[u8], at: usize) -> Option<u32> {
    body.get(at..at + 4)
        .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
}

/// A wire `string` at `at`: a u32 length that counts the NUL, the bytes, padding to 4 (wayland-util). Lossy UTF-8.
fn wl_string(body: &[u8], at: usize) -> Option<String> {
    let len = word(body, at)? as usize;
    let text = body.get(at + 4..at + 4 + len)?;
    Some(String::from_utf8_lossy(text.strip_suffix(&[0]).unwrap_or(text)).into_owned())
}

/// The first complete message of `buf`: (object id, opcode, total size).
fn message(buf: &[u8]) -> Option<(u32, u16, usize)> {
    let id = word(buf, 0)?;
    let second = word(buf, 4)?;
    let size = (second >> 16) as usize;
    (size >= HEADER_BYTES && size <= buf.len()).then_some((id, (second & 0xffff) as u16, size))
}

/// What a client request means to the probe; also learns the interface of each bound id.
fn note_request(ids: &mut Ids, object: u32, opcode: u16, body: &[u8]) -> Option<Event> {
    if object == WL_DISPLAY_ID && opcode == WL_DISPLAY_GET_REGISTRY {
        ids.registry = word(body, 0);
        return None;
    }
    if Some(object) == ids.registry && opcode == WL_REGISTRY_BIND {
        // name: u32, interface: string (length with NUL, bytes padded to 4), version: u32, new_id: u32
        let len = word(body, 4)? as usize;
        let text = body.get(8..8 + len)?;
        let name = String::from_utf8_lossy(text.strip_suffix(&[0]).unwrap_or(text)).into_owned();
        let at = 8 + len.div_ceil(4) * 4;
        let (version, new_id) = (word(body, at)?, word(body, at + 4)?);
        if name == WL_SEAT {
            ids.seat_version = Some(version);
        }
        ids.interfaces.insert(new_id, name.clone());
        return Some(Event::Bound(name));
    }
    // object creations first: they insert ids, which the interface match below reads
    if ids.interfaces.get(&object).is_some_and(|i| i == WL_SEAT) && opcode == WL_SEAT_GET_POINTER {
        let pointer = word(body, 0)?;
        ids.interfaces.insert(pointer, WL_POINTER.to_owned());
        ids.pointer = Some(pointer);
        return None;
    }
    if ids.interfaces.get(&object).is_some_and(|i| i == WL_SEAT) && opcode == WL_SEAT_GET_KEYBOARD {
        ids.keyboard = word(body, 0);
        return None;
    }
    if ids.interfaces.get(&object).is_some_and(|i| i == WL_SEAT) && opcode == WL_SEAT_GET_TOUCH {
        ids.touch = word(body, 0);
        return Some(Event::TouchCreated);
    }
    if ids.interfaces.get(&object).is_some_and(|i| i == WM_BASE)
        && opcode == XDG_WM_BASE_GET_XDG_SURFACE
    {
        // new_id: u32, surface: object
        let (xdg, surface) = (word(body, 0)?, word(body, 4)?);
        ids.interfaces.insert(xdg, XDG_SURFACE.to_owned());
        ids.surface.get_or_insert(surface);
        ids.xdg_surface.get_or_insert(xdg);
        return None;
    }
    if ids
        .interfaces
        .get(&object)
        .is_some_and(|i| i == XDG_SURFACE)
        && opcode == XDG_SURFACE_GET_TOPLEVEL
    {
        // new_id: u32
        let toplevel = word(body, 0)?;
        ids.interfaces.insert(toplevel, XDG_TOPLEVEL.to_owned());
        ids.toplevel.get_or_insert(toplevel);
        return None;
    }
    if Some(object) == ids.surface && opcode == WL_SURFACE_COMMIT {
        let initial = !ids.first_commit;
        ids.first_commit = true;
        return initial.then_some(Event::FirstCommit);
    }
    match (ids.interfaces.get(&object).map(String::as_str), opcode) {
        (Some(XDG_TOPLEVEL), XDG_TOPLEVEL_SET_APP_ID) if ids.toplevel == Some(object) => {
            Some(Event::AppId {
                app_id: wl_string(body, 0)?,
            })
        }
        (Some(XDG_TOPLEVEL), XDG_TOPLEVEL_SET_MINIMIZED) if ids.toplevel == Some(object) => {
            Some(Event::Minimized)
        }
        (Some(WM_BASE), XDG_WM_BASE_PONG) => Some(Event::Pong {
            serial: word(body, 0)?,
        }),
        (Some(SHM), WL_SHM_CREATE_POOL) => Some(Event::ShmPool),
        (Some(XDG_SURFACE), XDG_SURFACE_ACK_CONFIGURE) => Some(Event::Acked {
            serial: word(body, 0)?,
        }),
        (Some(XDG_SURFACE), XDG_SURFACE_SET_WINDOW_GEOMETRY) if ids.xdg_surface == Some(object) => {
            Some(Event::Geometry {
                x: word(body, 0)? as i32,
                y: word(body, 4)? as i32,
                width: word(body, 8)? as i32,
                height: word(body, 12)? as i32,
            })
        }
        _ => None,
    }
}

/// Splits `buf` into complete messages and calls `note` on each; returns the byte count of the complete prefix.
fn scan(buf: &[u8], mut note: impl FnMut(u32, u16, &[u8])) -> usize {
    let mut at = 0;
    while let Some((object, opcode, size)) = message(&buf[at..]) {
        note(object, opcode, &buf[at + HEADER_BYTES..at + size]);
        at += size;
    }
    at
}

/// Reads once: the bytes and the fds that came with them. Empty bytes mean the peer closed.
fn receive(from: BorrowedFd<'_>) -> Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut data = vec![0u8; READ_BYTES];
    let mut space = [MaybeUninit::uninit(); FD_CONTROL_BYTES];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = recvmsg(
        from,
        &mut [IoSliceMut::new(&mut data)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    data.truncate(got.bytes);
    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            fds.extend(received);
        }
    }
    Ok((data, fds))
}

/// Sends `bytes`, the first `sendmsg` carrying up to `MAX_FDS_PER_SEND` of `fds` (removed from the queue).
pub(crate) fn forward(to: BorrowedFd<'_>, bytes: &[u8], fds: &mut Vec<OwnedFd>) -> Result<()> {
    let mut sent = 0;
    while sent < bytes.len() || !fds.is_empty() {
        let take = fds.len().min(MAX_FDS_PER_SEND);
        let batch: Vec<OwnedFd> = fds.drain(..take).collect();
        let borrowed: Vec<BorrowedFd<'_>> = batch.iter().map(AsFd::as_fd).collect();
        let mut space = [MaybeUninit::uninit(); FD_CONTROL_BYTES];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !borrowed.is_empty() {
            control.push(SendAncillaryMessage::ScmRights(&borrowed));
        }
        let n = sendmsg(
            to,
            &[IoSlice::new(&bytes[sent..])],
            &mut control,
            SendFlags::NOSIGNAL,
        )?;
        sent += n;
    }
    Ok(())
}

/// A pointer action the proxy injects; each is followed by its `frame`. Coordinates are surface-local, logical; `button` is a linux input code (BTN_LEFT = 0x110).
/// Scrolls: `Wheel` turns `notches` vertical detents (positive = down) from a wheel source; `Finger` is one touchpad step of (`dx`, `dy`) surface pixels, `FingerStop` lifts the fingers (`axis_stop`).
/// Scroll actions need `wl_seat` v5 (axis_source); a client bound lower gets none (dropped, reported on stderr).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerAction {
    Enter { x: f64, y: f64 },
    Leave,
    Motion { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    Wheel { notches: i32 },
    Finger { dx: f64, dy: f64 },
    FingerStop,
}

/// A touch action the proxy injects on its `wl_touch`, each followed by `frame`. `id` is the wl_touch contact id; coordinates surface-local, logical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchAction {
    Down { id: i32, x: f64, y: f64 },
    Motion { id: i32, x: f64, y: f64 },
    Up { id: i32 },
}

/// What the proxy writes to the client between two forwarded messages.
#[derive(Debug, Clone, PartialEq)]
enum Injection {
    Ping(u32),
    Touch {
        action: TouchAction,
        time_ms: u32,
    },
    Pointer {
        action: PointerAction,
        time_ms: u32,
    },
    Key {
        code: u32,
        pressed: bool,
        time_ms: u32,
    },
    Configure {
        width: i32,
        height: i32,
        serial: u32,
        states: Vec<u32>,
    },
}

/// The proxy: accepts clients on `listen`, relays them to `upstream`, reports [`Event`]s.
pub struct Proxy {
    pub listen: PathBuf,
    pub events: Receiver<(Instant, Event)>,
    injections: Arc<Mutex<Vec<Injection>>>,
    wakes: Arc<Mutex<Vec<OwnedFd>>>,
}

/// An injected input event's timestamp: CLOCK_MONOTONIC in ms, truncated to u32 -- the base wlroots and weston stamp input with. The protocol leaves the base undefined, but clients that turn it into Android event times read it as uptimeMillis (CLOCK_MONOTONIC), so a private epoch dates every event hours in the past.
fn input_time_ms() -> u32 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let ms = t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000;
    ms as u32
}

impl Proxy {
    /// Starts accepting in the background. The threads end with the process; `listen` is removed on drop.
    pub fn start(upstream: &Path, listen: &Path) -> Result<Proxy> {
        let _ = std::fs::remove_file(listen);
        let listener =
            UnixListener::bind(listen).with_context(|| format!("binding {}", listen.display()))?;
        let (tx, events) = channel();
        let injections = Arc::new(Mutex::new(Vec::new()));
        let wakes = Arc::new(Mutex::new(Vec::new()));
        let (up, inj, wk) = (
            upstream.to_owned(),
            Arc::clone(&injections),
            Arc::clone(&wakes),
        );
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                match serve(client, &up, &tx, &inj, &wk) {
                    Ok(()) => {}
                    Err(e) => eprintln!("wayland proxy: connection setup failed: {e:#}"),
                }
            }
        });
        Ok(Proxy {
            listen: listen.to_owned(),
            events,
            injections,
            wakes,
        })
    }

    /// Queues a synthetic `xdg_wm_base.ping(serial)` for the connection that bound `xdg_wm_base`; it reaches the client between two forwarded messages.
    pub fn inject_ping(&self, serial: u32) -> Result<()> {
        self.queue(Injection::Ping(serial))
    }

    /// Queues a pointer action on the connection's `wl_pointer`, at its window surface; the client gets it as compositor input. Nothing is written until the client has both (`xdg_surface` and `get_pointer`), so inject after [`Event::Acked`].
    pub fn inject_pointer(&self, action: PointerAction) -> Result<()> {
        self.queue(Injection::Pointer {
            action,
            time_ms: input_time_ms(),
        })
    }

    /// Queues a touch action on the client's `wl_touch`, at its window surface. The compositor's seat need have no touch: the proxy adds the capability, answers `get_touch` itself and is the only source of its events (module doc). Nothing is written before the client has the touch and its window.
    pub fn inject_touch(&self, action: TouchAction) -> Result<()> {
        self.queue(Injection::Touch {
            action,
            time_ms: input_time_ms(),
        })
    }

    /// Queues `wl_keyboard.key` of linux input `code` (KEY_A = 30) on the client's keyboard, which must hold focus (the window active: the compositor sent `enter`). Only the key: no `modifiers` event, so the client's xkb state does not see it as a modifier (the Android side derives meta from key transitions). The client's own repeat (`repeat_info`) runs while a key is held.
    pub fn inject_key(&self, code: u32, pressed: bool) -> Result<()> {
        self.queue(Injection::Key {
            code,
            pressed,
            time_ms: input_time_ms(),
        })
    }

    /// Queues an xdg-shell configure of the window: `xdg_toplevel.configure(width, height, states)`, then `xdg_surface.configure(serial)`, the order a compositor sends them.
    /// `states` join `activated` in the array; a configure is the window's whole state, so one without `states` clears the others (`[]` = focused, no other state). Values: [`XDG_STATE_MAXIMIZED`], [`XDG_STATE_SUSPENDED`].
    /// The client's `ack_configure(serial)` comes back as [`Event::Acked`] but is not forwarded: the compositor never sent that serial, and it would drop the client for it (`client_to_server`). Inject after the first [`Event::Acked`], as [`Proxy::inject_pointer`]: nothing is written before the window exists.
    pub fn inject_configure(
        &self,
        width: i32,
        height: i32,
        serial: u32,
        states: &[u32],
    ) -> Result<()> {
        self.queue(Injection::Configure {
            width,
            height,
            serial,
            states: states.to_vec(),
        })
    }

    fn queue(&self, item: Injection) -> Result<()> {
        self.injections
            .lock()
            .map_err(|_| anyhow::anyhow!("injection queue poisoned"))?
            .push(item);
        for wake in self
            .wakes
            .lock()
            .map_err(|_| anyhow::anyhow!("wake list poisoned"))?
            .iter()
        {
            write(wake, &1u64.to_ne_bytes())?;
        }
        Ok(())
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.listen);
    }
}

type Shared<T> = Arc<Mutex<T>>;

fn serve(
    client: UnixStream,
    upstream: &Path,
    events: &Sender<(Instant, Event)>,
    injections: &Shared<Vec<Injection>>,
    wakes: &Shared<Vec<OwnedFd>>,
) -> Result<()> {
    let server = UnixStream::connect(upstream)
        .with_context(|| format!("connecting to {}", upstream.display()))?;
    let ids: Shared<Ids> = Arc::default();
    let wake = eventfd(0, EventfdFlags::CLOEXEC)?;
    wakes
        .lock()
        .map_err(|_| anyhow::anyhow!("wake list poisoned"))?
        .push(wake.try_clone()?);

    let (client_r, server_w) = (client.try_clone()?, server.try_clone()?);
    let (ids_c, events_c) = (Arc::clone(&ids), events.clone());
    thread::spawn(move || {
        let _ = client_to_server(&client_r, &server_w, &ids_c, &events_c);
        let _ = server_w.shutdown(std::net::Shutdown::Both);
        let _ = events_c.send((Instant::now(), Event::Closed));
    });
    let (events_s, injections) = (events.clone(), Arc::clone(injections));
    thread::spawn(move || {
        let _ = server_to_client(&server, &client, &wake, &ids, &events_s, &injections);
        let _ = client.shutdown(std::net::Shutdown::Both);
    });
    Ok(())
}

fn client_to_server(
    client: &UnixStream,
    server: &UnixStream,
    ids: &Shared<Ids>,
    events: &Sender<(Instant, Event)>,
) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    loop {
        let (data, received) = receive(client.as_fd())?;
        if data.is_empty() && received.is_empty() {
            return Ok(());
        }
        pending.extend_from_slice(&data);
        fds.extend(received);
        // an ack of an injected configure is absorbed: the compositor never sent that serial, so forwarding it is a protocol error that drops the client
        let (complete, forwarded) = {
            let mut ids = ids.lock().map_err(|_| anyhow::anyhow!("ids poisoned"))?;
            let mut forwarded = Vec::new();
            let mut at = 0;
            while let Some((object, opcode, size)) = message(&pending[at..]) {
                let raw = &pending[at..at + size];
                // the touch is the proxy's: its requests (release) never reach the compositor
                let on_touch = ids.touch == Some(object);
                let event = note_request(&mut ids, object, opcode, &raw[HEADER_BYTES..]);
                let absorbed = on_touch
                    || match &event {
                        Some(Event::Acked { serial }) => ids.injected.contains(serial),
                        _ => false,
                    };
                if event == Some(Event::TouchCreated) {
                    // the compositor's seat has no touch and refuses get_touch (wlroots: protocol error), but the client's new id must still be taken, in order:
                    // a wl_display.sync takes it, and its callback's done and delete_id are dropped on the way back (server_to_client)
                    forwarded.extend(self::event(
                        WL_DISPLAY_ID,
                        WL_DISPLAY_SYNC,
                        &raw[HEADER_BYTES..HEADER_BYTES + 4],
                    ));
                } else if !absorbed {
                    forwarded.extend_from_slice(raw);
                }
                if let Some(event) = event {
                    let _ = events.send((Instant::now(), event));
                }
                at += size;
            }
            (at, forwarded)
        };
        if !forwarded.is_empty() {
            forward(server.as_fd(), &forwarded, &mut fds)?;
        }
        pending.drain(..complete);
    }
}

fn server_to_client(
    server: &UnixStream,
    client: &UnixStream,
    wake: &OwnedFd,
    ids: &Shared<Ids>,
    events: &Sender<(Instant, Event)>,
    injections: &Shared<Vec<Injection>>,
) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    loop {
        let mut watch = [
            PollFd::new(server, PollFlags::IN),
            PollFd::new(wake, PollFlags::IN),
        ];
        poll(&mut watch, None)?;
        let (server_ready, woken) = (
            !watch[0].revents().is_empty(),
            !watch[1].revents().is_empty(),
        );
        if server_ready {
            let (data, received) = receive(server.as_fd())?;
            if data.is_empty() && received.is_empty() {
                return Ok(());
            }
            pending.extend_from_slice(&data);
            fds.extend(received);
            let (wm_base, seats, touch) = {
                let ids = ids.lock().map_err(|_| anyhow::anyhow!("ids poisoned"))?;
                let seats: Vec<u32> = ids
                    .interfaces
                    .iter()
                    .filter_map(|(id, i)| (i == WL_SEAT).then_some(*id))
                    .collect();
                (ids.id_of(WM_BASE), seats, ids.touch)
            };
            // the seat claims touch, so the client asks for the proxy's wl_touch; the sync that took that id answers on it and deletes it: both are the proxy's to drop
            let mut out = Vec::with_capacity(pending.len());
            let complete = scan(&pending, |object, opcode, body| {
                let id_of_touch = |v: Option<u32>| v.is_some() && v == touch;
                if id_of_touch(Some(object))
                    || (object == WL_DISPLAY_ID
                        && opcode == WL_DISPLAY_DELETE_ID
                        && id_of_touch(word(body, 0)))
                {
                    return;
                }
                if seats.contains(&object)
                    && opcode == WL_SEAT_CAPABILITIES
                    && let Some(caps) = word(body, 0)
                {
                    out.extend(self::event(
                        object,
                        opcode,
                        &(caps | SEAT_CAP_TOUCH).to_ne_bytes(),
                    ));
                    return;
                }
                out.extend(self::event(object, opcode, body));
                if Some(object) == wm_base
                    && opcode == XDG_WM_BASE_PING
                    && let Some(serial) = word(body, 0)
                {
                    let _ = events.send((
                        Instant::now(),
                        Event::Ping {
                            serial,
                            injected: false,
                        },
                    ));
                }
            });
            if complete > 0 {
                forward(client.as_fd(), &out, &mut fds)?;
                pending.drain(..complete);
            }
        }
        if woken {
            let mut counter = [0u8; 8];
            let _ = read(wake, &mut counter);
        }
        // only a connection with windows takes injections: an app may hold others (atl-platform-ui-linux's own wl_output connection, outputs.rs), and one of those taking the queue would drop it
        if ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ids poisoned"))?
            .id_of(WM_BASE)
            .is_none()
        {
            continue;
        }
        // injections are written between whole forwarded messages: `pending` holds only a partial message here, which is not forwarded yet
        let items = std::mem::take(
            &mut *injections
                .lock()
                .map_err(|_| anyhow::anyhow!("injection queue poisoned"))?,
        );
        if items.is_empty() {
            continue;
        }
        let (wm_base, pointer, keyboard, touch, surface, toplevel, xdg_surface, seat_version) = {
            let ids = ids.lock().map_err(|_| anyhow::anyhow!("ids poisoned"))?;
            (
                ids.id_of(WM_BASE),
                ids.pointer,
                ids.keyboard,
                ids.touch,
                ids.surface,
                ids.toplevel,
                ids.xdg_surface,
                ids.seat_version.unwrap_or(1),
            )
        };
        for item in items {
            let bytes = match item {
                Injection::Ping(serial) => match wm_base {
                    Some(wm_base) => {
                        let _ = events.send((
                            Instant::now(),
                            Event::Ping {
                                serial,
                                injected: true,
                            },
                        ));
                        ping_bytes(wm_base, serial)
                    }
                    None => {
                        eprintln!(
                            "wayland proxy: ping dropped, the client has not bound xdg_wm_base"
                        );
                        continue;
                    }
                },
                Injection::Pointer { action, time_ms } => match (pointer, surface) {
                    (Some(pointer), Some(surface)) => {
                        match pointer_bytes(pointer, surface, seat_version, action, time_ms) {
                            Some(bytes) => {
                                let _ = events.send((Instant::now(), Event::PointerInjected));
                                bytes
                            }
                            None => {
                                eprintln!(
                                    "wayland proxy: {action:?} dropped, the client bound wl_seat v{seat_version}"
                                );
                                continue;
                            }
                        }
                    }
                    _ => {
                        eprintln!(
                            "wayland proxy: {action:?} dropped, the client has no pointer and window surface yet"
                        );
                        continue;
                    }
                },
                Injection::Touch { action, time_ms } => match (touch, surface) {
                    (Some(touch), Some(surface)) => {
                        let _ = events.send((Instant::now(), Event::TouchInjected));
                        touch_bytes(touch, surface, action, time_ms)
                    }
                    _ => {
                        eprintln!(
                            "wayland proxy: {action:?} dropped, the client has no touch and window surface yet"
                        );
                        continue;
                    }
                },
                Injection::Key {
                    code,
                    pressed,
                    time_ms,
                } => match keyboard {
                    // serial = timestamp, as for the pointer; state 1 pressed, 0 released
                    Some(keyboard) => event(
                        keyboard,
                        WL_KEYBOARD_KEY,
                        &[
                            time_ms.to_ne_bytes(),
                            time_ms.to_ne_bytes(),
                            code.to_ne_bytes(),
                            u32::from(pressed).to_ne_bytes(),
                        ]
                        .concat(),
                    ),
                    None => {
                        eprintln!("wayland proxy: key {code} dropped, the client has no keyboard");
                        continue;
                    }
                },
                Injection::Configure {
                    width,
                    height,
                    serial,
                    states,
                } => match (toplevel, xdg_surface) {
                    (Some(toplevel), Some(xdg_surface)) => {
                        // recorded before the bytes go out: the client's ack cannot come before this
                        ids.lock()
                            .map_err(|_| anyhow::anyhow!("ids poisoned"))?
                            .injected
                            .push(serial);
                        let _ = events.send((Instant::now(), Event::ConfigureInjected { serial }));
                        configure_bytes(toplevel, xdg_surface, width, height, serial, &states)
                    }
                    _ => {
                        eprintln!(
                            "wayland proxy: configure {width}x{height} dropped, the client has no window role yet"
                        );
                        continue;
                    }
                },
            };
            forward(client.as_fd(), &bytes, &mut Vec::new())?;
        }
    }
}

/// One event as a client receives it: object, `size << 16 | opcode`, the body (native endian).
fn event(object: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
    let size = (HEADER_BYTES + body.len()) as u32;
    let mut out = Vec::with_capacity(HEADER_BYTES + body.len());
    out.extend_from_slice(&object.to_ne_bytes());
    out.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
    out.extend_from_slice(body);
    out
}

/// `xdg_wm_base.ping(serial)` as a client receives it: object, `12 << 16 | 0`, serial.
fn ping_bytes(wm_base: u32, serial: u32) -> Vec<u8> {
    event(wm_base, XDG_WM_BASE_PING, &serial.to_ne_bytes())
}

/// `xdg_toplevel.configure(width, height, states)` then `xdg_surface.configure(serial)` as a client receives them. The states array is `activated`, then `states`: a u32 byte length, then one u32 per entry.
fn configure_bytes(
    toplevel: u32,
    xdg_surface: u32,
    width: i32,
    height: i32,
    serial: u32,
    states: &[u32],
) -> Vec<u8> {
    let entries: Vec<u32> = std::iter::once(XDG_STATE_ACTIVATED)
        .chain(states.iter().copied())
        .collect();
    let states_bytes = (entries.len() * 4) as u32;
    let mut body = [
        width.to_ne_bytes(),
        height.to_ne_bytes(),
        states_bytes.to_ne_bytes(),
    ]
    .concat();
    for entry in entries {
        body.extend(entry.to_ne_bytes());
    }
    let mut out = event(toplevel, XDG_TOPLEVEL_CONFIGURE, &body);
    out.extend(event(
        xdg_surface,
        XDG_SURFACE_CONFIGURE,
        &serial.to_ne_bytes(),
    ));
    out
}

/// A touch action's `wl_touch` events, then `frame`. The serial of down and up is the timestamp, as for the pointer.
fn touch_bytes(touch: u32, surface: u32, action: TouchAction, time_ms: u32) -> Vec<u8> {
    let t = time_ms.to_ne_bytes();
    let ev = |opcode: u16, args: &[[u8; 4]]| event(touch, opcode, &args.concat());
    let mut out = match action {
        TouchAction::Down { id, x, y } => ev(
            WL_TOUCH_DOWN,
            &[
                t,
                t,
                surface.to_ne_bytes(),
                id.to_ne_bytes(),
                fixed(x),
                fixed(y),
            ],
        ),
        TouchAction::Motion { id, x, y } => {
            ev(WL_TOUCH_MOTION, &[t, id.to_ne_bytes(), fixed(x), fixed(y)])
        }
        TouchAction::Up { id } => ev(WL_TOUCH_UP, &[t, t, id.to_ne_bytes()]),
    };
    out.extend(event(touch, WL_TOUCH_FRAME, &[]));
    out
}

/// A surface coordinate as wl_fixed (24.8).
fn fixed(v: f64) -> [u8; 4] {
    ((v * FIXED_ONE).round() as i32).to_ne_bytes()
}

/// A pointer action's `wl_pointer` events as a client bound at `version` receives them: the action's events, then `frame` (v5+), which GDK flushes the group on. The serial of enter, leave and button is the timestamp.
/// None for a scroll the version cannot carry (axis_source needs v5). A wheel notch carries `axis_value120` from v8, `axis_discrete` before.
fn pointer_bytes(
    pointer: u32,
    surface: u32,
    version: u32,
    action: PointerAction,
    time_ms: u32,
) -> Option<Vec<u8>> {
    let t = time_ms.to_ne_bytes();
    let u = |v: u32| v.to_ne_bytes();
    let ev = |opcode: u16, args: &[[u8; 4]]| event(pointer, opcode, &args.concat());
    let scroll = matches!(
        action,
        PointerAction::Wheel { .. } | PointerAction::Finger { .. } | PointerAction::FingerStop
    );
    if scroll && version < POINTER_V_FRAME {
        return None;
    }
    let mut out = match action {
        PointerAction::Enter { x, y } => ev(WL_POINTER_ENTER, &[t, u(surface), fixed(x), fixed(y)]),
        PointerAction::Leave => ev(WL_POINTER_LEAVE, &[t, u(surface)]),
        PointerAction::Motion { x, y } => ev(WL_POINTER_MOTION, &[t, fixed(x), fixed(y)]),
        PointerAction::Button { button, pressed } => {
            ev(WL_POINTER_BUTTON, &[t, t, u(button), u(u32::from(pressed))])
        }
        PointerAction::Wheel { notches } => {
            let mut out = ev(WL_POINTER_AXIS_SOURCE, &[u(AXIS_SOURCE_WHEEL)]);
            out.extend(if version >= POINTER_V_VALUE120 {
                let value120 = (f64::from(notches) * VALUE120_PER_NOTCH) as i32;
                ev(
                    WL_POINTER_AXIS_VALUE120,
                    &[u(AXIS_VERTICAL), value120.to_ne_bytes()],
                )
            } else {
                ev(
                    WL_POINTER_AXIS_DISCRETE,
                    &[u(AXIS_VERTICAL), notches.to_ne_bytes()],
                )
            });
            out.extend(ev(
                WL_POINTER_AXIS,
                &[
                    t,
                    u(AXIS_VERTICAL),
                    fixed(f64::from(notches) * WHEEL_NOTCH_VALUE),
                ],
            ));
            out
        }
        PointerAction::Finger { dx, dy } => {
            let mut out = ev(WL_POINTER_AXIS_SOURCE, &[u(AXIS_SOURCE_FINGER)]);
            out.extend(ev(WL_POINTER_AXIS, &[t, u(AXIS_VERTICAL), fixed(dy)]));
            out.extend(ev(WL_POINTER_AXIS, &[t, u(AXIS_HORIZONTAL), fixed(dx)]));
            out
        }
        PointerAction::FingerStop => {
            let mut out = ev(WL_POINTER_AXIS_SOURCE, &[u(AXIS_SOURCE_FINGER)]);
            out.extend(ev(WL_POINTER_AXIS_STOP, &[t, u(AXIS_VERTICAL)]));
            out.extend(ev(WL_POINTER_AXIS_STOP, &[t, u(AXIS_HORIZONTAL)]));
            out
        }
    };
    if version >= POINTER_V_FRAME {
        out.extend(event(pointer, WL_POINTER_FRAME, &[]));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(object: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
        let size = (HEADER_BYTES + body.len()) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&object.to_ne_bytes());
        v.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        v.extend_from_slice(body);
        v
    }

    fn bind(registry: u32, name: u32, interface: &str, version: u32, new_id: u32) -> Vec<u8> {
        let mut text = interface.as_bytes().to_vec();
        text.push(0);
        let len = text.len() as u32;
        while !text.len().is_multiple_of(4) {
            text.push(0);
        }
        let mut body = name.to_ne_bytes().to_vec();
        body.extend_from_slice(&len.to_ne_bytes());
        body.extend_from_slice(&text);
        body.extend_from_slice(&version.to_ne_bytes());
        body.extend_from_slice(&new_id.to_ne_bytes());
        msg(registry, WL_REGISTRY_BIND, &body)
    }

    /// The wire layout is libwayland's (wayland-util: header words, strings with NUL padded to 4): the bytes here are written out by hand from that, not by this module's encoder. A pong for the bound id decodes, an other id's does not.
    #[test]
    fn tracks_binds_and_pongs() {
        let mut stream = msg(1, WL_DISPLAY_GET_REGISTRY, &2u32.to_ne_bytes());
        stream.extend(bind(2, 14, "xdg_wm_base", 1, 7));
        stream.extend(bind(2, 10, "wl_shm", 1, 8));
        stream.extend(msg(7, XDG_WM_BASE_PONG, &0xA7C0_0001u32.to_ne_bytes()));
        stream.extend(msg(9, XDG_WM_BASE_PONG, &5u32.to_ne_bytes()));
        stream.extend(msg(8, WL_SHM_CREATE_POOL, &[0; 8]));
        let mut ids = Ids::default();
        let mut seen = Vec::new();
        let used = scan(&stream, |o, op, b| {
            seen.extend(note_request(&mut ids, o, op, b));
        });
        assert_eq!(used, stream.len());
        assert_eq!(
            seen,
            [
                Event::Bound("xdg_wm_base".into()),
                Event::Bound("wl_shm".into()),
                Event::Pong {
                    serial: 0xA7C0_0001
                },
                Event::ShmPool,
            ]
        );
        assert_eq!(ids.id_of(WM_BASE), Some(7));
    }

    /// `xdg_toplevel.set_app_id` is request 3; its string is written out by hand (length 26 counts the NUL, 25 letters, 2 pad bytes to 28). Only the window's toplevel reports it.
    #[test]
    fn set_app_id_is_an_event() {
        let mut ids = Ids::default();
        ids.interfaces.insert(12, XDG_TOPLEVEL.to_owned());
        ids.toplevel = Some(12);
        let mut body = 26u32.to_ne_bytes().to_vec();
        body.extend_from_slice(b"atl.com.example.atlwindow\0\0\0");
        assert_eq!(
            note_request(&mut ids, 12, XDG_TOPLEVEL_SET_APP_ID, &body),
            Some(Event::AppId {
                app_id: "atl.com.example.atlwindow".into(),
            })
        );
        assert_eq!(
            note_request(&mut ids, 13, XDG_TOPLEVEL_SET_APP_ID, &body),
            None
        );
    }

    /// `xdg_toplevel.set_minimized` (request 13, no arguments) of the window's toplevel is [`Event::Minimized`]; another toplevel's is not.
    #[test]
    fn set_minimized_is_an_event() {
        let mut ids = Ids::default();
        ids.interfaces.insert(12, XDG_TOPLEVEL.to_owned());
        ids.interfaces.insert(13, XDG_TOPLEVEL.to_owned());
        ids.toplevel = Some(12);
        assert_eq!(
            note_request(&mut ids, 12, XDG_TOPLEVEL_SET_MINIMIZED, &[]),
            Some(Event::Minimized)
        );
        assert_eq!(
            note_request(&mut ids, 13, XDG_TOPLEVEL_SET_MINIMIZED, &[]),
            None
        );
    }

    /// The window's first `commit` is [`Event::FirstCommit`], bufferless or not; later commits and another surface's do not report it.
    #[test]
    fn first_commit_is_the_windows_first_commit() {
        let mut ids = Ids {
            surface: Some(5),
            ..Ids::default()
        };
        assert_eq!(
            note_request(&mut ids, 6, WL_SURFACE_COMMIT, &[]),
            None,
            "another surface's commit is not the window's"
        );
        assert_eq!(
            note_request(&mut ids, 5, WL_SURFACE_COMMIT, &[]),
            Some(Event::FirstCommit)
        );
        assert_eq!(note_request(&mut ids, 5, WL_SURFACE_COMMIT, &[]), None);
    }

    /// A message cut off mid-body is not consumed: the prefix ends at the last complete message.
    #[test]
    fn partial_message_is_left_in_the_buffer() {
        let whole = msg(3, 0, &[1, 2, 3, 4]);
        let mut stream = whole.clone();
        stream.extend_from_slice(&whole[..11]);
        assert_eq!(scan(&stream, |_, _, _| {}), whole.len());
    }

    /// `xdg_wm_base.ping` is event 0 with one uint: 12 bytes, size 12 in the high half of the second word.
    /// wl_pointer.motion(time, x, y) with wl_fixed 24.8 (2.5 = 0x280, -1.0 = -256), then frame: written out from the wire format, not by the encoder.
    #[test]
    fn pointer_motion_follows_the_wire_format() {
        let body = [
            5u32.to_ne_bytes(),
            0x280u32.to_ne_bytes(),
            (-256i32).to_ne_bytes(),
        ]
        .concat();
        let mut want = msg(20, WL_POINTER_MOTION, &body);
        want.extend(msg(20, WL_POINTER_FRAME, &[]));
        assert_eq!(
            pointer_bytes(20, 30, 5, PointerAction::Motion { x: 2.5, y: -1.0 }, 5),
            Some(want)
        );
        // v4 has no frame event
        assert_eq!(
            pointer_bytes(20, 30, 4, PointerAction::Motion { x: 2.5, y: -1.0 }, 5),
            Some(msg(20, WL_POINTER_MOTION, &body))
        );
    }

    /// Two notches down: axis_source wheel (6: 0), then axis_value120 (9: axis 0, 240) at v8 or axis_discrete (8: axis 0, 2) at v5, then axis (4: time, axis 0, 30.0 = 0x1e00 in 24.8), then frame (5). Below v5 nothing.
    #[test]
    fn wheel_follows_the_seat_version() {
        let tail = |mut v: Vec<u8>| {
            let axis = [
                5u32.to_ne_bytes(),
                0u32.to_ne_bytes(),
                0x1e00u32.to_ne_bytes(),
            ]
            .concat();
            v.extend(msg(20, 4, &axis));
            v.extend(msg(20, 5, &[]));
            v
        };
        let source = msg(20, 6, &0u32.to_ne_bytes());
        let mut v8 = source.clone();
        v8.extend(msg(
            20,
            9,
            &[0u32.to_ne_bytes(), 240i32.to_ne_bytes()].concat(),
        ));
        let mut v5 = source;
        v5.extend(msg(
            20,
            8,
            &[0u32.to_ne_bytes(), 2i32.to_ne_bytes()].concat(),
        ));
        let wheel = PointerAction::Wheel { notches: 2 };
        assert_eq!(pointer_bytes(20, 30, 8, wheel, 5), Some(tail(v8)));
        assert_eq!(pointer_bytes(20, 30, 5, wheel, 5), Some(tail(v5)));
        assert_eq!(pointer_bytes(20, 30, 4, wheel, 5), None);
        assert_eq!(pointer_bytes(20, 30, 4, PointerAction::FingerStop, 5), None);
    }

    /// wl_touch.down is serial, time, surface, id, x, y (wayland.xml): 8 + 24 bytes, then a bare frame (opcode 3, 8 bytes); x 10.5 is 2688 in 24.8.
    #[test]
    fn touch_down_follows_the_wire_format() {
        let b = touch_bytes(
            9,
            5,
            TouchAction::Down {
                id: 1,
                x: 10.5,
                y: 2.0,
            },
            7,
        );
        assert_eq!(b.len(), 40);
        assert_eq!(message(&b), Some((9, WL_TOUCH_DOWN, 32)));
        assert_eq!(
            (word(&b, 16), word(&b, 20), word(&b, 24)),
            (Some(5), Some(1), Some(2688))
        );
        assert_eq!(message(&b[32..]), Some((9, WL_TOUCH_FRAME, 8)));
    }

    #[test]
    fn ping_is_twelve_bytes() {
        let ping = ping_bytes(7, 0xA7C0_0002);
        assert_eq!(ping.len(), 12);
        assert_eq!(message(&ping), Some((7, 0, 12)));
        assert_eq!(word(&ping, 8), Some(0xA7C0_0002));
    }

    /// Written out from xdg-shell's XML (`xdg_toplevel.configure(int width, int height, array states)`, `xdg_surface.configure(uint serial)`): the expected bytes are literals, not this module's encoder.
    /// The toplevel's message is 24 bytes (header, 1600, 1000, a 4-byte array holding `activated` = 4); the surface's is 12 bytes (header, serial 5).
    #[test]
    fn configure_is_the_toplevel_then_the_surface() {
        let bytes = configure_bytes(7, 8, 1600, 1000, 5, &[]);
        let parts: [&[u8]; 5] = [
            &[7, 0, 0, 0, 0x00, 0x00, 0x18, 0x00],
            &[0x40, 0x06, 0, 0, 0xE8, 0x03, 0, 0],
            &[4, 0, 0, 0, 4, 0, 0, 0],
            &[8, 0, 0, 0, 0x00, 0x00, 0x0C, 0x00],
            &[5, 0, 0, 0],
        ];
        assert_eq!(bytes, parts.concat());
        assert_eq!(message(&bytes), Some((7, 0, 24)));
        assert_eq!(message(&bytes[24..]), Some((8, 0, 12)));
    }

    /// The states array is `activated` (4), then maximized (1) and suspended (9): 12 bytes, so the toplevel's message is 32 bytes (header, 1600, 1000, the length, three entries).
    #[test]
    fn configure_carries_the_states_beside_activated() {
        let states = [XDG_STATE_MAXIMIZED, XDG_STATE_SUSPENDED];
        let bytes = configure_bytes(7, 8, 1600, 1000, 5, &states);
        let parts: [&[u8]; 6] = [
            &[7, 0, 0, 0, 0x00, 0x00, 0x20, 0x00],
            &[0x40, 0x06, 0, 0, 0xE8, 0x03, 0, 0],
            &[12, 0, 0, 0],
            &[4, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0],
            &[8, 0, 0, 0, 0x00, 0x00, 0x0C, 0x00],
            &[5, 0, 0, 0],
        ];
        assert_eq!(bytes, parts.concat());
        assert_eq!(message(&bytes), Some((7, 0, 32)));
        assert_eq!(message(&bytes[32..]), Some((8, 0, 12)));
    }
}
