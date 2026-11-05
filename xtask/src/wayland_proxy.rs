// SPDX-License-Identifier: MIT
//! A transparent Wayland proxy for `xtask test platform-ui`'s ping probe.
//!
//! Clients connect to the proxy's socket; every byte and every passed fd is forwarded unchanged to the real compositor and back. On the way it decodes the few messages it needs, whole messages only (8-byte header: object id, `size << 16 | opcode`, native endian):
//! * `wl_display.get_registry` and `wl_registry.bind` -> which object id is which interface (`xdg_wm_base`, `wl_shm`, ...);
//! * `xdg_wm_base.pong(serial)` (request opcode 3) -> [`Event::Pong`], `wl_shm.create_pool` (opcode 0) -> [`Event::ShmPool`];
//! * `xdg_wm_base.ping(serial)` from the compositor (event opcode 0) -> [`Event::Ping`] with `injected: false`.
//!
//! [`Proxy::inject_ping`] writes a synthetic `xdg_wm_base.ping(serial)` to the client between two forwarded messages, which is what a compositor's ping is on the wire. A client whose main loop is blocked reads nothing, so no `pong` comes back; a client iterating answers at once.
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
    /// A client connection closed.
    Closed,
}

/// Object ids of one connection and the interface each was bound as.
#[derive(Default)]
struct Ids {
    registry: Option<u32>,
    interfaces: HashMap<u32, String>,
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
        let new_id = word(body, 8 + len.div_ceil(4) * 4 + 4)?;
        ids.interfaces.insert(new_id, name.clone());
        return Some(Event::Bound(name));
    }
    match (ids.interfaces.get(&object).map(String::as_str), opcode) {
        (Some(WM_BASE), XDG_WM_BASE_PONG) => Some(Event::Pong {
            serial: word(body, 0)?,
        }),
        (Some(SHM), WL_SHM_CREATE_POOL) => Some(Event::ShmPool),
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

/// The proxy: accepts clients on `listen`, relays them to `upstream`, reports [`Event`]s.
pub struct Proxy {
    pub listen: PathBuf,
    pub events: Receiver<(Instant, Event)>,
    injections: Arc<Mutex<Vec<u32>>>,
    wakes: Arc<Mutex<Vec<OwnedFd>>>,
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
        self.injections
            .lock()
            .map_err(|_| anyhow::anyhow!("injection queue poisoned"))?
            .push(serial);
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
    injections: &Shared<Vec<u32>>,
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
        let complete = {
            let mut ids = ids.lock().map_err(|_| anyhow::anyhow!("ids poisoned"))?;
            scan(&pending, |object, opcode, body| {
                if let Some(event) = note_request(&mut ids, object, opcode, body) {
                    let _ = events.send((Instant::now(), event));
                }
            })
        };
        if complete > 0 {
            forward(server.as_fd(), &pending[..complete], &mut fds)?;
            pending.drain(..complete);
        }
    }
}

fn server_to_client(
    server: &UnixStream,
    client: &UnixStream,
    wake: &OwnedFd,
    ids: &Shared<Ids>,
    events: &Sender<(Instant, Event)>,
    injections: &Shared<Vec<u32>>,
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
        let wm_base = ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ids poisoned"))?
            .id_of(WM_BASE);
        if let Some(wm_base) = wm_base {
            let serials: Vec<u32> = std::mem::take(
                &mut *injections
                    .lock()
                    .map_err(|_| anyhow::anyhow!("injection queue poisoned"))?,
            );
            for serial in serials {
                forward(
                    client.as_fd(),
                    &ping_bytes(wm_base, serial),
                    &mut Vec::new(),
                )?;
                let _ = events.send((
                    Instant::now(),
                    Event::Ping {
                        serial,
                        injected: true,
                    },
                ));
            }
        }
    }
}

/// `xdg_wm_base.ping(serial)` as a client receives it: object, `12 << 16 | 0`, serial.
fn ping_bytes(wm_base: u32, serial: u32) -> Vec<u8> {
    let header = ((HEADER_BYTES as u32 + 4) << 16) | u32::from(XDG_WM_BASE_PING);
    [wm_base, header, serial]
        .iter()
        .flat_map(|w| w.to_ne_bytes())
        .collect()
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
    #[test]
    fn ping_is_twelve_bytes() {
        let ping = ping_bytes(7, 0xA7C0_0002);
        assert_eq!(ping.len(), 12);
        assert_eq!(message(&ping), Some((7, 0, 12)));
        assert_eq!(word(&ping, 8), Some(0xA7C0_0002));
    }
}
