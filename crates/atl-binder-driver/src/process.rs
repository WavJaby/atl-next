// SPDX-License-Identifier: MIT
//! One process's connection to binderd, safe: the control connection, the region, the calling thread's connection, and each driver operation as protocol messages.

use std::cell::RefCell;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Mutex;

use atl_binder_wire::proto::{self, Reply, Request, VERSION, WriteReadReply};
use atl_binder_wire::shim;
use rustix::io::{Errno, fcntl_dupfd_cloexec};
use rustix::net::SocketType;

use crate::region::Region;

/// The device path libbinder opens (`ProcessState::self`, `DEFAULT_BINDER_VM`'s driver `kDefaultDriver`); the others (`/dev/vndbinder`, `/dev/hwbinder`) do not exist here.
pub const DEVICE: &str = "/dev/binder";

thread_local! {
    /// The calling thread's connection: binderd's `binder_thread`. Dropped (closed) at thread exit, which binderd takes as `BINDER_THREAD_EXIT`.
    static THREAD: RefCell<Option<OwnedFd>> = const { RefCell::new(None) };
}

/// A `BINDER_WRITE_READ` that failed: errno and how much of the write stream was consumed (the kernel copies `bwr` back on failure too).
#[derive(Debug, PartialEq, Eq)]
pub struct WriteReadError {
    pub errno: Errno,
    pub write_consumed: usize,
}

pub struct Process {
    /// Control requests and replies pair up in order: one at a time.
    control: Mutex<OwnedFd>,
    binder_version: i32,
    region: Mutex<Option<Region>>,
}

fn status(reply: &Reply) -> Result<(), Errno> {
    match reply {
        Reply::Status(0) => Ok(()),
        Reply::Status(e) if *e < 0 => Err(Errno::from_raw_os_error(-e)),
        _ => Err(Errno::PROTO),
    }
}

/// The next message on `sock` that is not a poll `Wake` (a stale one may precede any reply, proto module doc).
fn recv_reply(sock: BorrowedFd<'_>) -> Result<(Reply, Vec<OwnedFd>), Errno> {
    loop {
        let (msg, fds) = proto::recv(sock)?.ok_or(Errno::PIPE)?;
        match Reply::decode(&msg).map_err(|_| Errno::PROTO)? {
            Reply::Wake => continue,
            r => return Ok((r, fds)),
        }
    }
}

impl Process {
    /// Handshakes `control` (a SEQPACKET socket, else `EBADF`); a version binderd does not accept: `EPROTO`.
    pub fn connect(control: OwnedFd) -> Result<Self, Errno> {
        if rustix::net::sockopt::socket_type(&control)? != SocketType::SEQPACKET {
            return Err(Errno::BADF);
        }
        let p = Self {
            control: Mutex::new(control),
            binder_version: 0,
            region: Mutex::new(None),
        };
        let (reply, _) = p.control(&Request::Hello { version: VERSION }, &[])?;
        match reply {
            Reply::Hello {
                version: VERSION,
                binder_version,
            } => Ok(Self {
                binder_version,
                ..p
            }),
            Reply::Status(e) if e < 0 => Err(Errno::from_raw_os_error(-e)),
            _ => Err(Errno::PROTO),
        }
    }

    fn control(
        &self,
        req: &Request,
        fds: &[BorrowedFd<'_>],
    ) -> Result<(Reply, Vec<OwnedFd>), Errno> {
        let c = self.control.lock().map_err(|_| Errno::IO)?;
        proto::send(c.as_fd(), &req.encode(), fds)?;
        recv_reply(c.as_fd())
    }

    fn control_status(&self, req: &Request) -> Result<(), Errno> {
        status(&self.control(req, &[])?.0)
    }

    /// `BINDER_VERSION`.
    pub fn binder_version(&self) -> i32 {
        self.binder_version
    }

    /// A new driver fd: a dup of the control connection, close-on-exec.
    pub fn dup(&self) -> Result<OwnedFd, Errno> {
        let c = self.control.lock().map_err(|_| Errno::IO)?;
        fcntl_dupfd_cloexec(c.as_fd(), 0)
    }

    /// `mmap` of the driver: the region's base address. One region per process (binderd refuses a second: `EBUSY`).
    pub fn mmap(&self, len: usize) -> Result<usize, Errno> {
        let mut slot = self.region.lock().map_err(|_| Errno::IO)?;
        if slot.is_some() {
            return Err(Errno::BUSY);
        }
        let region = Region::map(len)?;
        if let Err(e) = self.control_status(&Request::Mmap { size: len as u64 }) {
            region.unmap()?;
            return Err(e);
        }
        *slot = Some(region);
        Ok(region.base())
    }

    /// `munmap` of the region; any other range: `EINVAL`.
    pub fn munmap(&self, addr: usize, len: usize) -> Result<(), Errno> {
        let mut slot = self.region.lock().map_err(|_| Errno::IO)?;
        match *slot {
            Some(r) if r.base() == addr && r.len() == len => {
                *slot = None;
                r.unmap()
            }
            _ => Err(Errno::INVAL),
        }
    }

    pub fn set_max_threads(&self, n: u32) -> Result<(), Errno> {
        self.control_status(&Request::SetMaxThreads(n))
    }

    pub fn set_context_mgr(&self, ptr: u64, cookie: u64, flags: u32) -> Result<(), Errno> {
        self.control_status(&Request::SetContextMgr { ptr, cookie, flags })
    }

    /// An ioctl binderd does not implement: sent for binderd to count, answered `EINVAL`.
    pub fn unsupported(&self, ioctl: u32) -> Result<(), Errno> {
        self.control_status(&Request::Unsupported { ioctl })
    }

    fn new_thread(&self) -> Result<OwnedFd, Errno> {
        let (reply, mut fds) = self.control(&Request::NewThread, &[])?;
        status(&reply)?;
        if fds.len() != 1 {
            return Err(Errno::PROTO);
        }
        Ok(fds.remove(0))
    }

    /// Runs `f` on the calling thread's connection, made first if the thread has none.
    fn with_thread<R>(
        &self,
        f: impl FnOnce(BorrowedFd<'_>) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        THREAD.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(self.new_thread()?);
            }
            f(slot.as_ref().ok_or(Errno::IO)?.as_fd())
        })
    }

    /// `BINDER_THREAD_EXIT`: closes the calling thread's connection.
    pub fn thread_exit(&self) {
        THREAD.with(|slot| drop(slot.borrow_mut().take()));
    }

    /// The calling thread's connection, in poll mode: readable while the thread has work.
    pub fn poll_fd(&self) -> Result<RawFd, Errno> {
        self.with_thread(|sock| {
            proto::send(sock, &Request::Poll.encode(), &[])?;
            status(&recv_reply(sock)?.0)?;
            Ok(sock.as_raw_fd())
        })
    }

    /// `BINDER_WRITE_READ` with BC stream `commands` and read capacity `read_buf`: (write_consumed, read_consumed).
    /// `read_mem(ptr, len)` reads the process's memory (transaction data and offsets); `dup_fd(fd)` duplicates an fd a transaction passes.
    pub fn write_read(
        &self,
        commands: &[u8],
        read_buf: &mut [u8],
        read_mem: &mut dyn FnMut(u64, usize) -> Vec<u8>,
        dup_fd: &mut dyn FnMut(i32) -> Result<OwnedFd, Errno>,
    ) -> Result<(usize, usize), WriteReadError> {
        let fail = |errno| WriteReadError {
            errno,
            write_consumed: 0,
        };
        let region = *self.region.lock().map_err(|_| fail(Errno::IO))?;
        let mut commands = commands.to_vec();
        let out = shim::outgoing(
            &mut commands,
            region.map(|r| (r.base() as u64, r.len() as u64)),
            read_mem,
        );
        let fds = out
            .fds
            .iter()
            .map(|fd| dup_fd(*fd))
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail)?;
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
        let req = Request::WriteRead {
            read_size: read_buf.len() as u64,
            commands,
            payload: out.payload,
        };
        let (reply, got) = self
            .with_thread(|sock| {
                proto::send(sock, &req.encode(), &borrowed)?;
                recv_reply(sock)
            })
            .map_err(fail)?;
        let Reply::WriteRead(r) = reply else {
            return Err(fail(Errno::PROTO));
        };
        self.deliver(r, got, region, read_buf)
    }

    /// Places a reply: copies and fd numbers into the region, the BR stream (addresses rebased) into `read_buf`.
    fn deliver(
        &self,
        mut r: WriteReadReply,
        fds: Vec<OwnedFd>,
        region: Option<Region>,
        read_buf: &mut [u8],
    ) -> Result<(usize, usize), WriteReadError> {
        let write_consumed = usize::try_from(r.write_consumed).unwrap_or(usize::MAX);
        let fail = |errno| WriteReadError {
            errno,
            write_consumed,
        };
        if r.status != 0 {
            return Err(fail(Errno::from_raw_os_error(-r.status)));
        }
        if r.returns.len() > read_buf.len() || fds.len() != r.fd_offsets.len() {
            return Err(fail(Errno::PROTO));
        }
        if !r.copies.is_empty() || !fds.is_empty() {
            let region = region.ok_or(fail(Errno::FAULT))?;
            for (off, bytes) in &r.copies {
                if !region.write(*off, bytes) {
                    return Err(fail(Errno::FAULT));
                }
            }
            for (off, fd) in r.fd_offsets.iter().zip(fds) {
                // the fd now belongs to the receiving Parcel, as the kernel's installed fd does
                let n = fd.into_raw_fd() as u32;
                if !region.write(*off, &n.to_ne_bytes()) {
                    return Err(fail(Errno::FAULT));
                }
            }
        }
        if let Some(region) = region {
            shim::incoming(&mut r.returns, region.base() as u64);
        }
        read_buf[..r.returns.len()].copy_from_slice(&r.returns);
        Ok((write_consumed, r.returns.len()))
    }
}
