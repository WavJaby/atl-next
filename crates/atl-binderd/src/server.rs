// SPDX-License-Identifier: MIT
//! binderd's socket server: the core state machine behind `atl_binder_wire::proto` connections, one thread, `poll(2)` over every connection.
//!
//! Processes are registered over the admin connection (the broker's; in tests the harness): `register` message = tag + whether it may be the context manager, with the process's control connection as its one fd. binderd exits when the admin connection closes.
//! Connection EOF: a thread connection = `BINDER_THREAD_EXIT`; the control connection = process death (`Binderd::process_exit`), its thread connections dropped.
//! Every request binderd refuses at the protocol level is counted (`Fault`), as the core counts its rejections.
//! Known limit (M2): sockets are blocking; a client that stops mid-message or never reads its replies stalls binderd. Untrusted apps need non-blocking connections with per-connection buffers before the broker spawns them.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use atl_binder_wire::proto::{self, Reply, Request, VERSION, WriteReadReply};
use atl_binder_wire::uapi::BINDER_CURRENT_PROTOCOL_VERSION;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno as SysErrno;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

use crate::{
    Binderd, Errno, Outcome, PidNs, ProcId, ProcSpec, Read, ReadOut, Tag, ThreadId, WriteRead,
};

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
    /// An ioctl the driver does not implement (`Request::Unsupported`).
    UnsupportedIoctl,
    /// A malformed admin registration.
    BadRegistration,
}

/// Admin registration: uid, host pid, context-manager flag; the control connection is its fd.
pub fn encode_register(uid: u32, host_pid: i32, context_manager: bool) -> Vec<u8> {
    let mut v = uid.to_ne_bytes().to_vec();
    v.extend_from_slice(&host_pid.to_ne_bytes());
    v.extend_from_slice(&u32::from(context_manager).to_ne_bytes());
    v
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

struct Conn {
    fd: OwnedFd,
    kind: Kind,
}

pub struct Server {
    core: Binderd<OwnedFd>,
    admin: Option<OwnedFd>,
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
        Self {
            core: Binderd::new(),
            admin: Some(admin),
            conns: BTreeMap::new(),
            by_thread: BTreeMap::new(),
            next: 0,
            faults: BTreeMap::new(),
        }
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
        self.conns.insert(id, Conn { fd, kind });
        id
    }

    /// Runs until the admin connection closes.
    pub fn run(&mut self) -> Result<(), SysErrno> {
        while self.admin.is_some() {
            let ids: Vec<ConnId> = self.conns.keys().copied().collect();
            let ready: Vec<(Option<ConnId>, bool)> = {
                let mut fds: Vec<PollFd<'_>> = Vec::with_capacity(ids.len() + 1);
                let admin = self.admin.as_ref().expect("checked");
                fds.push(PollFd::new(admin, PollFlags::IN));
                for id in &ids {
                    fds.push(PollFd::new(&self.conns[id].fd, PollFlags::IN));
                }
                loop {
                    match poll(&mut fds, None) {
                        Ok(_) => break,
                        Err(SysErrno::INTR) => continue,
                        Err(e) => return Err(e),
                    }
                }
                fds.iter()
                    .enumerate()
                    .filter(|(_, f)| !f.revents().is_empty())
                    .map(|(i, f)| {
                        let hup = f.revents().intersects(PollFlags::HUP | PollFlags::ERR)
                            && !f.revents().contains(PollFlags::IN);
                        ((i > 0).then(|| ids[i - 1]), hup)
                    })
                    .collect()
            };
            for (id, _hup) in ready {
                match id {
                    None => self.admin_event(),
                    Some(id) => self.conn_event(id),
                }
            }
        }
        Ok(())
    }

    fn admin_event(&mut self) {
        let Some(admin) = self.admin.as_ref() else {
            return;
        };
        match proto::recv(admin.as_fd()) {
            Ok(None) | Err(_) => self.admin = None,
            Ok(Some((msg, mut fds))) => {
                let status = match (decode_register(&msg), fds.len()) {
                    (Some((uid, host_pid, cm)), 1) => {
                        self.register(fds.remove(0), uid, host_pid, cm);
                        0
                    }
                    _ => {
                        self.fault(Fault::BadRegistration);
                        -SysErrno::INVAL.raw_os_error()
                    }
                };
                let admin = self.admin.as_ref().expect("checked");
                if proto::send(admin.as_fd(), &Reply::Status(status).encode(), &[]).is_err() {
                    self.admin = None;
                }
            }
        }
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

    fn send(&mut self, id: ConnId, reply: &Reply, fds: &[BorrowedFd<'_>]) {
        let Some(c) = self.conns.get(&id) else {
            return;
        };
        if proto::send(c.fd.as_fd(), &reply.encode(), fds).is_err() {
            self.drop_conn(id);
        }
    }

    fn conn_event(&mut self, id: ConnId) {
        let Some(c) = self.conns.get(&id) else {
            return;
        };
        let (msg, fds) = match proto::recv(c.fd.as_fd()) {
            Ok(Some(m)) => m,
            Ok(None) => return self.drop_conn(id),
            Err(_) => {
                self.fault(Fault::Malformed);
                return self.drop_conn(id);
            }
        };
        let Ok(req) = Request::decode(&msg) else {
            self.fault(Fault::Malformed);
            return self.send(id, &Reply::Status(-SysErrno::PROTO.raw_os_error()), &[]);
        };
        match self.conns[&id].kind {
            Kind::Control { proc, hello } => self.control(id, proc, hello, req),
            Kind::Thread { thread, .. } => self.thread(id, thread, req, fds),
        }
        self.finish();
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
            (true, Request::Unsupported { .. }) => {
                self.fault(Fault::UnsupportedIoctl);
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
                        return self.send(id, &Reply::Status(0), &[theirs.as_fd()]);
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
        self.send(id, &reply, &[]);
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
                        self.send(id, &Reply::WriteRead(r), &[]);
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
                self.send(id, &Reply::Status(0), &[]);
            }
            _ => {
                self.fault(Fault::WrongConnection);
                self.send(id, &Reply::Status(-SysErrno::INVAL.raw_os_error()), &[]);
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
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
        self.send(id, &Reply::WriteRead(r), &borrowed);
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
            self.send(id, &Reply::Wake, &[]);
        }
    }

    fn drop_conn(&mut self, id: ConnId) {
        let Some(c) = self.conns.remove(&id) else {
            return;
        };
        match c.kind {
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
