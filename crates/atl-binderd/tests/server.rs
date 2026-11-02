// SPDX-License-Identifier: MIT
//! The socket server end to end: a server thread, clients speaking `proto` over real SEQPACKET socketpairs. Expected values from the protocol (proto module doc) and the uapi, never from the server.
#![allow(clippy::unwrap_used, reason = "test helpers outside #[test] functions; a failure is the test failing")]

use std::os::fd::{AsFd, OwnedFd};
use std::thread::JoinHandle;

use atl_binder_wire::proto::{self, Reply, Request, VERSION, WriteReadReply};
use atl_binder_wire::uapi::*;
use atl_binder_wire::{Command, CommandWriter, Return, TransactionData, decode_returns};
use atl_binderd::server::{Fault, Server, encode_register};
use atl_binderd::{Ignored, Reject};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

/// libbinder's region (ProcessState.cpp BINDER_VM_SIZE, 4 KiB pages) and read buffer (256).
const REGION: u64 = (1 << 20) - 2 * 4096;
const READ: u64 = 256;

fn pair() -> (OwnedFd, OwnedFd) {
    socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .expect("socketpair")
}

struct Harness {
    admin: OwnedFd,
    server: JoinHandle<Server>,
}

impl Harness {
    fn start() -> Self {
        let (admin, theirs) = pair();
        let server = std::thread::spawn(move || {
            let mut s = Server::new(theirs);
            s.run().expect("run");
            s
        });
        Self { admin, server }
    }

    /// A registered process's control connection.
    fn process(&self, uid: u32, cm: bool) -> OwnedFd {
        let (ours, theirs) = pair();
        proto::send(
            self.admin.as_fd(),
            &encode_register(uid, 1, cm),
            &[theirs.as_fd()],
        )
        .unwrap();
        assert_eq!(recv(&self.admin), Reply::Status(0));
        ours
    }

    fn stop(self) -> Server {
        drop(self.admin);
        self.server.join().expect("server thread")
    }
}

fn recv(fd: &OwnedFd) -> Reply {
    let (m, _) = proto::recv(fd.as_fd()).unwrap().expect("message");
    Reply::decode(&m).unwrap()
}

fn call(fd: &OwnedFd, r: Request) -> (Reply, Vec<OwnedFd>) {
    proto::send(fd.as_fd(), &r.encode(), &[]).unwrap();
    let (m, fds) = proto::recv(fd.as_fd()).unwrap().expect("reply");
    (Reply::decode(&m).unwrap(), fds)
}

fn hello(c: &OwnedFd) {
    let (r, _) = call(c, Request::Hello { version: VERSION });
    assert_eq!(
        r,
        Reply::Hello {
            version: VERSION,
            binder_version: BINDER_CURRENT_PROTOCOL_VERSION
        }
    );
}

fn thread(c: &OwnedFd) -> OwnedFd {
    let (r, mut fds) = call(c, Request::NewThread);
    assert_eq!(r, Reply::Status(0));
    fds.remove(0)
}

fn write_read(t: &OwnedFd, cmds: &[Command], payload: Vec<u8>, read_size: u64) -> WriteReadReply {
    let mut w = CommandWriter::new();
    for c in cmds {
        w.push(c);
    }
    let (r, _) = call(
        t,
        Request::WriteRead {
            read_size,
            commands: w.0,
            payload,
        },
    );
    match r {
        Reply::WriteRead(r) => r,
        other => panic!("{other:?}"),
    }
}

fn errno(e: rustix::io::Errno) -> Reply {
    Reply::Status(-e.raw_os_error())
}

/// Handshake rules, per-process ioctls and their guards, each counted.
#[test]
fn control_requests() {
    let h = Harness::start();
    let sm = h.process(1000, true);
    let app = h.process(10001, false);
    // before Hello: refused
    assert_eq!(
        call(&app, Request::NewThread).0,
        errno(rustix::io::Errno::PROTO)
    );
    assert_eq!(
        call(
            &app,
            Request::Hello {
                version: VERSION + 1
            }
        )
        .0,
        errno(rustix::io::Errno::PROTONOSUPPORT)
    );
    hello(&app);
    hello(&sm);
    assert_eq!(
        call(&app, Request::Hello { version: VERSION }).0,
        errno(rustix::io::Errno::PROTO)
    );
    assert_eq!(
        call(&app, Request::Mmap { size: REGION }).0,
        Reply::Status(0)
    );
    assert_eq!(
        call(&app, Request::Mmap { size: REGION }).0,
        errno(rustix::io::Errno::BUSY)
    );
    assert_eq!(call(&app, Request::SetMaxThreads(15)).0, Reply::Status(0));
    assert_eq!(
        call(&app, Request::Unsupported { ioctl: 0x4004_6210 }).0,
        errno(rustix::io::Errno::INVAL)
    );
    assert_eq!(call(&app, Request::Poll).0, errno(rustix::io::Errno::INVAL));
    // only the tagged process may become the context manager; the flag is accepted and counted
    let cm = Request::SetContextMgr {
        ptr: 0,
        cookie: 0,
        flags: FLAT_BINDER_FLAG_TXN_SECURITY_CTX,
    };
    assert_eq!(call(&app, cm.clone()).0, errno(rustix::io::Errno::PERM));
    assert_eq!(call(&sm, cm).0, Reply::Status(0));
    let t = thread(&app);
    assert_eq!(
        call(&t, Request::SetMaxThreads(1)).0,
        errno(rustix::io::Errno::INVAL)
    );
    proto::send(app.as_fd(), &[1, 2], &[]).unwrap();
    assert_eq!(recv(&app), errno(rustix::io::Errno::PROTO));
    // admin: a registration without its fd
    proto::send(h.admin.as_fd(), &encode_register(1, 1, false), &[]).unwrap();
    assert_eq!(recv(&h.admin), errno(rustix::io::Errno::INVAL));
    let s = h.stop();
    let f = s.faults();
    assert_eq!(
        (
            f[&Fault::BadHandshake],
            f[&Fault::BadVersion],
            f[&Fault::UnsupportedIoctl]
        ),
        (2, 1, 1)
    );
    assert_eq!(
        (
            f[&Fault::WrongConnection],
            f[&Fault::Malformed],
            f[&Fault::BadRegistration]
        ),
        (2, 1, 1)
    );
    assert_eq!(s.core().rejects()[&Reject::ContextManagerDenied], 1);
    assert_eq!(s.core().rejects()[&Reject::AlreadyMapped], 1);
    assert_eq!(s.core().ignored()[&Ignored::TxnSecurityCtx], 1);
}

/// A call from an app to handle 0 reaches a polling context manager: Wake once while work is pending, the read delivers BR_TRANSACTION with the region copy, the reply reaches the blocked caller with the request's write_consumed.
#[test]
fn transaction_through_poll() {
    let h = Harness::start();
    let sm = h.process(1000, true);
    let app = h.process(10001, false);
    for c in [&sm, &app] {
        hello(c);
        assert_eq!(call(c, Request::Mmap { size: REGION }).0, Reply::Status(0));
    }
    assert_eq!(
        call(
            &sm,
            Request::SetContextMgr {
                ptr: 0,
                cookie: 0,
                flags: 0
            }
        )
        .0,
        Reply::Status(0)
    );
    let st = thread(&sm);
    assert_eq!(call(&st, Request::Poll).0, Reply::Status(0));
    // a new thread is readable once (need_return): one Wake
    assert_eq!(recv(&st), Reply::Wake);
    // setupPolling: BC_ENTER_LOOPER write-only; that ioctl clears need_return, so no further Wake until work arrives
    let r = write_read(&st, &[Command::EnterLooper], vec![], 0);
    assert_eq!(r.status, 0);

    let at = thread(&app);
    let txn = TransactionData {
        code: 7,
        data_size: 8,
        offsets: 8,
        ..Default::default()
    }
    .with_handle(0);
    let mut w = CommandWriter::new();
    w.push(&Command::Transaction(txn));
    let want_consumed = w.0.len() as u64;
    proto::send(
        at.as_fd(),
        &Request::WriteRead {
            read_size: READ,
            commands: w.0,
            payload: vec![9; 8],
        }
        .encode(),
        &[],
    )
    .unwrap();
    // the poll thread becomes readable (one Wake)
    assert_eq!(recv(&st), Reply::Wake);
    let r = write_read(&st, &[], vec![], READ);
    let rets = decode_returns(&r.returns).unwrap();
    let Return::Transaction(t) = rets[1] else {
        panic!("{rets:?}")
    };
    assert_eq!((t.code, t.sender_euid, t.data_size), (7, 10001, 8));
    assert_eq!(r.copies, vec![(t.buffer, vec![9; 8])]);
    let reply = TransactionData {
        data_size: 0,
        ..Default::default()
    };
    let r2 = write_read(
        &st,
        &[Command::Reply(reply), Command::FreeBuffer(t.buffer)],
        vec![],
        READ,
    );
    assert_eq!(r2.status, 0);
    let (m, _) = proto::recv(at.as_fd()).unwrap().unwrap();
    let Reply::WriteRead(r) = Reply::decode(&m).unwrap() else {
        panic!()
    };
    assert_eq!(r.write_consumed, want_consumed);
    let mut rets = decode_returns(&r.returns).unwrap();
    // the deferred TRANSACTION_COMPLETE may end this read; libbinder reads again (waitForResponse) for the reply
    if !matches!(rets.last(), Some(Return::Reply(_))) {
        rets.extend(decode_returns(&write_read(&at, &[], vec![], READ).returns).unwrap());
    }
    assert!(matches!(rets.last(), Some(Return::Reply(_))), "{rets:?}");
    // control EOF = death: later calls to the manager see a dead reply
    drop(st);
    drop(sm);
    // the hangup is pending before this request is sent, so the server's next poll sees both and handles the lower (older) connection first
    assert_eq!(call(&app, Request::SetMaxThreads(1)).0, Reply::Status(0));
    let r = write_read(&at, &[Command::Transaction(txn)], vec![0; 8], READ);
    let rets = decode_returns(&r.returns).unwrap();
    assert!(rets.contains(&Return::DeadReply), "{rets:?} {r:?}");
    h.stop();
}
