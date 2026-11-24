// SPDX-License-Identifier: MIT
#![forbid(unsafe_code)]
//! The test environment's Wayland wire client: requests written by hand ([`Client::send`]), events read back by roundtrip ([`Client::roundtrip`]), and [`forward`], the fd-carrying send both use.
//! No protocol bindings: the users (atl-labwc-setup; xtask-harness nested_wayland.rs and wayland_proxy.rs) know the opcodes they send. Test-environment only.

use std::io::{IoSlice, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};

/// libwayland's `MAX_FDS_OUT`: fds one `sendmsg` carries (wayland connection.c).
pub const MAX_FDS_PER_SEND: usize = 28;
/// Ancillary buffer for one `SCM_RIGHTS` message of `MAX_FDS_PER_SEND` fds.
pub const FD_CONTROL_BYTES: usize = rustix::cmsg_space!(ScmRights(MAX_FDS_PER_SEND));
/// How long one read waits for the compositor; it bounds a hang, a roundtrip takes ~30 ms unloaded (measured 2026-11-06).
pub const READ_WAIT: Duration = Duration::from_secs(10);

/// Opcodes of the core protocol (wayland.xml), requests and events numbered separately, in declaration order.
pub mod op {
    pub const DISPLAY_SYNC: u16 = 0;
    pub const DISPLAY_GET_REGISTRY: u16 = 1;
    pub const DISPLAY_ERROR: u16 = 0;
    pub const REGISTRY_BIND: u16 = 0;
    pub const REGISTRY_GLOBAL: u16 = 0;
    pub const CALLBACK_DONE: u16 = 0;
}

/// A request argument: `u`, `i`, `f` (wl_fixed, 24.8) or a string (`s`).
#[derive(Debug)]
pub enum Arg<'a> {
    U(u32),
    I(i32),
    /// wl_fixed: 24.8.
    F(f64),
    S(&'a str),
}

/// `v` as wl_fixed (24.8) carries it.
pub fn fixed(v: f64) -> f64 {
    (v * 256.0).round() / 256.0
}

pub fn word(body: &[u8], at: usize) -> Result<u32> {
    body.get(at..at + 4)
        .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .context("short Wayland message")
}

/// A string argument at `at`: length (with the NUL), bytes, padding to 4.
pub fn string(body: &[u8], at: usize) -> Result<String> {
    let len = word(body, at)? as usize;
    let text = body
        .get(at + 4..at + 4 + len)
        .context("short Wayland string")?;
    Ok(String::from_utf8_lossy(text.strip_suffix(&[0]).unwrap_or(text)).into_owned())
}

/// The word after the string argument at `at`.
pub fn word_after_string(body: &[u8], at: usize) -> Result<u32> {
    let len = word(body, at)? as usize;
    word(body, at + 4 + len.div_ceil(4) * 4)
}

/// A minimal Wayland client: requests out (fds through [`Client::attach`]), events back by roundtrip. No fds in: nothing it binds sends any.
pub struct Client {
    stream: UnixStream,
    next: u32,
    pending: Vec<u8>,
    pending_fds: Vec<OwnedFd>,
    /// Bytes read past the last roundtrip's `done`: events the compositor sent on its own (an input method's `done`), kept for the next roundtrip. Dropping them lost IME serials (measured 2026-11-06: 4 of 5 ime gate runs).
    inbox: Vec<u8>,
}

impl Client {
    /// Wraps a connected socket; the read timeout is [`READ_WAIT`].
    pub fn new(stream: UnixStream) -> Result<Self> {
        stream.set_read_timeout(Some(READ_WAIT))?;
        // id 1 is wl_display
        Ok(Self {
            stream,
            next: 2,
            pending: Vec::new(),
            pending_fds: Vec::new(),
            inbox: Vec::new(),
        })
    }

    /// `fd` for the next request taking one: fds travel beside the bytes, in order (the wire has no slot for them).
    pub fn attach(&mut self, fd: OwnedFd) {
        self.pending_fds.push(fd);
    }

    pub fn new_id(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    pub fn send(&mut self, object: u32, opcode: u16, args: &[Arg<'_>]) {
        let mut body = Vec::new();
        for a in args {
            match a {
                Arg::U(v) => body.extend(v.to_ne_bytes()),
                Arg::I(v) => body.extend(v.to_ne_bytes()),
                Arg::F(v) => body.extend(((fixed(*v) * 256.0) as i32).to_ne_bytes()),
                Arg::S(s) => {
                    let len = s.len() + 1;
                    body.extend((len as u32).to_ne_bytes());
                    body.extend(s.as_bytes());
                    body.resize(body.len() + len.div_ceil(4) * 4 - s.len(), 0);
                }
            }
        }
        let size = (8 + body.len()) as u32;
        self.pending.extend(object.to_ne_bytes());
        self.pending
            .extend(((size << 16) | u32::from(opcode)).to_ne_bytes());
        self.pending.extend(body);
    }

    /// Flushes the requests, then reads events until a `wl_display.sync` sent after them is done; a `wl_display.error` fails.
    pub fn roundtrip(&mut self) -> Result<Vec<(u32, u16, Vec<u8>)>> {
        let done = self.new_id();
        self.send(1, op::DISPLAY_SYNC, &[Arg::U(done)]);
        forward(
            self.stream.as_fd(),
            &std::mem::take(&mut self.pending),
            &mut self.pending_fds,
        )?;
        let mut events = Vec::new();
        let mut buf = std::mem::take(&mut self.inbox);
        loop {
            let mut chunk = [0u8; 4096];
            let n = self
                .stream
                .read(&mut chunk)
                .context("reading from the compositor")?;
            ensure!(n > 0, "the compositor closed the connection");
            buf.extend(&chunk[..n]);
            while buf.len() >= 8 {
                let object = word(&buf, 0)?;
                let second = word(&buf, 4)?;
                let size = (second >> 16) as usize;
                if buf.len() < size {
                    break;
                }
                let opcode = (second & 0xffff) as u16;
                let body = buf[8..size].to_vec();
                buf.drain(..size);
                if object == 1 && opcode == op::DISPLAY_ERROR {
                    bail!(
                        "protocol error on object {}: code {}, {}",
                        word(&body, 0)?,
                        word(&body, 4)?,
                        string(&body, 8)?
                    );
                }
                if object == done && opcode == op::CALLBACK_DONE {
                    self.inbox = buf;
                    return Ok(events);
                }
                events.push((object, opcode, body));
            }
        }
    }

    /// Reads and drops what the compositor sends until it closes the connection. A client that only holds objects must keep reading, or the compositor's events pile up on its socket.
    pub fn drain_until_closed(&mut self) -> Result<()> {
        self.stream.set_read_timeout(None)?;
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("reading from the compositor"),
            }
        }
    }
}

/// Sends `bytes`, the first `sendmsg` carrying up to `MAX_FDS_PER_SEND` of `fds` (removed from the queue).
pub fn forward(to: BorrowedFd<'_>, bytes: &[u8], fds: &mut Vec<OwnedFd>) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// wayland.xml's wire format, written out by hand: `wl_registry.bind(7, "wl_seat", 5, 9)` on object 2 is 8 header bytes, name, string length 8 ("wl_seat" + NUL), 8 string bytes, version, id = 32 bytes.
    #[test]
    fn bind_follows_the_wire_format() {
        let (a, _b) = UnixStream::pair().expect("pair");
        let mut c = Client::new(a).expect("client");
        c.send(
            2,
            op::REGISTRY_BIND,
            &[Arg::U(7), Arg::S("wl_seat"), Arg::U(5), Arg::U(9)],
        );
        let mut want = Vec::new();
        for w in [2u32, 32 << 16, 7, 8] {
            want.extend(w.to_ne_bytes());
        }
        want.extend(b"wl_seat\0");
        for w in [5u32, 9] {
            want.extend(w.to_ne_bytes());
        }
        assert_eq!(c.pending, want);
        assert_eq!(string(&want[8..], 4).expect("string"), "wl_seat");
        assert_eq!(word_after_string(&want[8..], 4).expect("version"), 5);
    }
}
