// SPDX-License-Identifier: MIT
//! The app <-> binderd wire protocol, version `VERSION`: AF_UNIX `SOCK_SEQPACKET` connections, native-endian fields.
//!
//! Connections:
//! - control, one per process: the broker makes a socketpair per spawned process and gives binderd one end tagged with the process's identity; the process inherits the other (`ATL_BINDER_FD`). Carries the ioctls that are per process: handshake, mapping, thread-pool size, context manager, new thread connections.
//! - thread, one per binder thread: `NewThread` on control returns a fresh socketpair end by `SCM_RIGHTS`, same tag. It is binderd's `binder_thread`: no tids are trusted. Closing it = `BINDER_THREAD_EXIT`.
//!
//! Every request gets exactly one reply on the same connection, in order. A `WriteRead` whose read would sleep in the kernel gets its reply when the read completes.
//! Poll mode (`Poll` on a thread connection, libbinder `setupPolling`): binderd then also sends `Wake` on that connection, outside any reply, by this rule: after handling any event, for each poll thread with no `Wake` outstanding that has work (`Binderd::has_work`, the kernel's `binder_poll` readability), send one `Wake`; the next request on that connection clears "outstanding".
//! So the connection is readable exactly while the kernel's fd would poll readable, plus at most one stale `Wake` the next reply's reader discards (`recv_reply` skips `Wake`). Level-triggered pollers (libutils Looper) see each pending item.
//!
//! Framing: a message is sent as fragments of at most `FRAGMENT` bytes, the first prefixed with the message's total length (u64), so a transaction payload of up to the 4 MiB region fits whatever the socket buffer size. Fds ride on the first fragment, at most `MAX_FDS` (the kernel's `SCM_MAX_FD`).
//!
//! Pointer fields: `WriteRead` carries its BC stream with transaction `data.ptr.buffer` / `.offsets` as offsets into the request's payload; the reply's BR stream carries them as offsets into the receiver's region, whose bytes come as `copies`; received fds are installed and their numbers written at `fd_offsets` (region offsets) in order.
#![allow(clippy::module_name_repetitions)]

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::io::Errno;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

/// Protocol version, checked for equality in the handshake. Bump on any change to the messages below.
pub const VERSION: u32 = 1;

/// Largest fragment: well under the default AF_UNIX send buffer (net.core.wmem_default, 208 KiB), so one fragment never fails with EMSGSIZE.
pub const FRAGMENT: usize = 32 * 1024;

/// Fds per message: the kernel's `SCM_MAX_FD` (include/net/scm.h), the most one `SCM_RIGHTS` message takes.
pub const MAX_FDS: usize = 253;

/// Largest message accepted: the region cap (4 MiB, binder_alloc.c) plus room for the BC stream and headers; anything larger is malformed.
pub const MAX_MESSAGE: u64 = 8 << 20;

/// A request, from a process to binderd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Control: first message, `binder_driver_open`.
    Hello { version: u32 },
    /// Control: the process mapped its region (`binder_driver_mmap`).
    Mmap { size: u64 },
    /// Control: `BINDER_SET_MAX_THREADS`.
    SetMaxThreads(u32),
    /// Control: `BINDER_SET_CONTEXT_MGR_EXT` (legacy `SET_CONTEXT_MGR`: all zero).
    SetContextMgr { ptr: u64, cookie: u64, flags: u32 },
    /// Control: a connection for a new binder thread, returned as the reply's one fd.
    NewThread,
    /// Control: an ioctl binderd does not implement, for it to count; replied `-EINVAL`.
    Unsupported { ioctl: u32 },
    /// Thread: `BINDER_WRITE_READ`. The request's fds are the `BINDER_TYPE_FD` objects' fds in stream and object order.
    WriteRead {
        read_size: u64,
        commands: Vec<u8>,
        payload: Vec<u8>,
    },
    /// Thread: this thread polls its connection (`binder_driver_poll_fd`); `Wake` messages follow.
    Poll,
}

/// A message from binderd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// 0, or a negative errno.
    Status(i32),
    /// Handshake accepted: binderd's protocol version and the binder protocol it speaks (`BINDER_VERSION`).
    Hello {
        version: u32,
        binder_version: i32,
    },
    WriteRead(WriteReadReply),
    /// Poll mode: work is pending for this thread.
    Wake,
}

/// `BINDER_WRITE_READ`'s result. The reply's fds are installed in `fd_offsets` order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WriteReadReply {
    /// 0, or the ioctl's negative errno (then nothing was read).
    pub status: i32,
    pub write_consumed: u64,
    /// BR stream; `read_consumed` is its length.
    pub returns: Vec<u8>,
    /// (region offset, bytes) to place before the BR stream is used.
    pub copies: Vec<(u64, Vec<u8>)>,
    /// Region offsets of the u32 fd numbers to write, one per received fd.
    pub fd_offsets: Vec<u64>,
}

/// Request / reply tags.
mod tag {
    pub const HELLO: u32 = 1;
    pub const MMAP: u32 = 2;
    pub const SET_MAX_THREADS: u32 = 3;
    pub const SET_CONTEXT_MGR: u32 = 4;
    pub const NEW_THREAD: u32 = 5;
    pub const UNSUPPORTED: u32 = 6;
    pub const WRITE_READ: u32 = 16;
    pub const POLL: u32 = 17;
    pub const STATUS: u32 = 32;
    pub const HELLO_OK: u32 = 33;
    pub const WRITE_READ_REPLY: u32 = 34;
    pub const WAKE: u32 = 35;
}

/// A message that does not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

struct Enc(Vec<u8>);

impl Enc {
    fn new(tag: u32) -> Self {
        Self(tag.to_ne_bytes().to_vec())
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn i32(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn bytes(self, b: &[u8]) -> Self {
        let mut s = self.u64(b.len() as u64);
        s.0.extend_from_slice(b);
        s
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.0.len() < n {
            return Err(Malformed);
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_ne_bytes(
            self.take(4)?.try_into().map_err(|_| Malformed)?,
        ))
    }
    fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(i32::from_ne_bytes(
            self.take(4)?.try_into().map_err(|_| Malformed)?,
        ))
    }
    fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_ne_bytes(
            self.take(8)?.try_into().map_err(|_| Malformed)?,
        ))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, Malformed> {
        let n = usize::try_from(self.u64()?).map_err(|_| Malformed)?;
        Ok(self.take(n)?.to_vec())
    }
    fn end(self) -> Result<(), Malformed> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Malformed)
        }
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Hello { version } => Enc::new(tag::HELLO).u32(*version),
            Self::Mmap { size } => Enc::new(tag::MMAP).u64(*size),
            Self::SetMaxThreads(n) => Enc::new(tag::SET_MAX_THREADS).u32(*n),
            Self::SetContextMgr { ptr, cookie, flags } => Enc::new(tag::SET_CONTEXT_MGR)
                .u64(*ptr)
                .u64(*cookie)
                .u32(*flags),
            Self::NewThread => Enc::new(tag::NEW_THREAD),
            Self::Unsupported { ioctl } => Enc::new(tag::UNSUPPORTED).u32(*ioctl),
            Self::WriteRead {
                read_size,
                commands,
                payload,
            } => Enc::new(tag::WRITE_READ)
                .u64(*read_size)
                .bytes(commands)
                .bytes(payload),
            Self::Poll => Enc::new(tag::POLL),
        }
        .0
    }

    pub fn decode(b: &[u8]) -> Result<Self, Malformed> {
        let mut d = Dec(b);
        let r = match d.u32()? {
            tag::HELLO => Self::Hello { version: d.u32()? },
            tag::MMAP => Self::Mmap { size: d.u64()? },
            tag::SET_MAX_THREADS => Self::SetMaxThreads(d.u32()?),
            tag::SET_CONTEXT_MGR => Self::SetContextMgr {
                ptr: d.u64()?,
                cookie: d.u64()?,
                flags: d.u32()?,
            },
            tag::NEW_THREAD => Self::NewThread,
            tag::UNSUPPORTED => Self::Unsupported { ioctl: d.u32()? },
            tag::WRITE_READ => Self::WriteRead {
                read_size: d.u64()?,
                commands: d.bytes()?,
                payload: d.bytes()?,
            },
            tag::POLL => Self::Poll,
            _ => return Err(Malformed),
        };
        d.end()?;
        Ok(r)
    }
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Status(s) => Enc::new(tag::STATUS).i32(*s),
            Self::Hello {
                version,
                binder_version,
            } => Enc::new(tag::HELLO_OK).u32(*version).i32(*binder_version),
            Self::WriteRead(r) => {
                let mut e = Enc::new(tag::WRITE_READ_REPLY)
                    .i32(r.status)
                    .u64(r.write_consumed)
                    .bytes(&r.returns)
                    .u64(r.copies.len() as u64);
                for (off, bytes) in &r.copies {
                    e = e.u64(*off).bytes(bytes);
                }
                e = e.u64(r.fd_offsets.len() as u64);
                for off in &r.fd_offsets {
                    e = e.u64(*off);
                }
                e
            }
            Self::Wake => Enc::new(tag::WAKE),
        }
        .0
    }

    pub fn decode(b: &[u8]) -> Result<Self, Malformed> {
        let mut d = Dec(b);
        let r = match d.u32()? {
            tag::STATUS => Self::Status(d.i32()?),
            tag::HELLO_OK => Self::Hello {
                version: d.u32()?,
                binder_version: d.i32()?,
            },
            tag::WRITE_READ_REPLY => {
                let status = d.i32()?;
                let write_consumed = d.u64()?;
                let returns = d.bytes()?;
                let n = d.u64()?;
                let mut copies = Vec::new();
                for _ in 0..n {
                    copies.push((d.u64()?, d.bytes()?));
                }
                let n = d.u64()?;
                let mut fd_offsets = Vec::new();
                for _ in 0..n {
                    fd_offsets.push(d.u64()?);
                }
                Self::WriteRead(WriteReadReply {
                    status,
                    write_consumed,
                    returns,
                    copies,
                    fd_offsets,
                })
            }
            tag::WAKE => Self::Wake,
            _ => return Err(Malformed),
        };
        d.end()?;
        Ok(r)
    }
}

/// `msg` split into its fragments (module doc): the first starts with the u64 total length; fds go with the first.
pub fn fragments(msg: &[u8]) -> Vec<Vec<u8>> {
    let first = msg.len().min(FRAGMENT - 8);
    let mut head = (msg.len() as u64).to_ne_bytes().to_vec();
    head.extend_from_slice(&msg[..first]);
    let mut v = vec![head];
    v.extend(msg[first..].chunks(FRAGMENT).map(<[u8]>::to_vec));
    v
}

/// Sends `msg` with `fds` on `sock` (fragmented, see the module doc), blocking, retrying on EINTR. More than `MAX_FDS` fds: `EINVAL`.
pub fn send(sock: BorrowedFd<'_>, msg: &[u8], fds: &[BorrowedFd<'_>]) -> Result<(), Errno> {
    if fds.len() > MAX_FDS {
        return Err(Errno::INVAL);
    }
    for (i, f) in fragments(msg).iter().enumerate() {
        send_fragment(sock, f, if i == 0 { fds } else { &[] }, false)?;
    }
    Ok(())
}

/// Sends one fragment with `fds`, retrying on EINTR; `nonblocking`: `EAGAIN` instead of waiting for socket buffer space. A SEQPACKET send is all or nothing.
pub fn send_fragment(
    sock: BorrowedFd<'_>,
    frag: &[u8],
    fds: &[BorrowedFd<'_>],
    nonblocking: bool,
) -> Result<(), Errno> {
    let mut space = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(Errno::INVAL);
    }
    let flags = if nonblocking {
        SendFlags::NOSIGNAL | SendFlags::DONTWAIT
    } else {
        SendFlags::NOSIGNAL
    };
    loop {
        match sendmsg(sock, &[IoSlice::new(frag)], &mut control, flags) {
            Ok(n) if n == frag.len() => return Ok(()),
            Ok(_) => return Err(Errno::MSGSIZE),
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// A received message and its fds.
pub type Message = (Vec<u8>, Vec<OwnedFd>);

/// The next message on `sock` and its fds (close-on-exec), or `None` at end of stream (the peer closed), retrying on EINTR. A fragment that does not fit, truncated fds or a length past `MAX_MESSAGE`: `EPROTO`.
pub fn recv(sock: BorrowedFd<'_>) -> Result<Option<Message>, Errno> {
    let mut a = Assembler::default();
    loop {
        let Some((frag, fds)) = recv_fragment(sock, false)? else {
            return if a.is_empty() {
                Ok(None)
            } else {
                Err(Errno::PROTO)
            };
        };
        if let Some(m) = a.push(&frag, fds)? {
            return Ok(Some(m));
        }
    }
}

/// One fragment and its fds (close-on-exec), or `None` at end of stream, retrying on EINTR; `nonblocking`: `EAGAIN` when none is queued. A fragment that does not fit or truncated fds: `EPROTO`.
pub fn recv_fragment(sock: BorrowedFd<'_>, nonblocking: bool) -> Result<Option<Message>, Errno> {
    let mut buf = vec![0u8; FRAGMENT];
    let mut space = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let flags = if nonblocking {
        RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT
    } else {
        RecvFlags::CMSG_CLOEXEC
    };
    let n = loop {
        match recvmsg(sock, &mut [IoSliceMut::new(&mut buf)], &mut control, flags) {
            Ok(r) if r.flags.intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC) => {
                return Err(Errno::PROTO);
            }
            Ok(r) => break r.bytes,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e),
        }
    };
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = m {
            fds.extend(rights);
        }
    }
    if n == 0 {
        return Ok(None);
    }
    buf.truncate(n);
    Ok(Some((buf, fds)))
}

/// Reassembles a message from its fragments, one `push` per fragment received. Memory held = bytes received so far, never the claimed length (at most `MAX_MESSAGE`).
#[derive(Default)]
pub struct Assembler {
    /// Claimed total length and the bytes so far; `None` between messages.
    msg: Option<(usize, Vec<u8>)>,
    fds: Vec<OwnedFd>,
}

impl Assembler {
    /// Between messages: no fragment of a partial one held.
    pub fn is_empty(&self) -> bool {
        self.msg.is_none()
    }

    /// Takes the next fragment: the message once complete. A first fragment shorter than the length prefix, a length past `MAX_MESSAGE`, fds on a later fragment or bytes past the length: `EPROTO`.
    pub fn push(&mut self, frag: &[u8], fds: Vec<OwnedFd>) -> Result<Option<Message>, Errno> {
        let (total, msg) = match &mut self.msg {
            Some(m) if fds.is_empty() => {
                m.1.extend_from_slice(frag);
                m
            }
            Some(_) => return Err(Errno::PROTO),
            None => {
                let (len, rest) = frag.split_first_chunk::<8>().ok_or(Errno::PROTO)?;
                let total = u64::from_ne_bytes(*len);
                if total > MAX_MESSAGE {
                    return Err(Errno::PROTO);
                }
                self.fds = fds;
                self.msg.insert((
                    usize::try_from(total).map_err(|_| Errno::PROTO)?,
                    rest.to_vec(),
                ))
            }
        };
        match msg.len().cmp(&*total) {
            std::cmp::Ordering::Less => Ok(None),
            std::cmp::Ordering::Greater => Err(Errno::PROTO),
            std::cmp::Ordering::Equal => {
                let (_, msg) = self.msg.take().expect("set above");
                Ok(Some((msg, std::mem::take(&mut self.fds))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::*;

    fn pair() -> (OwnedFd, OwnedFd) {
        socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair")
    }

    #[test]
    fn requests_round_trip() {
        let all = [
            Request::Hello { version: VERSION },
            Request::Mmap { size: 1 << 20 },
            Request::SetMaxThreads(15),
            Request::SetContextMgr {
                ptr: 1,
                cookie: 2,
                flags: 3,
            },
            Request::NewThread,
            Request::Unsupported { ioctl: 0xc018_620b },
            Request::WriteRead {
                read_size: 256,
                commands: vec![1, 2, 3],
                payload: vec![4; 9],
            },
            Request::Poll,
        ];
        for r in all {
            assert_eq!(Request::decode(&r.encode()), Ok(r));
        }
    }

    #[test]
    fn replies_round_trip() {
        let all = [
            Reply::Status(-22),
            Reply::Hello {
                version: VERSION,
                binder_version: 8,
            },
            Reply::WriteRead(WriteReadReply {
                status: 0,
                write_consumed: 48,
                returns: vec![7; 12],
                copies: vec![(64, vec![1, 2]), (128, vec![])],
                fd_offsets: vec![72],
            }),
            Reply::Wake,
        ];
        for r in all {
            assert_eq!(Reply::decode(&r.encode()), Ok(r));
        }
    }

    /// Unknown tags, short fields, trailing bytes and a length past the end are refused.
    #[test]
    fn malformed_refused() {
        assert_eq!(Request::decode(&99u32.to_ne_bytes()), Err(Malformed));
        assert_eq!(Request::decode(&[1, 0]), Err(Malformed));
        let mut extra = Request::NewThread.encode();
        extra.push(0);
        assert_eq!(Request::decode(&extra), Err(Malformed));
        let mut long = Request::WriteRead {
            read_size: 0,
            commands: vec![],
            payload: vec![],
        }
        .encode();
        long[12..20].copy_from_slice(&5u64.to_ne_bytes());
        assert_eq!(Request::decode(&long), Err(Malformed));
        assert_eq!(Reply::decode(&[]), Err(Malformed));
    }

    /// A message several fragments long arrives whole, with its fd; then end of stream.
    #[test]
    fn fragments_and_fds() {
        let (a, b) = pair();
        let (x, _y) = pair();
        // 2.5 fragments: the first holds FRAGMENT - 8 bytes
        let msg: Vec<u8> = (0..FRAGMENT * 5 / 2).map(|i| (i % 251) as u8).collect();
        send(a.as_fd(), &msg, &[x.as_fd()]).expect("send");
        let (got, fds) = recv(b.as_fd()).expect("recv").expect("message");
        assert_eq!(got, msg);
        assert_eq!(fds.len(), 1);
        drop(a);
        assert!(recv(b.as_fd()).expect("eof").is_none());
    }

    /// A length prefix past `MAX_MESSAGE` is refused before any allocation.
    #[test]
    fn oversized_refused() {
        let (a, b) = pair();
        let len = (MAX_MESSAGE + 1).to_ne_bytes();
        send_fragment(a.as_fd(), &len, &[], false).expect("send");
        assert!(matches!(recv(b.as_fd()), Err(Errno::PROTO)));
        assert_eq!(
            send(a.as_fd(), &[], &vec![a.as_fd(); MAX_FDS + 1]),
            Err(Errno::INVAL)
        );
    }
}
