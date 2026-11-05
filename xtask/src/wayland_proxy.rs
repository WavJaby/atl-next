// SPDX-License-Identifier: MIT
//! A transparent Wayland proxy for `xtask test platform-ui`'s ping probe.
//!
//! Clients connect to the proxy's socket; every byte and every passed fd is forwarded unchanged to the real compositor and back. On the way it decodes the few messages it needs, whole messages only (8-byte header: object id, `size << 16 | opcode`, native endian):
//! * `wl_display.get_registry` and `wl_registry.bind` -> which object id is which interface (`xdg_wm_base`, `wl_shm`, ...);
//! * `xdg_wm_base.pong(serial)` (request opcode 3) -> [`Event::Pong`], `wl_shm.create_pool` (opcode 0) -> [`Event::ShmPool`];
//! * `xdg_wm_base.ping(serial)` from the compositor (event opcode 0) -> [`Event::Ping`] with `injected: false`.
//!
//! [`Proxy::inject_ping`] writes a synthetic `xdg_wm_base.ping(serial)` to the client between two forwarded messages, which is what a compositor's ping is on the wire. A client whose main loop is blocked reads nothing, so no `pong` comes back; a client iterating answers at once.
//! [`Proxy::inject_pointer`] writes `wl_pointer` events (enter, leave, motion, button, wheel and finger scrolls, each then `frame`; scrolls honour the bound `wl_seat` version) on the pointer `wl_seat.get_pointer` created, at the surface of the first `xdg_surface` (the window); the client sees them as the compositor's pointer input.
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
const XDG_TOPLEVEL: &str = "xdg_toplevel";
/// The `configure` events of xdg_toplevel and xdg_surface are both opcode 0 (their requests are other opcodes).
const XDG_TOPLEVEL_CONFIGURE: u16 = 0;
const XDG_SURFACE_CONFIGURE: u16 = 0;
/// xdg-shell's `activated` window state: the injected configure says the window has focus, as the compositor's do for the focused window.
const XDG_STATE_ACTIVATED: u32 = 4;
const WL_SEAT: &str = "wl_seat";
const WL_SEAT_GET_POINTER: u16 = 0;
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
    /// [`Proxy::inject_pointer`]'s action was written to the client (a dropped action reports nothing).
    PointerInjected,
    /// [`Proxy::inject_configure`]'s configure was written to the client; its `ack_configure` with the same serial comes back as [`Event::Acked`].
    ConfigureInjected { serial: u32 },
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
    /// The version the client bound `wl_seat` at, which its `wl_pointer` inherits: events newer than it must not be sent.
    seat_version: Option<u32>,
    /// The `wl_surface` of the first `xdg_surface`: the window.
    surface: Option<u32>,
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
    match (ids.interfaces.get(&object).map(String::as_str), opcode) {
        (Some(WM_BASE), XDG_WM_BASE_PONG) => Some(Event::Pong {
            serial: word(body, 0)?,
        }),
        (Some(SHM), WL_SHM_CREATE_POOL) => Some(Event::ShmPool),
        (Some(XDG_SURFACE), XDG_SURFACE_ACK_CONFIGURE) => Some(Event::Acked {
            serial: word(body, 0)?,
        }),
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
fn forward(to: BorrowedFd<'_>, bytes: &[u8], fds: &mut Vec<OwnedFd>) -> Result<()> {
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

/// What the proxy writes to the client between two forwarded messages.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Injection {
    Ping(u32),
    Pointer {
        action: PointerAction,
        time_ms: u32,
    },
    Configure {
        width: i32,
        height: i32,
        serial: u32,
    },
}

/// The proxy: accepts clients on `listen`, relays them to `upstream`, reports [`Event`]s.
pub struct Proxy {
    pub listen: PathBuf,
    pub events: Receiver<(Instant, Event)>,
    injections: Arc<Mutex<Vec<Injection>>>,
    wakes: Arc<Mutex<Vec<OwnedFd>>>,
    /// Origin of the event timestamps the injected pointer events carry.
    epoch: Instant,
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
            epoch: Instant::now(),
        })
    }

    /// Queues a synthetic `xdg_wm_base.ping(serial)` for the connection that bound `xdg_wm_base`; it reaches the client between two forwarded messages.
    pub fn inject_ping(&self, serial: u32) -> Result<()> {
        self.queue(Injection::Ping(serial))
    }

    /// Queues a pointer action on the connection's `wl_pointer`, at its window surface; the client gets it as compositor input. Nothing is written until the client has both (`xdg_surface` and `get_pointer`), so inject after [`Event::Acked`].
    pub fn inject_pointer(&self, action: PointerAction) -> Result<()> {
        let time_ms = u32::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.queue(Injection::Pointer { action, time_ms })
    }

    /// Queues an xdg-shell configure of the window: `xdg_toplevel.configure(width, height, activated)`, then `xdg_surface.configure(serial)`, the order a compositor sends them.
    /// The client's `ack_configure(serial)` comes back as [`Event::Acked`] but is not forwarded: the compositor never sent that serial, and it would drop the client for it (`client_to_server`). Inject after the first [`Event::Acked`], as [`Proxy::inject_pointer`]: nothing is written before the window exists.
    pub fn inject_configure(&self, width: i32, height: i32, serial: u32) -> Result<()> {
        self.queue(Injection::Configure {
            width,
            height,
            serial,
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
                let event = note_request(&mut ids, object, opcode, &raw[HEADER_BYTES..]);
                let absorbed = match &event {
                    Some(Event::Acked { serial }) => ids.injected.contains(serial),
                    _ => false,
                };
                if let Some(event) = event {
                    let _ = events.send((Instant::now(), event));
                }
                if !absorbed {
                    forwarded.extend_from_slice(raw);
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
            let wm_base = ids
                .lock()
                .map_err(|_| anyhow::anyhow!("ids poisoned"))?
                .id_of(WM_BASE);
            let complete = scan(&pending, |object, opcode, body| {
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
                forward(client.as_fd(), &pending[..complete], &mut fds)?;
                pending.drain(..complete);
            }
        }
        if woken {
            let mut counter = [0u8; 8];
            let _ = read(wake, &mut counter);
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
        let (wm_base, pointer, surface, toplevel, xdg_surface, seat_version) = {
            let ids = ids.lock().map_err(|_| anyhow::anyhow!("ids poisoned"))?;
            (
                ids.id_of(WM_BASE),
                ids.pointer,
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
                Injection::Configure {
                    width,
                    height,
                    serial,
                } => match (toplevel, xdg_surface) {
                    (Some(toplevel), Some(xdg_surface)) => {
                        // recorded before the bytes go out: the client's ack cannot come before this
                        ids.lock()
                            .map_err(|_| anyhow::anyhow!("ids poisoned"))?
                            .injected
                            .push(serial);
                        let _ = events.send((Instant::now(), Event::ConfigureInjected { serial }));
                        configure_bytes(toplevel, xdg_surface, width, height, serial)
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

/// `xdg_toplevel.configure(width, height, states)` then `xdg_surface.configure(serial)` as a client receives them. The states array is the one `activated` entry: a u32 byte length, then the entry.
fn configure_bytes(
    toplevel: u32,
    xdg_surface: u32,
    width: i32,
    height: i32,
    serial: u32,
) -> Vec<u8> {
    let states_bytes = 4u32;
    let body = [
        width.to_ne_bytes(),
        height.to_ne_bytes(),
        states_bytes.to_ne_bytes(),
        XDG_STATE_ACTIVATED.to_ne_bytes(),
    ]
    .concat();
    let mut out = event(toplevel, XDG_TOPLEVEL_CONFIGURE, &body);
    out.extend(event(
        xdg_surface,
        XDG_SURFACE_CONFIGURE,
        &serial.to_ne_bytes(),
    ));
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
        let bytes = configure_bytes(7, 8, 1600, 1000, 5);
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
}
