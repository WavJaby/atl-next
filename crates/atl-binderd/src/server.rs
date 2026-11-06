// SPDX-License-Identifier: MIT
//! binderd's socket server: the core state machine behind `atl_binder_wire::proto` connections, one thread, `poll(2)` over every connection.
//!
//! Processes are registered over the admin connection (the broker's; in tests the harness): `register` message = tag + whether it may be the context manager, with the process's control connection as its one fd. binderd exits when the admin connection closes.
//! Connection EOF: a thread connection = `BINDER_THREAD_EXIT`; the control connection = process death (`Binderd::process_exit`), its thread connections dropped.
//! Every request binderd refuses at the protocol level is counted (`Fault`), as the core counts its rejections.
//!
//! Every peer is untrusted, so no socket call blocks: fragments are received and sent with `MSG_DONTWAIT`, each connection has its own reassembly buffer (`proto::Assembler`, at most `MAX_MESSAGE`) and output queue (at most `MAX_QUEUED` messages).
//! Per poll round each ready connection first flushes what its socket takes, then reads up to one whole message, which runs through the core synchronously; a connection stopped mid-message or not reading only holds its own buffers.
//! A connection past its output bound is counted (`Fault::Overflow`) and its process dropped, as if it had crashed (`Server::kill`).

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use atl_binder_wire::proto::{self, Assembler, Reply, Request, VERSION, WriteReadReply};
use atl_binder_wire::uapi::BINDER_CURRENT_PROTOCOL_VERSION;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno as SysErrno;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

use crate::{
    Binderd, Errno, Outcome, PidNs, ProcId, ProcSpec, Read, ReadOut, Tag, ThreadId, WriteRead,
};

/// Messages a connection may have queued beyond its socket buffer. A peer has at most one request outstanding (a driver call waits for its reply; the control connection is held under a lock per call; the broker's `register` waits too), so a legitimate backlog is: a stale `Wake` not yet flushed + the reply to its current request + the one `Wake` that request re-arms = 3. More means the peer sends requests without reading replies.
pub const MAX_QUEUED: usize = 3;

/// Protocol-level refusals, counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Fault {
    /// A message that does not decode, or a socket error on receive.
    Malformed,
    /// Hello with another protocol version.
    BadVersion,
    /// A request before the handshake, or a second Hello.
    BadHandshake,
    /// A control request on a thread connection or the reverse.
    WrongConnection,
    /// An ioctl the driver does not implement (`Request::Unsupported`), by request code: the log names which.
    UnsupportedIoctl(u32),
    /// A malformed admin registration.
    BadRegistration,
    /// More than `MAX_QUEUED` replies waiting for a peer that does not read: its process is dropped.
    Overflow,
}

/// Admin registration: uid, host pid, context-manager flag; the control connection is its fd.
pub fn encode_register(uid: u32, host_pid: i32, context_manager: bool) -> Vec<u8> {
    let mut v = uid.to_ne_bytes().to_vec();
    v.extend_from_slice(&host_pid.to_ne_bytes());
    v.extend_from_slice(&u32::from(context_manager).to_ne_bytes());
    v
}

/// The admin side (broker, test harness): registers the process whose control connection is `control` and waits for binderd's answer.
pub fn register(
    admin: BorrowedFd<'_>,
    control: BorrowedFd<'_>,
    uid: u32,
    host_pid: i32,
    context_manager: bool,
) -> Result<(), SysErrno> {
    proto::send(
        admin,
        &encode_register(uid, host_pid, context_manager),
        &[control],
    )?;
    let (msg, _) = proto::recv(admin)?.ok_or(SysErrno::PIPE)?;
    match Reply::decode(&msg) {
        Ok(Reply::Status(0)) => Ok(()),
        Ok(Reply::Status(e)) if e < 0 => Err(SysErrno::from_raw_os_error(-e)),
        _ => Err(SysErrno::PROTO),
    }
}

fn decode_register(b: &[u8]) -> Option<(u32, i32, bool)> {
    if b.len() != 12 {
        return None;
    }
    let u = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().expect("4 bytes"));
    Some((u(0), u(4) as i32, u(8) != 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ConnId(u64);

enum Kind {
    Admin,
    Control {
        proc: ProcId,
        hello: bool,
    },
    Thread {
        proc: ProcId,
        thread: ThreadId,
        poll: bool,
        wake_outstanding: bool,
        /// write_consumed of a read now blocked, for its reply.
        pending: Option<u64>,
    },
}

/// A message being sent: its fragments not yet taken by the socket; its fds ride on the first.
struct Outgoing {
    frags: VecDeque<Vec<u8>>,
    fds: Vec<OwnedFd>,
}

struct Conn {
    fd: OwnedFd,
    kind: Kind,
    rx: Assembler,
    tx: VecDeque<Outgoing>,
}

pub struct Server {
    core: Binderd<OwnedFd>,
    admin: Option<ConnId>,
    conns: BTreeMap<ConnId, Conn>,
    by_thread: BTreeMap<ThreadId, ConnId>,
    next: u64,
    faults: BTreeMap<Fault, u64>,
}

fn errno_of(e: Errno) -> i32 {
    -match e {
        Errno::Inval => SysErrno::INVAL,
        Errno::Perm => SysErrno::PERM,
        Errno::Busy => SysErrno::BUSY,
        Errno::Fault => SysErrno::FAULT,
    }
    .raw_os_error()
}

impl Server {
    pub fn new(admin: OwnedFd) -> Self {
        let mut s = Self {
            core: Binderd::new(),
            admin: None,
            conns: BTreeMap::new(),
            by_thread: BTreeMap::new(),
            next: 0,
            faults: BTreeMap::new(),
        };
        s.admin = Some(s.add_conn(admin, Kind::Admin));
        s
    }

    pub fn core(&self) -> &Binderd<OwnedFd> {
        &self.core
    }

    pub fn faults(&self) -> &BTreeMap<Fault, u64> {
        &self.faults
    }

    fn fault(&mut self, f: Fault) {
        *self.faults.entry(f).or_default() += 1;
    }

    fn add_conn(&mut self, fd: OwnedFd, kind: Kind) -> ConnId {
        self.next += 1;
        let id = ConnId(self.next);
        if let Kind::Thread { thread, .. } = kind {
            self.by_thread.insert(thread, id);
        }
        self.conns.insert(
            id,
            Conn {
                fd,
                kind,
                rx: Assembler::default(),
                tx: VecDeque::new(),
            },
        );
        id
    }

    /// Runs until the admin connection closes.
    pub fn run(&mut self) -> Result<(), SysErrno> {
        while self.admin.is_some() {
            let ids: Vec<ConnId> = self.conns.keys().copied().collect();
            let ready: Vec<(ConnId, PollFlags)> = {
                let mut fds: Vec<PollFd<'_>> = ids
                    .iter()
                    .map(|id| {
                        let c = &self.conns[id];
                        let mut want = PollFlags::IN;
                        if !c.tx.is_empty() {
                            want |= PollFlags::OUT;
                        }
                        PollFd::new(&c.fd, want)
                    })
                    .collect();
                loop {
                    match poll(&mut fds, None) {
                        Ok(_) => break,
                        Err(SysErrno::INTR) => continue,
                        Err(e) => return Err(e),
                    }
                }
                fds.iter()
                    .zip(&ids)
                    .filter(|(f, _)| !f.revents().is_empty())
                    .map(|(f, id)| (*id, f.revents()))
                    .collect()
            };
            for (id, ev) in ready {
                if ev.intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP) {
                    self.flush(id);
                }
                if ev.intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) {
                    self.receive(id);
                }
            }
        }
        Ok(())
    }

    /// Reads fragments until one message is whole (then handles it) or none is queued. EOF or a socket / framing error drops the connection.
    fn receive(&mut self, id: ConnId) {
        loop {
            let Some(c) = self.conns.get_mut(&id) else {
                return;
            };
            let got = match proto::recv_fragment(c.fd.as_fd(), true) {
                Err(SysErrno::AGAIN) => return,
                Err(_) => Err(()),
                Ok(None) if c.rx.is_empty() => return self.drop_conn(id),
                Ok(None) => Err(()),
                Ok(Some((frag, fds))) => c.rx.push(&frag, fds).map_err(|_| ()),
            };
            match got {
                Err(()) => {
                    self.fault(Fault::Malformed);
                    return self.drop_conn(id);
                }
                Ok(None) => continue,
                Ok(Some((msg, fds))) => return self.message(id, &msg, fds),
            }
        }
    }

    fn message(&mut self, id: ConnId, msg: &[u8], fds: Vec<OwnedFd>) {
        if matches!(self.conns[&id].kind, Kind::Admin) {
            return self.admin_message(id, msg, fds);
        }
        let Ok(req) = Request::decode(msg) else {
            self.fault(Fault::Malformed);
            return self.send(id, &Reply::Status(-SysErrno::PROTO.raw_os_error()), vec![]);
        };
        match self.conns[&id].kind {
            Kind::Control { proc, hello } => self.control(id, proc, hello, req),
            Kind::Thread { thread, .. } => self.thread(id, thread, req, fds),
            Kind::Admin => {}
        }
        self.finish();
    }

    fn admin_message(&mut self, id: ConnId, msg: &[u8], mut fds: Vec<OwnedFd>) {
        let status = match (decode_register(msg), fds.len()) {
            (Some((uid, host_pid, cm)), 1) => {
                self.register(fds.remove(0), uid, host_pid, cm);
                0
            }
            _ => {
                self.fault(Fault::BadRegistration);
                -SysErrno::INVAL.raw_os_error()
            }
        };
        self.send(id, &Reply::Status(status), vec![]);
    }

    /// A process whose control connection is `control`, tagged as the host's pid namespace sees it (sandbox pid namespaces come with the broker).
    pub fn register(&mut self, control: OwnedFd, uid: u32, host_pid: i32, context_manager: bool) {
        let proc = self.core.add_proc(ProcSpec {
            tag: Tag {
                uid,
                host_pid,
                pid_ns: PidNs::Host,
            },
            may_be_context_manager: context_manager,
        });
        self.add_conn(control, Kind::Control { proc, hello: false });
    }

    /// Queues `reply` and sends what the socket takes now; past `MAX_QUEUED` the connection is dropped.
    fn send(&mut self, id: ConnId, reply: &Reply, fds: Vec<OwnedFd>) {
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        c.tx.push_back(Outgoing {
            frags: proto::fragments(&reply.encode()).into(),
            fds,
        });
        self.flush(id);
        if self.conns.get(&id).is_some_and(|c| c.tx.len() > MAX_QUEUED) {
            self.fault(Fault::Overflow);
            self.kill(id);
        }
    }

    /// Drops the process `id` belongs to, via its control connection (a thread connection alone would only be `BINDER_THREAD_EXIT`); the admin connection: binderd stops.
    fn kill(&mut self, id: ConnId) {
        let control = match self.conns.get(&id).map(|c| &c.kind) {
            Some(Kind::Thread { proc, .. }) => self
                .conns
                .iter()
                .find(|(_, c)| matches!(c.kind, Kind::Control { proc: p, .. } if p == *proc))
                .map(|(cid, _)| *cid),
            _ => None,
        };
        self.drop_conn(control.unwrap_or(id));
        self.drop_conn(id);
    }

    /// Sends queued fragments until the socket buffer is full; a send error drops the connection.
    fn flush(&mut self, id: ConnId) {
        loop {
            let Some(Conn { fd, tx, .. }) = self.conns.get_mut(&id) else {
                return;
            };
            let Some(out) = tx.front_mut() else {
                return;
            };
            let Some(frag) = out.frags.front() else {
                tx.pop_front();
                continue;
            };
            let fds: Vec<BorrowedFd<'_>> = out.fds.iter().map(AsFd::as_fd).collect();
            match proto::send_fragment(fd.as_fd(), frag, &fds, true) {
                Ok(()) => {
                    out.frags.pop_front();
                    out.fds.clear();
                }
                Err(SysErrno::AGAIN) => return,
                Err(_) => return self.drop_conn(id),
            }
        }
    }

    fn control(&mut self, id: ConnId, proc: ProcId, hello: bool, req: Request) {
        let st = |r: Result<(), Errno>| Reply::Status(r.map_or_else(errno_of, |()| 0));
        let reply = match (hello, req) {
            (false, Request::Hello { version }) if version == VERSION => {
                if let Some(Conn {
                    kind: Kind::Control { hello, .. },
                    ..
                }) = self.conns.get_mut(&id)
                {
                    *hello = true;
                }
                Reply::Hello {
                    version: VERSION,
                    binder_version: BINDER_CURRENT_PROTOCOL_VERSION,
                }
            }
            (false, Request::Hello { .. }) => {
                self.fault(Fault::BadVersion);
                Reply::Status(-SysErrno::PROTONOSUPPORT.raw_os_error())
            }
            (true, Request::Hello { .. }) | (false, _) => {
                self.fault(Fault::BadHandshake);
                Reply::Status(-SysErrno::PROTO.raw_os_error())
            }
            (true, Request::Mmap { size }) => st(self.core.map_region(proc, size)),
            (true, Request::SetMaxThreads(n)) => st(self.core.set_max_threads(proc, n)),
            (true, Request::SetContextMgr { ptr, cookie, flags }) => {
                st(self.core.set_context_manager(proc, ptr, cookie, flags))
            }
            (true, Request::Unsupported { ioctl }) => {
                self.fault(Fault::UnsupportedIoctl(ioctl));
                Reply::Status(-SysErrno::INVAL.raw_os_error())
            }
            (true, Request::NewThread) => {
                let pair = socketpair(
                    AddressFamily::UNIX,
                    SocketType::SEQPACKET,
                    SocketFlags::CLOEXEC,
                    None,
                );
                match (pair, self.core.add_thread(proc)) {
                    (Ok((ours, theirs)), Some(thread)) => {
                        self.add_conn(
                            ours,
                            Kind::Thread {
                                proc,
                                thread,
                                poll: false,
                                wake_outstanding: false,
                                pending: None,
                            },
                        );
                        return self.send(id, &Reply::Status(0), vec![theirs]);
                    }
                    (Err(e), _) => Reply::Status(-e.raw_os_error()),
                    (_, None) => Reply::Status(-SysErrno::INVAL.raw_os_error()),
                }
            }
            (true, Request::WriteRead { .. } | Request::Poll) => {
                self.fault(Fault::WrongConnection);
                Reply::Status(-SysErrno::INVAL.raw_os_error())
            }
        };
        self.send(id, &reply, vec![]);
    }

    fn thread(&mut self, id: ConnId, thread: ThreadId, req: Request, fds: Vec<OwnedFd>) {
        if let Some(Conn {
            kind: Kind::Thread {
                wake_outstanding, ..
            },
            ..
        }) = self.conns.get_mut(&id)
        {
            *wake_outstanding = false;
        }
        match req {
            Request::WriteRead {
                read_size,
                commands,
                payload,
            } => {
                let out = self.core.write_read(
                    thread,
                    WriteRead {
                        commands: &commands,
                        payload: &payload,
                        read_size: usize::try_from(read_size).unwrap_or(usize::MAX),
                        fds,
                    },
                );
                match out {
                    Err(e) => {
                        let r = WriteReadReply {
                            status: errno_of(e.errno),
                            write_consumed: e.write_consumed as u64,
                            ..Default::default()
                        };
                        self.send(id, &Reply::WriteRead(r), vec![]);
                    }
                    Ok(Outcome {
                        write_consumed,
                        read,
                    }) => match read {
                        Read::Skipped => self.send_read(id, write_consumed as u64, None),
                        Read::Done(out) => self.send_read(id, write_consumed as u64, Some(out)),
                        Read::Blocked => {
                            if let Some(Conn {
                                kind: Kind::Thread { pending, .. },
                                ..
                            }) = self.conns.get_mut(&id)
                            {
                                *pending = Some(write_consumed as u64);
                            }
                        }
                    },
                }
            }
            Request::Poll => {
                if let Some(Conn {
                    kind: Kind::Thread { poll, .. },
                    ..
                }) = self.conns.get_mut(&id)
                {
                    *poll = true;
                }
                self.send(id, &Reply::Status(0), vec![]);
            }
            _ => {
                self.fault(Fault::WrongConnection);
                self.send(id, &Reply::Status(-SysErrno::INVAL.raw_os_error()), vec![]);
            }
        }
    }

    fn send_read(&mut self, id: ConnId, write_consumed: u64, out: Option<ReadOut<OwnedFd>>) {
        let mut r = WriteReadReply {
            write_consumed,
            ..Default::default()
        };
        let mut fds = Vec::new();
        if let Some(out) = out {
            r.returns = out.returns;
            r.copies = out
                .copies
                .into_iter()
                .map(|c| (c.offset, c.bytes))
                .collect();
            for f in out.fds {
                r.fd_offsets.push(f.offset);
                fds.push(f.fd);
            }
        }
        self.send(id, &Reply::WriteRead(r), fds);
    }

    /// After every event: replies for reads that completed, then poll wakeups (proto module doc).
    fn finish(&mut self) {
        for (thread, out) in self.core.take_completed() {
            let Some(&id) = self.by_thread.get(&thread) else {
                continue;
            };
            let wc = match self.conns.get_mut(&id) {
                Some(Conn {
                    kind: Kind::Thread { pending, .. },
                    ..
                }) => pending.take().unwrap_or(0),
                _ => continue,
            };
            self.send_read(id, wc, Some(out));
        }
        let wake: Vec<ConnId> = self
            .conns
            .iter()
            .filter_map(|(id, c)| match c.kind {
                Kind::Thread {
                    thread,
                    poll: true,
                    wake_outstanding: false,
                    ..
                } if self.core.has_work(thread) => Some(*id),
                _ => None,
            })
            .collect();
        for id in wake {
            if let Some(Conn {
                kind: Kind::Thread {
                    wake_outstanding, ..
                },
                ..
            }) = self.conns.get_mut(&id)
            {
                *wake_outstanding = true;
            }
            self.send(id, &Reply::Wake, vec![]);
        }
    }

    fn drop_conn(&mut self, id: ConnId) {
        let Some(c) = self.conns.remove(&id) else {
            return;
        };
        match c.kind {
            Kind::Admin => self.admin = None,
            Kind::Thread { thread, .. } => {
                self.by_thread.remove(&thread);
                self.core.thread_exit(thread);
            }
            Kind::Control { proc, .. } => {
                let threads: Vec<ConnId> = self
                    .conns
                    .iter()
                    .filter(|(_, c)| matches!(c.kind, Kind::Thread { proc: p, .. } if p == proc))
                    .map(|(id, _)| *id)
                    .collect();
                for t in threads {
                    if let Some(Conn {
                        kind: Kind::Thread { thread, .. },
                        ..
                    }) = self.conns.remove(&t)
                    {
                        self.by_thread.remove(&thread);
                    }
                }
                self.core.process_exit(proc);
            }
        }
        self.finish();
    }
}
