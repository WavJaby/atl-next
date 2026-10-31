// SPDX-License-Identifier: MIT
//! Scripted BC -> BR streams for simulated processes. Expected streams are written out from the uapi
//! and the kernel's behaviour (binder.c v6.12, cited per test), never produced by binderd itself.

use atl_binder_wire::uapi::*;
use atl_binder_wire::{
    Command, CommandWriter, HandleCookie, Object, PtrCookie, Return, TransactionData,
    decode_returns,
};

use super::*;

/// libbinder's read buffer: `IPCThreadState` sizes `mIn` to 256 bytes.
const READ: usize = 256;
/// libbinder's mapping: `BINDER_VM_SIZE` = 1 MiB - 2 pages (ProcessState.cpp:47), 4 KiB pages.
const REGION: u64 = (1 << 20) - 2 * 4096;

/// Context manager node as ServiceManager registers it.
const SM_PTR: u64 = 0x5000;
const SM_COOKIE: u64 = 0x5001;

type D = Binderd<u32>;

struct World {
    d: D,
    sm: ProcId,
    a: ProcId,
    b: ProcId,
}

fn tag(uid: u32, host_pid: i32, pid_ns: PidNs) -> Tag {
    Tag {
        uid,
        host_pid,
        pid_ns,
    }
}

/// atld's ServiceManager (system uid 1000, host pid ns) and two apps in their own sandboxes.
fn world() -> World {
    world_with(REGION, ACCEPT_NONE)
}

const ACCEPT_NONE: u32 = 0;

fn world_with(sm_region: u64, sm_flags: u32) -> World {
    let mut d = D::new();
    let sm = d.add_proc(ProcSpec {
        tag: tag(1000, 100, PidNs::Host),
        region_size: sm_region,
        may_be_context_manager: true,
    });
    let a = d.add_proc(ProcSpec {
        tag: tag(10001, 200, PidNs::Sandbox { id: 1, pid: 2 }),
        region_size: REGION,
        may_be_context_manager: false,
    });
    let b = d.add_proc(ProcSpec {
        tag: tag(10002, 300, PidNs::Sandbox { id: 2, pid: 2 }),
        region_size: REGION,
        may_be_context_manager: false,
    });
    let s = d.add_thread(sm).unwrap();
    d.set_context_manager(s, SM_PTR, SM_COOKIE, sm_flags)
        .unwrap();
    World { d, sm, a, b }
}

fn stream(cmds: &[Command]) -> Vec<u8> {
    let mut w = CommandWriter::new();
    for c in cmds {
        w.push(c);
    }
    w.0
}

fn wr(d: &mut D, t: ThreadId, cmds: &[Command], payload: &[u8], fds: Vec<u32>) -> Outcome<u32> {
    let commands = stream(cmds);
    d.write_read(
        t,
        WriteRead {
            commands: &commands,
            payload,
            read_size: READ,
            fds,
        },
    )
    .unwrap()
}

/// Write only (`read_size` 0), as libbinder's `flushCommands`: the thread does not wait.
fn w0(d: &mut D, t: ThreadId, cmds: &[Command]) -> usize {
    let commands = stream(cmds);
    let o = d
        .write_read(
            t,
            WriteRead {
                commands: &commands,
                payload: &[],
                read_size: 0,
                fds: vec![],
            },
        )
        .unwrap();
    assert_eq!(o.read, Read::Skipped);
    o.write_consumed
}

/// The decoded BR stream of a finished read.
fn done(o: &Read<u32>) -> Vec<Return> {
    match o {
        Read::Done(out) => decode_returns(&out.returns).unwrap(),
        other => panic!("expected a finished read, got {other:?}"),
    }
}

/// The only completed read, for thread `t`.
fn completed(d: &mut D, t: ThreadId) -> ReadOut<u32> {
    let mut c = d.take_completed();
    assert_eq!(c.len(), 1, "completions: {c:?} rejects {:?}", d.rejects());
    let (who, out) = c.remove(0);
    assert_eq!(who, t);
    out
}

fn rets(out: &ReadOut<u32>) -> Vec<Return> {
    decode_returns(&out.returns).unwrap()
}

/// A new thread's first read returns at once (`looper_need_return`); afterwards reads wait.
fn thread(d: &mut D, p: ProcId, looper: bool) -> ThreadId {
    let t = d.add_thread(p).unwrap();
    let cmds: &[Command] = if looper { &[Command::EnterLooper] } else { &[] };
    let o = wr(d, t, cmds, &[], vec![]);
    assert_eq!(done(&o.read), vec![Return::Noop]);
    t
}

/// A looper thread now blocked waiting for process work.
fn waiting_looper(d: &mut D, p: ProcId) -> ThreadId {
    let t = thread(d, p, true);
    let o = wr(d, t, &[], &[], vec![]);
    assert_eq!(o.read, Read::Blocked);
    t
}

fn call(handle: u32, code: u32, flags: u32, data_size: u64, offsets_size: u64) -> TransactionData {
    TransactionData {
        code,
        flags,
        data_size,
        offsets_size,
        buffer: 0,
        offsets: data_size,
        ..Default::default()
    }
    .with_handle(handle)
}

/// Payload with `objects` at the given data offsets, followed by the offsets array.
fn payload(data_size: usize, objects: &[(usize, Object)]) -> (Vec<u8>, u64) {
    let mut p = vec![0u8; data_size];
    for (off, o) in objects {
        o.write(&mut p, *off);
    }
    for (off, _) in objects {
        p.extend_from_slice(&(*off as u64).to_ne_bytes());
    }
    (p, (objects.len() * 8) as u64)
}

fn delivered(target: u64, cookie: u64, code: u32, flags: u32) -> TransactionData {
    TransactionData {
        target,
        cookie,
        code,
        flags,
        ..Default::default()
    }
}

/// Kernel `binder_thread_read` stops at one transaction per read and reports `sender_pid` /
/// `sender_euid`, `data.ptr.offsets = buffer + ALIGN(data_size, 8)`.
fn txn_of(r: &Return) -> TransactionData {
    match r {
        Return::Transaction(t) | Return::Reply(t) => *t,
        other => panic!("not a transaction: {other:?}"),
    }
}

#[test]
fn context_manager_only_for_atld() {
    let mut w = world();
    let t = w.d.add_thread(w.a).unwrap();
    // one manager exists: EBUSY, checked first (binder.c:5436-5440)
    assert_eq!(w.d.set_context_manager(t, 1, 1, 0), Err(Errno::Busy));
    // an app before any manager: EPERM (our policy in place of the kernel's SELinux hook)
    let mut fresh = D::new();
    let app = fresh.add_proc(ProcSpec {
        tag: tag(10001, 200, PidNs::Host),
        region_size: REGION,
        may_be_context_manager: false,
    });
    let at = fresh.add_thread(app).unwrap();
    assert_eq!(fresh.set_context_manager(at, 1, 1, 0), Err(Errno::Perm));
    assert_eq!(fresh.rejects()[&Reject::ContextManagerDenied], 1);
    assert_eq!(w.d.rejects()[&Reject::ContextManagerBusy], 1);
}

/// Sync call to handle 0 and its reply. Caller's TRANSACTION_COMPLETE is deferred until the reply
/// (binder.c:3703-3712), the server's comes at once (:3688); sender_pid as the receiver's pid
/// namespace sees it (:4977-4986), 0 for a reply (`t->from` is NULL, :3277).
#[test]
fn transaction_and_reply() {
    let mut w = world();
    let s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    let data = b"request!".to_vec();
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 1, 0, 8, 0))],
        &data,
        vec![],
    );
    assert_eq!(o.write_consumed, 4 + 64);
    assert_eq!(o.read, Read::Blocked);

    let got = completed(&mut w.d, s);
    let mut want = delivered(SM_PTR, SM_COOKIE, 1, 0);
    // atld (host pid ns) sees the app's host pid; uid from the broker's tag
    want.sender_pid = 200;
    want.sender_euid = 10001;
    want.data_size = 8;
    // first buffer in an empty region: offset 0; offsets array at ALIGN(8, 8)
    want.buffer = 0;
    want.offsets = 8;
    assert_eq!(rets(&got), vec![Return::Noop, Return::Transaction(want)]);
    assert_eq!(
        got.copies,
        vec![RegionCopy {
            offset: 0,
            bytes: data
        }]
    );

    let reply = TransactionData {
        data_size: 4,
        offsets: 4,
        ..Default::default()
    };
    let o = wr(
        &mut w.d,
        s,
        &[Command::Reply(reply), Command::FreeBuffer(0)],
        b"done",
        vec![],
    );
    assert_eq!(
        done(&o.read),
        vec![Return::Noop, Return::TransactionComplete]
    );
    let got = completed(&mut w.d, a);
    let mut want = delivered(0, 0, 0, 0);
    want.sender_pid = 0;
    want.sender_euid = 1000;
    want.data_size = 4;
    want.offsets = 8;
    assert_eq!(
        rets(&got),
        vec![
            Return::Noop,
            Return::TransactionComplete,
            Return::Reply(want)
        ]
    );
    assert!(w.d.rejects().is_empty(), "{:?}", w.d.rejects());
}

/// B hands its binder to ServiceManager, which keeps a strong handle (BC_ACQUIRE), then A fetches it.
/// Returns (A's handle, B's thread). Exercises BINDER -> HANDLE translation (binder.c:2219-2270) and
/// the owner's BR_INCREFS + BR_ACQUIRE (`BINDER_WORK_NODE`, :4777-4870).
fn publish_and_fetch(w: &mut World, s: ThreadId) -> (u32, ThreadId) {
    const B_PTR: u64 = 0xB0;
    const B_COOKIE: u64 = 0xB1;
    let b = thread(&mut w.d, w.b, false);
    let obj = Object::Binder {
        weak: false,
        flags: 0,
        ptr: B_PTR,
        cookie: B_COOKIE,
    };
    let (p, offsets_size) = payload(24, &[(0, obj)]);
    let o = wr(
        &mut w.d,
        b,
        &[Command::Transaction(call(0, 3, 0, 24, offsets_size))],
        &p,
        vec![],
    );
    assert_eq!(o.read, Read::Blocked);
    let got = completed(&mut w.d, s);
    let t = txn_of(&rets(&got)[1]);
    // the manager sees a strong handle, first free number from 1 (binder.c:1050-1062)
    let mut bytes = got.copies[0].bytes.clone();
    assert_eq!(
        Object::read(&bytes, 0),
        Some(Object::Handle {
            weak: false,
            flags: 0,
            handle: 1,
            cookie: 0
        })
    );
    // keep it, free the buffer, reply
    let o = wr(
        &mut w.d,
        s,
        &[
            Command::Acquire(1),
            Command::FreeBuffer(t.buffer),
            Command::Reply(TransactionData::default()),
        ],
        &[],
        vec![],
    );
    assert_eq!(
        done(&o.read),
        vec![Return::Noop, Return::TransactionComplete]
    );
    let pc = PtrCookie {
        ptr: B_PTR,
        cookie: B_COOKIE,
    };
    // node work queued (deferred) on B's thread during translation, ahead of its TRANSACTION_COMPLETE
    let got = completed(&mut w.d, b);
    let r = rets(&got);
    assert_eq!(
        r[..4],
        [
            Return::Noop,
            Return::IncRefs(pc),
            Return::Acquire(pc),
            Return::TransactionComplete
        ]
    );
    assert!(matches!(r[4], Return::Reply(_)));
    let consumed = w0(
        &mut w.d,
        b,
        &[
            Command::IncRefsDone(pc),
            Command::AcquireDone(pc),
            Command::FreeBuffer(txn_of(&r[4]).buffer),
        ],
    );
    assert_eq!(consumed, 4 + 16 + 4 + 16 + 4 + 8);
    // the manager's looper goes back to waiting
    assert_eq!(wr(&mut w.d, s, &[], &[], vec![]).read, Read::Blocked);

    // A asks; the manager answers with its handle 1
    let a = thread(&mut w.d, w.a, false);
    wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 4, 0, 0, 0))],
        &[],
        vec![],
    );
    let got = completed(&mut w.d, s);
    let t = txn_of(&rets(&got)[1]);
    bytes.truncate(24);
    let (p, offsets_size) = payload(24, &[(0, Object::read(&bytes, 0).unwrap())]);
    let reply = TransactionData {
        data_size: 24,
        offsets_size,
        offsets: 24,
        ..Default::default()
    };
    wr(
        &mut w.d,
        s,
        &[Command::Reply(reply), Command::FreeBuffer(t.buffer)],
        &p,
        vec![],
    );
    let got = completed(&mut w.d, a);
    let r = rets(&got);
    let t = txn_of(&r[2]);
    // a handle to another process's node: a new handle in A's table, again 1
    assert_eq!(
        Object::read(&got.copies[0].bytes, 0),
        Some(Object::Handle {
            weak: false,
            flags: 0,
            handle: 1,
            cookie: 0
        })
    );
    w0(
        &mut w.d,
        a,
        &[Command::Acquire(1), Command::FreeBuffer(t.buffer)],
    );
    (1, b)
}

/// While serving A's call, the manager calls back into A: the nested call goes to A's thread that is
/// waiting in the call chain, not to A's process queue (binder.c:3192-3220); A sees its own deferred
/// TRANSACTION_COMPLETE first, then the incoming call. A handle sent back to its owner arrives as the
/// owner's binder (:2272-2310).
#[test]
fn nested_call_reaches_the_waiting_thread() {
    let mut w = world();
    let s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    // a second, idle looper in A must not get the nested call
    let a_idle = waiting_looper(&mut w.d, w.a);
    const A_PTR: u64 = 0xA0;
    const A_COOKIE: u64 = 0xA1;
    let obj = Object::Binder {
        weak: false,
        flags: 0,
        ptr: A_PTR,
        cookie: A_COOKIE,
    };
    let (p, offsets_size) = payload(24, &[(0, obj)]);
    wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 1, 0, 24, offsets_size))],
        &p,
        vec![],
    );
    let got = completed(&mut w.d, s);
    let first = txn_of(&rets(&got)[1]);

    // manager -> A's binder (handle 1), nested inside A's call
    let o = wr(
        &mut w.d,
        s,
        &[Command::Transaction(call(1, 2, 0, 0, 0))],
        &[],
        vec![],
    );
    assert_eq!(o.read, Read::Blocked);
    let got = completed(&mut w.d, a);
    let pc = PtrCookie {
        ptr: A_PTR,
        cookie: A_COOKIE,
    };
    let mut nested = delivered(A_PTR, A_COOKIE, 2, 0);
    // the host-namespace manager is invisible inside A's sandbox
    nested.sender_pid = 0;
    nested.sender_euid = 1000;
    assert_eq!(
        rets(&got),
        vec![
            Return::Noop,
            Return::IncRefs(pc),
            Return::Acquire(pc),
            Return::TransactionComplete,
            Return::Transaction(nested)
        ]
    );
    assert!(w.d.take_completed().is_empty(), "idle looper stays asleep");

    // A takes the refs it was given and answers the nested call
    let o = wr(
        &mut w.d,
        a,
        &[
            Command::IncRefsDone(pc),
            Command::AcquireDone(pc),
            Command::Reply(TransactionData::default()),
            Command::FreeBuffer(0),
        ],
        &[],
        vec![],
    );
    assert_eq!(
        o.read,
        Read::Done(ReadOut {
            returns: {
                let mut v = Vec::new();
                Return::Noop.encode(&mut v);
                Return::TransactionComplete.encode(&mut v);
                v
            },
            copies: vec![],
            fds: vec![],
        })
    );
    let got = completed(&mut w.d, s);
    let mut reply = delivered(0, 0, 0, 0);
    reply.sender_euid = 10001;
    // the manager's first buffer still holds A's call at 0 (32 bytes: 24 data + 8 offsets)
    reply.buffer = 32;
    reply.offsets = 32;
    assert_eq!(
        rets(&got),
        vec![
            Return::Noop,
            Return::TransactionComplete,
            Return::Reply(reply)
        ]
    );

    // A goes back to waiting for its own call's reply
    assert_eq!(wr(&mut w.d, a, &[], &[], vec![]).read, Read::Blocked);
    // the manager sends A's handle 1 back to A in its reply: A gets its own binder, not a handle
    let (p, offsets_size) = payload(
        24,
        &[(
            0,
            Object::Handle {
                weak: false,
                flags: 0,
                handle: 1,
                cookie: 0,
            },
        )],
    );
    let r = TransactionData {
        data_size: 24,
        offsets_size,
        offsets: 24,
        ..Default::default()
    };
    wr(
        &mut w.d,
        s,
        &[
            Command::FreeBuffer(32),
            Command::Reply(r),
            Command::FreeBuffer(first.buffer),
        ],
        &p,
        vec![],
    );
    let got = completed(&mut w.d, a);
    assert!(matches!(rets(&got)[1], Return::Reply(_)));
    assert_eq!(
        Object::read(&got.copies[0].bytes, 0),
        Some(Object::Binder {
            weak: false,
            flags: 0,
            ptr: A_PTR,
            cookie: A_COOKIE
        })
    );
    let _ = a_idle;
    assert!(w.d.rejects().is_empty(), "{:?}", w.d.rejects());
}

/// One-way calls: TRANSACTION_COMPLETE at once (:3736), sender_pid 0 (:3277), the most recently
/// waiting looper takes the first (`list_add` + `list_first_entry`, :4581, :607), and the second waits
/// on the node until the first buffer is freed (`has_async_transaction`, :2856-2859, :4019-4031).
#[test]
fn oneway_order_and_per_node_serialization() {
    let mut w = world();
    let s1 = waiting_looper(&mut w.d, w.sm);
    let s2 = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    for code in [1, 2] {
        let o = wr(
            &mut w.d,
            a,
            &[Command::Transaction(call(0, code, TF_ONE_WAY, 4, 0))],
            &code.to_ne_bytes(),
            vec![],
        );
        assert_eq!(
            done(&o.read),
            vec![Return::Noop, Return::TransactionComplete]
        );
    }
    let got = completed(&mut w.d, s2);
    let mut first = delivered(SM_PTR, SM_COOKIE, 1, TF_ONE_WAY);
    first.sender_pid = 0;
    first.sender_euid = 10001;
    first.data_size = 4;
    first.offsets = 8;
    assert_eq!(rets(&got), vec![Return::Noop, Return::Transaction(first)]);
    // s1 keeps waiting: call 2 is parked on the node
    assert!(w.d.take_completed().is_empty());

    // freeing call 1's buffer releases call 2, which the freeing looper picks up itself
    let o = wr(&mut w.d, s2, &[Command::FreeBuffer(0)], &[], vec![]);
    let mut second = first;
    second.code = 2;
    // allocated at send time, while call 1 held 0..8 (4 bytes padded to 8)
    second.buffer = 8;
    second.offsets = 16;
    assert_eq!(
        done(&o.read),
        vec![Return::Noop, Return::Transaction(second)]
    );
    assert!(w.d.take_completed().is_empty(), "s1 found nothing to do");
    let _ = s1;
}

/// Strong/weak lifecycle of B's node: refs from the manager, the manager's release, BR_RELEASE +
/// BR_DECREFS to the owner's looper and the node gone (:876-915, :4847-4860).
#[test]
fn refcount_lifecycle() {
    let mut w = world();
    let s = waiting_looper(&mut w.d, w.sm);
    let (_, _) = publish_and_fetch(&mut w, s);
    let b_looper = waiting_looper(&mut w.d, w.b);
    let pc = PtrCookie {
        ptr: 0xB0,
        cookie: 0xB1,
    };
    // A drops its only (strong) count: the manager still holds one, nothing reaches B
    let a = w.d.procs[&w.a].threads.iter().copied().next().unwrap();
    w0(&mut w.d, a, &[Command::Release(1)]);
    assert!(w.d.take_completed().is_empty());
    // the manager drops the last ref: B is told to release and drop its refs
    w0(&mut w.d, s, &[Command::Release(1)]);
    let got = completed(&mut w.d, b_looper);
    assert_eq!(
        rets(&got),
        vec![Return::Noop, Return::Release(pc), Return::DecRefs(pc)]
    );
    assert!(w.d.procs[&w.b].nodes.is_empty());
    // a ref that is gone can not be released again
    w0(&mut w.d, s, &[Command::Release(1)]);
    assert_eq!(w.d.rejects()[&Reject::InvalidRefcount], 1);
}

/// B dies: the manager's death notification fires (`binder_node_release`, :6150-6170); clearing it
/// after delivery completes on BC_DEAD_BINDER_DONE (:4410-4470); A's pending call to B gets
/// BR_DEAD_REPLY after its deferred TRANSACTION_COMPLETE (`binder_release_work` -> `send_failed_reply`).
#[test]
fn death_on_process_exit() {
    let mut w = world();
    let s = waiting_looper(&mut w.d, w.sm);
    let (h, _b) = publish_and_fetch(&mut w, s);
    const COOKIE: u64 = 0xD;
    wr(
        &mut w.d,
        s,
        &[Command::RequestDeathNotification(HandleCookie {
            handle: 1,
            cookie: COOKIE,
        })],
        &[],
        vec![],
    );
    // A calls B; B has no looper, so the call waits in B's process queue
    let a = w.d.procs[&w.a].threads.iter().copied().next().unwrap();
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(h, 9, 0, 0, 0))],
        &[],
        vec![],
    );
    assert_eq!(o.read, Read::Blocked);

    w.d.process_exit(w.b);
    let mut c = w.d.take_completed();
    c.sort_by_key(|(t, _)| *t);
    let by = |t: ThreadId| {
        c.iter()
            .find(|(x, _)| *x == t)
            .map(|(_, o)| decode_returns(&o.returns).unwrap())
            .unwrap()
    };
    assert_eq!(
        by(a),
        vec![Return::Noop, Return::TransactionComplete, Return::DeadReply]
    );
    assert_eq!(by(s), vec![Return::Noop, Return::DeadBinder(COOKIE)]);

    let o = wr(
        &mut w.d,
        s,
        &[
            Command::ClearDeathNotification(HandleCookie {
                handle: 1,
                cookie: COOKIE,
            }),
            Command::DeadBinderDone(COOKIE),
        ],
        &[],
        vec![],
    );
    assert_eq!(
        done(&o.read),
        vec![Return::Noop, Return::ClearDeathNotificationDone(COOKIE)]
    );
    // calls to the dead node: BR_DEAD_REPLY at once
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(h, 9, 0, 0, 0))],
        &[],
        vec![],
    );
    assert_eq!(done(&o.read), vec![Return::Noop, Return::DeadReply]);
    // a fresh request on a dead node fires immediately (:4380-4390)
    wr(
        &mut w.d,
        a,
        &[Command::RequestDeathNotification(HandleCookie {
            handle: h,
            cookie: 0xE,
        })],
        &[],
        vec![],
    );
    assert!(w.d.rejects().is_empty(), "{:?}", w.d.rejects());
}

/// Process exit of a server mid-call: the caller gets BR_DEAD_REPLY (`binder_thread_release`, :5253).
#[test]
fn server_exit_mid_call() {
    let mut w = world();
    let s = waiting_looper(&mut w.d, w.sm);
    let (h, b) = publish_and_fetch(&mut w, s);
    let a = w.d.procs[&w.a].threads.iter().copied().next().unwrap();
    // B serves the call on its thread, now a looper waiting for work
    let o = wr(&mut w.d, b, &[Command::EnterLooper], &[], vec![]);
    assert_eq!(o.read, Read::Blocked);
    wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(h, 9, 0, 0, 0))],
        &[],
        vec![],
    );
    let got = completed(&mut w.d, b);
    assert!(matches!(rets(&got)[1], Return::Transaction(_)));
    w.d.process_exit(w.b);
    let got = completed(&mut w.d, a);
    assert_eq!(
        rets(&got),
        vec![Return::Noop, Return::TransactionComplete, Return::DeadReply]
    );
}

/// The region is the limit: a call larger than the free space, or one-way calls past half the region,
/// fail with BR_FAILED_REPLY (`-ENOSPC`, binder_alloc.c:449, :470-490) and count.
#[test]
fn buffer_exhaustion_fails_the_call() {
    // 128-byte region: 64 bytes of async space
    let mut w = world_with(128, ACCEPT_NONE);
    let _s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    let big = vec![0u8; 200];
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 1, 0, 200, 0))],
        &big,
        vec![],
    );
    assert_eq!(done(&o.read), vec![Return::Noop, Return::FailedReply]);
    assert_eq!(w.d.rejects()[&Reject::NoSpace], 1);
    // 40 + 40 one-way bytes > 64
    let data = vec![0u8; 40];
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 2, TF_ONE_WAY, 40, 0))],
        &data,
        vec![],
    );
    assert_eq!(
        done(&o.read),
        vec![Return::Noop, Return::TransactionComplete]
    );
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 3, TF_ONE_WAY, 40, 0))],
        &data,
        vec![],
    );
    assert_eq!(done(&o.read), vec![Return::Noop, Return::FailedReply]);
    assert_eq!(w.d.rejects()[&Reject::NoSpace], 2);
}

/// Untrusted input: each bad request fails the call with BR_FAILED_REPLY and counts; an unknown BC
/// stops the stream with EINVAL (binder.c:4637-4640); a queued error stops further commands
/// (:4052).
#[test]
fn invalid_input_fails_and_counts() {
    let mut w = world();
    let _s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    let failed = |d: &mut D, cmds: &[Command], p: &[u8], r: Reject| {
        let before = d.rejects().get(&r).copied().unwrap_or(0);
        let o = wr(d, a, cmds, p, vec![]);
        assert_eq!(
            done(&o.read),
            vec![Return::Noop, Return::FailedReply],
            "{r:?}"
        );
        assert_eq!(d.rejects()[&r], before + 1, "{r:?}");
    };
    // no such handle
    failed(
        &mut w.d,
        &[Command::Transaction(call(7, 1, 0, 0, 0))],
        &[],
        Reject::InvalidHandle,
    );
    // data beyond the payload
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 64, 0))],
        &[0; 8],
        Reject::PayloadOutOfBounds,
    );
    // offsets size not a multiple of 8
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 8, 4))],
        &[0; 12],
        Reject::InvalidOffsets,
    );
    // an object running past the data: a 24-byte handle record at offset 8 of 24 bytes
    let mut p = vec![0u8; 24];
    p[8..12].copy_from_slice(&BINDER_TYPE_HANDLE.to_ne_bytes());
    p.extend_from_slice(&8u64.to_ne_bytes());
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 24, 8))],
        &p,
        Reject::InvalidObject,
    );
    // a scatter-gather object
    let mut p = vec![0u8; 40];
    p[..4].copy_from_slice(&BINDER_TYPE_PTR.to_ne_bytes());
    p.extend_from_slice(&0u64.to_ne_bytes());
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 40, 8))],
        &p,
        Reject::UnsupportedObject,
    );
    // a handle A does not hold, inside the data
    let (p, os) = payload(
        24,
        &[(
            0,
            Object::Handle {
                weak: false,
                flags: 0,
                handle: 5,
                cookie: 0,
            },
        )],
    );
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 24, os))],
        &p,
        Reject::InvalidHandle,
    );
    // an fd to a manager that does not accept fds
    let (p, os) = payload(24, &[(0, Object::Fd { fd: 3, cookie: 0 })]);
    failed(
        &mut w.d,
        &[Command::Transaction(call(0, 1, 0, 24, os))],
        &p,
        Reject::FdsNotAccepted,
    );
    // reply with nothing to reply to
    failed(
        &mut w.d,
        &[Command::Reply(TransactionData::default())],
        &[],
        Reject::ReplyWithoutTransaction,
    );

    // after a failed call the rest of the stream waits until the error is read: consumed = the call
    let commands = stream(&[
        Command::Transaction(call(7, 1, 0, 0, 0)),
        Command::IncRefs(0),
    ]);
    let o =
        w.d.write_read(
            a,
            WriteRead {
                commands: &commands,
                payload: &[],
                read_size: READ,
                fds: vec![],
            },
        )
        .unwrap();
    assert_eq!(o.write_consumed, 4 + 64);

    // an unknown command: EINVAL, consumed up to it, counted
    let mut commands = stream(&[Command::IncRefs(0)]);
    commands.extend_from_slice(&0x1234_5678u32.to_ne_bytes());
    let e =
        w.d.write_read(
            a,
            WriteRead {
                commands: &commands,
                payload: &[],
                read_size: READ,
                fds: vec![],
            },
        )
        .unwrap_err();
    assert_eq!(
        e,
        WriteError {
            errno: Errno::Inval,
            write_consumed: 8
        }
    );
    assert_eq!(w.d.rejects()[&Reject::UnknownCommand], 1);
    // a truncated command: EFAULT
    let commands = &stream(&[Command::Acquire(1)])[..6];
    let e =
        w.d.write_read(
            a,
            WriteRead {
                commands,
                payload: &[],
                read_size: READ,
                fds: vec![],
            },
        )
        .unwrap_err();
    assert_eq!(e.errno, Errno::Fault);
    assert_eq!(w.d.rejects()[&Reject::TruncatedCommand], 1);
    // refcount, free and death commands that match nothing
    wr(
        &mut w.d,
        a,
        &[
            Command::Release(9),
            Command::FreeBuffer(0x40),
            Command::DeadBinderDone(1),
            Command::IncRefsDone(PtrCookie { ptr: 1, cookie: 1 }),
            Command::RegisterLooper,
        ],
        &[],
        vec![],
    );
    for r in [
        Reject::InvalidRefcount,
        Reject::InvalidFreeBuffer,
        Reject::InvalidDeath,
        Reject::InvalidNodeDone,
        Reject::InvalidLooper,
    ] {
        assert_eq!(w.d.rejects()[&r], 1, "{r:?}");
    }
    // a second request on a thread blocked in its read
    let s = w.d.procs[&w.sm].threads.iter().copied().last().unwrap();
    let e =
        w.d.write_read(
            s,
            WriteRead {
                commands: &[],
                payload: &[],
                read_size: READ,
                fds: vec![],
            },
        )
        .unwrap_err();
    assert_eq!(e.errno, Errno::Busy);
    assert_eq!(w.d.rejects()[&Reject::ThreadBusy], 1);
}

/// FD objects to a node created with FLAT_BINDER_FLAG_ACCEPTS_FDS: tokens travel in object order and
/// come out with the region offset of the fd field (binder.c:2345-2390); a missing token fails
/// (`-EBADF`), a spare one counts.
#[test]
fn fds_follow_objects() {
    let mut w = world_with(REGION, FLAT_BINDER_FLAG_ACCEPTS_FDS);
    let s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    let (p, os) = payload(
        48,
        &[
            (0, Object::Fd { fd: 3, cookie: 0 }),
            (24, Object::Fd { fd: 4, cookie: 0 }),
        ],
    );
    wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 1, TF_ACCEPT_FDS, 48, os))],
        &p,
        vec![33, 44],
    );
    let got = completed(&mut w.d, s);
    // buffer at 0; binder_fd_object.fd at +8
    assert_eq!(
        got.fds,
        vec![
            FdFixup { offset: 8, fd: 33 },
            FdFixup { offset: 32, fd: 44 }
        ]
    );
    // the reply may carry fds because the call said TF_ACCEPT_FDS
    let (p, os) = payload(24, &[(0, Object::Fd { fd: 5, cookie: 0 })]);
    let r = TransactionData {
        data_size: 24,
        offsets_size: os,
        offsets: 24,
        ..Default::default()
    };
    wr(&mut w.d, s, &[Command::Reply(r)], &p, vec![55, 66]);
    assert_eq!(w.d.rejects()[&Reject::UnusedFds], 1);
    let got = completed(&mut w.d, a);
    assert_eq!(got.fds, vec![FdFixup { offset: 8, fd: 55 }]);
    // an fd object without a token
    let (p, os) = payload(24, &[(0, Object::Fd { fd: 3, cookie: 0 })]);
    let o = wr(
        &mut w.d,
        a,
        &[Command::Transaction(call(0, 1, 0, 24, os))],
        &p,
        vec![],
    );
    assert_eq!(done(&o.read), vec![Return::Noop, Return::FailedReply]);
    assert_eq!(w.d.rejects()[&Reject::MissingFd], 1);
}

/// Thread pool: BR_SPAWN_LOOPER replaces the leading BR_NOOP when no looper is waiting and the pool
/// has room (binder.c:5079-5097); BC_REGISTER_LOOPER consumes the request (:4266-4280).
#[test]
fn spawn_looper_requests() {
    let mut w = world();
    let s = w.d.add_thread(w.sm).unwrap();
    // libbinder's default pool: 15 (ProcessState.cpp:48)
    w.d.set_max_threads(s, 15).unwrap();
    let o = wr(&mut w.d, s, &[Command::EnterLooper], &[], vec![]);
    assert_eq!(done(&o.read), vec![Return::SpawnLooper]);
    // the request is outstanding: no second one
    let t = w.d.add_thread(w.sm).unwrap();
    let o = wr(&mut w.d, t, &[Command::EnterLooper], &[], vec![]);
    assert_eq!(done(&o.read), vec![Return::Noop]);
    // the spawned thread registers; s waits, so nobody else is asked for
    let o = wr(&mut w.d, s, &[], &[], vec![]);
    assert_eq!(o.read, Read::Blocked);
    let n = w.d.add_thread(w.sm).unwrap();
    let o = wr(&mut w.d, n, &[Command::RegisterLooper], &[], vec![]);
    assert_eq!(done(&o.read), vec![Return::Noop]);
    assert!(w.d.rejects().is_empty(), "{:?}", w.d.rejects());
}

/// The remaining rejections: an unimplemented BC (`BC_TRANSACTION_SG`), a call to one's own process
/// (binder.c:3148-3156), a second sync call while the first awaits its reply (:3192-3205), a binder
/// object whose cookie differs from its node's (:2234-2240).
#[test]
fn protocol_misuse_counts() {
    let mut w = world();
    let _s = waiting_looper(&mut w.d, w.sm);
    let a = thread(&mut w.d, w.a, false);
    let commands = stream(&[Command::Unsupported(BC_TRANSACTION_SG)]);
    let e =
        w.d.write_read(
            a,
            WriteRead {
                commands: &commands,
                payload: &[],
                read_size: 0,
                fds: vec![],
            },
        )
        .unwrap_err();
    assert_eq!(e.errno, Errno::Inval);
    assert_eq!(w.d.rejects()[&Reject::UnsupportedCommand], 1);

    // the manager calling handle 0 is calling itself
    let m = thread(&mut w.d, w.sm, false);
    let o = wr(
        &mut w.d,
        m,
        &[Command::Transaction(call(0, 1, 0, 0, 0))],
        &[],
        vec![],
    );
    assert_eq!(done(&o.read), vec![Return::Noop, Return::FailedReply]);
    assert_eq!(w.d.rejects()[&Reject::SelfTransaction], 1);

    // same node pointer, other cookie: rejected, the first call goes through
    let obj = |cookie| Object::Binder {
        weak: false,
        flags: 0,
        ptr: 0xA0,
        cookie,
    };
    let (p1, os) = payload(24, &[(0, obj(1))]);
    let (p2, _) = payload(24, &[(0, obj(2))]);
    let mut p = p1;
    p.extend_from_slice(&p2);
    let second = TransactionData {
        buffer: 32,
        offsets: 56,
        ..call(0, 2, TF_ONE_WAY, 24, os)
    };
    let o = wr(
        &mut w.d,
        a,
        &[
            Command::Transaction(call(0, 1, TF_ONE_WAY, 24, os)),
            Command::Transaction(second),
        ],
        &p,
        vec![],
    );
    let pc = PtrCookie {
        ptr: 0xA0,
        cookie: 1,
    };
    assert_eq!(
        done(&o.read),
        vec![
            Return::Noop,
            Return::IncRefs(pc),
            Return::Acquire(pc),
            Return::TransactionComplete,
            Return::FailedReply
        ]
    );
    assert_eq!(w.d.rejects()[&Reject::CookieMismatch], 1);

    // a sync call sent write-only, then another before its reply: the first call's deferred
    // TRANSACTION_COMPLETE sits ahead of the error in the thread's queue
    let c = thread(&mut w.d, w.a, false);
    w0(&mut w.d, c, &[Command::Transaction(call(0, 1, 0, 0, 0))]);
    let o = wr(
        &mut w.d,
        c,
        &[Command::Transaction(call(0, 2, 0, 0, 0))],
        &[],
        vec![],
    );
    assert_eq!(
        done(&o.read),
        vec![
            Return::Noop,
            Return::TransactionComplete,
            Return::FailedReply
        ]
    );
    assert_eq!(w.d.rejects()[&Reject::BadTransactionStack], 1);
}

#[test]
fn sender_pid_follows_pid_namespaces() {
    let host = tag(1000, 100, PidNs::Host);
    let a = tag(10001, 200, PidNs::Sandbox { id: 1, pid: 2 });
    let a2 = tag(10001, 201, PidNs::Sandbox { id: 1, pid: 3 });
    let b = tag(10002, 300, PidNs::Sandbox { id: 2, pid: 2 });
    // the host's namespace sees everyone by host pid; a sandbox sees only its members, by their
    // namespace pid; others are 0 (task_tgid_nr_ns)
    assert_eq!(a.pid_seen_by(&host), 200);
    assert_eq!(host.pid_seen_by(&a), 0);
    assert_eq!(a2.pid_seen_by(&a), 3);
    assert_eq!(b.pid_seen_by(&a), 0);
}
