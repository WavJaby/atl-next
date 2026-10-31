// SPDX-License-Identifier: MIT
//! binderd core: the binder driver as a pure, synchronous state machine. Processes (tagged by the broker
//! with uid / host pid / pid namespace), threads (one per connection), nodes, per-process handle tables,
//! refcounts, transactions, the thread pool and death notifications, reproducing the Linux driver's
//! observable behaviour (`drivers/android/binder.c`, v6.12, cited by function and line).
//!
//! Shape: every `BINDER_WRITE_READ` is one `write_read` call. A read that would block in the kernel
//! returns `Read::Blocked`; when work arrives for that thread later, its read runs then and the result
//! is collected with `take_completed`. No threads, no clocks, no I/O: the same calls give the same
//! bytes. Transaction payloads travel inline (the client shim turns pointers into offsets into
//! `WriteRead::payload`); binderd allocates each buffer's offset in the receiver's region and hands the
//! bytes back as `RegionCopy` for the receiver's shim to place. File descriptors are opaque tokens `F`
//! consumed in object order and delivered with the region offset their number goes to.
//!
//! All input is untrusted: every rejection is counted (`Reject`), as the kernel logs them.
#![forbid(unsafe_code)]

mod alloc;
mod lifecycle;
mod transact;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use atl_binder_wire::{
    MAX_RETURN_SIZE, PtrCookie, Return, TransactionData, Uptr, decode_command, uapi,
};

use alloc::Alloc;
pub use alloc::MAX_REGION_SIZE;

/// A process connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcId(u32);

/// A thread connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NodeId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TxnId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DeathId(u32);

/// Where a process's pids are numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidNs {
    /// The host's namespace (atld).
    Host,
    /// A sandbox's own namespace (one per sandbox, child of the host's): its id and the process's pid
    /// inside it.
    Sandbox { id: u64, pid: i32 },
}

/// The broker's identity tag for a process: the kernel's `task_euid` / `task_tgid_nr_ns` inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub uid: u32,
    pub host_pid: i32,
    pub pid_ns: PidNs,
}

impl Tag {
    /// `sender_pid` as `receiver` sees this process (`task_tgid_nr_ns(sender, receiver's ns)`): the host
    /// sees every pid, a sandbox only its own members; 0 when invisible.
    fn pid_seen_by(&self, receiver: &Tag) -> i32 {
        match (receiver.pid_ns, self.pid_ns) {
            (PidNs::Host, _) => self.host_pid,
            (PidNs::Sandbox { id: r, .. }, PidNs::Sandbox { id: s, pid }) if r == s => pid,
            _ => 0,
        }
    }
}

/// A new process connection.
#[derive(Debug, Clone, Copy)]
pub struct ProcSpec {
    pub tag: Tag,
    /// The size the process mapped (capped at `MAX_REGION_SIZE` as the kernel caps the mapping).
    pub region_size: u64,
    /// Only atld's ServiceManager may become the context manager; replaces the kernel's SELinux check.
    pub may_be_context_manager: bool,
}

/// An ioctl failure, as the kernel would return it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    Inval,
    Perm,
    Busy,
    Fault,
}

/// Every rejected input, counted. Kernel equivalents are `binder_user_error` / `binder_txn_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reject {
    /// Not a BC code (`-EINVAL`, stream processing stops).
    UnknownCommand,
    /// A uapi BC binderd does not implement (scatter-gather, attempt-acquire, freeze notifications).
    UnsupportedCommand,
    /// The command stream ends inside a command (`-EFAULT`).
    TruncatedCommand,
    /// BC_INCREFS/ACQUIRE/RELEASE/DECREFS on a missing ref, or a count that would go below zero.
    InvalidRefcount,
    /// BC_INCREFS_DONE/ACQUIRE_DONE for an unknown node, wrong cookie, or nothing pending.
    InvalidNodeDone,
    /// BC_FREE_BUFFER of an offset that is no delivered buffer.
    InvalidFreeBuffer,
    /// BC_REGISTER_LOOPER without a spawn request or after ENTER, or ENTER after REGISTER.
    InvalidLooper,
    /// Death-notification request/clear/done that matches nothing.
    InvalidDeath,
    /// Transaction to a handle the sender holds no strong ref for.
    InvalidHandle,
    /// Transaction from a process to itself.
    SelfTransaction,
    /// New sync transaction while waiting for a reply, or one with a transaction queued first.
    BadTransactionStack,
    /// BC_REPLY with nothing to reply to.
    ReplyWithoutTransaction,
    /// Data or offsets outside the request's payload (`-EFAULT`).
    PayloadOutOfBounds,
    /// Offsets size not a multiple of `binder_size_t`, or offsets not ascending.
    InvalidOffsets,
    /// An offset that holds no valid flat object.
    InvalidObject,
    /// A scatter-gather object (PTR / FDA) or unknown object type.
    UnsupportedObject,
    /// A binder object whose cookie differs from its node's.
    CookieMismatch,
    /// FD object for a target that does not accept fds (`-EPERM`).
    FdsNotAccepted,
    /// FD object without a passed fd token (`-EBADF`).
    MissingFd,
    /// Fd tokens left over after the write stream.
    UnusedFds,
    /// The receiver's region is full, or its async half (`-ENOSPC`).
    NoSpace,
    /// SET_CONTEXT_MGR from a process not allowed to.
    ContextManagerDenied,
    /// SET_CONTEXT_MGR while one exists (`-EBUSY`).
    ContextManagerBusy,
    /// A request on a thread already blocked in a read.
    ThreadBusy,
}

/// One `BINDER_WRITE_READ` request.
#[derive(Debug)]
pub struct WriteRead<'a, F> {
    /// BC stream (`write_buffer[..write_size]`).
    pub commands: &'a [u8],
    /// Transaction data and offsets arrays; `data.ptr.buffer` / `.offsets` index into it.
    pub payload: &'a [u8],
    /// `read_size`: capacity for the BR stream.
    pub read_size: usize,
    /// Fd tokens, one per `BINDER_TYPE_FD` object, in stream and object order.
    pub fds: Vec<F>,
}

/// Bytes the receiving process places in its own region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionCopy {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// A delivered fd: install it and write its number at region `offset`.
#[derive(Debug, PartialEq, Eq)]
pub struct FdFixup<F> {
    pub offset: u64,
    pub fd: F,
}

/// What a read returns.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadOut<F> {
    /// BR stream (`read_consumed` = its length).
    pub returns: Vec<u8>,
    pub copies: Vec<RegionCopy>,
    pub fds: Vec<FdFixup<F>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Read<F> {
    /// `read_size` was 0.
    Skipped,
    Done(ReadOut<F>),
    /// The kernel would sleep; the result comes from `take_completed`.
    Blocked,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Outcome<F> {
    pub write_consumed: usize,
    pub read: Read<F>,
}

/// A write that failed as an ioctl: the error and how much of the stream was consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError {
    pub errno: Errno,
    pub write_consumed: usize,
}

// Looper state bits (kernel `BINDER_LOOPER_STATE_*`, binder.c:253-259).
const LOOPER_REGISTERED: u8 = 1;
const LOOPER_ENTERED: u8 = 2;
const LOOPER_EXITED: u8 = 4;
const LOOPER_INVALID: u8 = 8;

/// Which per-thread error slot a queued error came from (kernel `return_error` / `reply_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorSlot {
    Return,
    Reply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    Transaction(TxnId),
    TransactionComplete,
    Error(u32, ErrorSlot),
    Node(NodeId),
    Death(DeathId),
}

struct Proc {
    tag: Tag,
    may_be_context_manager: bool,
    alloc: Alloc,
    threads: BTreeSet<ThreadId>,
    /// Threads blocked waiting for process work, most recent first (kernel `list_add` at the head,
    /// `binder_select_thread_ilocked` takes the first, binder.c:607-619).
    waiting: VecDeque<ThreadId>,
    todo: VecDeque<Work>,
    nodes: BTreeMap<Uptr, NodeId>,
    refs: BTreeMap<u32, Ref>,
    ref_by_node: BTreeMap<NodeId, u32>,
    buffers: BTreeMap<u64, Buffer>,
    delivered_death: Vec<DeathId>,
    max_threads: u32,
    requested_threads: u32,
    requested_threads_started: u32,
}

struct Thread {
    proc: ProcId,
    looper: u8,
    /// Kernel `looper_need_return`: set for a new thread, cleared after each ioctl; the first read of a
    /// new thread returns without waiting.
    need_return: bool,
    todo: VecDeque<Work>,
    /// Kernel `process_todo`: deferred work (a sync call's TRANSACTION_COMPLETE) does not wake a read.
    process_todo: bool,
    stack: Option<TxnId>,
    /// A `return_error` is queued: the write loop stops until it is read (binder.c:4052).
    return_error_pending: bool,
    /// Blocked in a read of this capacity.
    blocked: Option<usize>,
}

struct Node {
    proc: Option<ProcId>,
    ptr: Uptr,
    cookie: Uptr,
    accept_fds: bool,
    internal_strong: u32,
    local_strong: u32,
    local_weak: u32,
    has_strong: bool,
    has_weak: bool,
    pending_strong: bool,
    pending_weak: bool,
    /// (proc, handle) of every ref to it.
    refs: BTreeSet<(ProcId, u32)>,
    /// The node's work item sits in some todo list.
    queued: bool,
    has_async: bool,
    async_todo: VecDeque<TxnId>,
}

struct Ref {
    node: NodeId,
    strong: u32,
    weak: u32,
    death: Option<DeathId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeathKind {
    DeadBinder,
    DeadBinderAndClear,
    Clear,
}

struct Death {
    proc: ProcId,
    cookie: Uptr,
    kind: DeathKind,
    queued: bool,
}

/// What a buffer holds a reference on until it is freed (kernel `binder_transaction_buffer_release`).
#[derive(Debug, Clone, Copy)]
enum Held {
    Node { node: NodeId, strong: bool },
    Ref { handle: u32, strong: bool },
}

struct Buffer {
    is_async: bool,
    /// Holds a local strong ref on it (`binder_get_node_refs_for_txn`, binder.c:2948).
    target_node: Option<NodeId>,
    txn: Option<TxnId>,
    allow_user_free: bool,
    held: Vec<Held>,
}

struct Txn<F> {
    from: Option<ThreadId>,
    sender: Tag,
    to_proc: ProcId,
    to_thread: Option<ThreadId>,
    from_parent: Option<TxnId>,
    to_parent: Option<TxnId>,
    is_reply: bool,
    code: u32,
    flags: u32,
    data_size: u64,
    offsets_size: u64,
    /// Region offset of its buffer in `to_proc`; `None` once the buffer was released.
    buffer: Option<u64>,
    /// Translated data, padding, offsets: the buffer's contents.
    bytes: Vec<u8>,
    /// (region offset of the fd number, token)
    fds: Vec<(u64, F)>,
}

/// The driver state.
pub struct Binderd<F> {
    procs: BTreeMap<ProcId, Proc>,
    threads: BTreeMap<ThreadId, Thread>,
    nodes: BTreeMap<NodeId, Node>,
    txns: BTreeMap<TxnId, Txn<F>>,
    deaths: BTreeMap<DeathId, Death>,
    context_manager: Option<NodeId>,
    next_id: u32,
    /// Blocked threads that got work, in wake order.
    woken: VecDeque<ThreadId>,
    completed: Vec<(ThreadId, ReadOut<F>)>,
    rejects: BTreeMap<Reject, u64>,
}

impl<F> Default for Binderd<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> Binderd<F> {
    pub fn new() -> Self {
        Self {
            procs: BTreeMap::new(),
            threads: BTreeMap::new(),
            nodes: BTreeMap::new(),
            txns: BTreeMap::new(),
            deaths: BTreeMap::new(),
            context_manager: None,
            next_id: 1,
            woken: VecDeque::new(),
            completed: Vec::new(),
            rejects: BTreeMap::new(),
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Counted rejections so far.
    pub fn rejects(&self) -> &BTreeMap<Reject, u64> {
        &self.rejects
    }

    fn reject(&mut self, r: Reject) {
        *self.rejects.entry(r).or_default() += 1;
    }

    pub fn add_proc(&mut self, spec: ProcSpec) -> ProcId {
        let id = ProcId(self.id());
        self.procs.insert(
            id,
            Proc {
                tag: spec.tag,
                may_be_context_manager: spec.may_be_context_manager,
                alloc: Alloc::new(spec.region_size),
                threads: BTreeSet::new(),
                waiting: VecDeque::new(),
                todo: VecDeque::new(),
                nodes: BTreeMap::new(),
                refs: BTreeMap::new(),
                ref_by_node: BTreeMap::new(),
                buffers: BTreeMap::new(),
                delivered_death: Vec::new(),
                max_threads: 0,
                requested_threads: 0,
                requested_threads_started: 0,
            },
        );
        id
    }

    pub fn add_thread(&mut self, proc: ProcId) -> Option<ThreadId> {
        let id = ThreadId(self.id());
        self.procs.get_mut(&proc)?.threads.insert(id);
        self.threads.insert(
            id,
            Thread {
                proc,
                looper: 0,
                need_return: true,
                todo: VecDeque::new(),
                process_todo: false,
                stack: None,
                return_error_pending: false,
                blocked: None,
            },
        );
        Some(id)
    }

    /// `BINDER_SET_MAX_THREADS`.
    pub fn set_max_threads(&mut self, thread: ThreadId, max: u32) -> Result<(), Errno> {
        let proc = self.threads.get(&thread).ok_or(Errno::Inval)?.proc;
        self.proc_mut(proc).max_threads = max;
        Ok(())
    }

    /// `BINDER_SET_CONTEXT_MGR_EXT` (legacy `SET_CONTEXT_MGR` = ptr 0, cookie 0, flags 0;
    /// `binder_ioctl_set_ctx_mgr`, binder.c:5428).
    pub fn set_context_manager(
        &mut self,
        thread: ThreadId,
        ptr: Uptr,
        cookie: Uptr,
        flags: u32,
    ) -> Result<(), Errno> {
        let proc = self.threads.get(&thread).ok_or(Errno::Inval)?.proc;
        if self.context_manager.is_some() {
            self.reject(Reject::ContextManagerBusy);
            return Err(Errno::Busy);
        }
        if !self.proc(proc).may_be_context_manager {
            self.reject(Reject::ContextManagerDenied);
            return Err(Errno::Perm);
        }
        let node = self.new_node(proc, ptr, cookie, flags);
        let n = self.node_mut(node);
        // pinned for the manager's lifetime: these local refs are never dropped
        n.local_strong += 1;
        n.local_weak += 1;
        n.has_strong = true;
        n.has_weak = true;
        self.context_manager = Some(node);
        Ok(())
    }

    /// `BINDER_WRITE_READ` (`binder_ioctl_write_read`).
    pub fn write_read(
        &mut self,
        thread: ThreadId,
        req: WriteRead<'_, F>,
    ) -> Result<Outcome<F>, WriteError> {
        let Some(t) = self.threads.get(&thread) else {
            return Err(WriteError {
                errno: Errno::Inval,
                write_consumed: 0,
            });
        };
        if t.blocked.is_some() {
            self.reject(Reject::ThreadBusy);
            return Err(WriteError {
                errno: Errno::Busy,
                write_consumed: 0,
            });
        }
        let proc = t.proc;
        let mut fds = req.fds.into_iter();
        let written = self.thread_write(thread, req.commands, req.payload, &mut fds);
        if fds.next().is_some() {
            self.reject(Reject::UnusedFds);
        }
        let result = match written {
            Err(e) => Err(e),
            Ok(write_consumed) => {
                let read = if req.read_size == 0 {
                    Read::Skipped
                } else {
                    match self.thread_read(thread, req.read_size) {
                        Some(out) => Read::Done(out),
                        None => {
                            self.block(thread, req.read_size);
                            Read::Blocked
                        }
                    }
                };
                // leftover process work: wake another waiting thread (binder_ioctl_write_read)
                if self.procs.get(&proc).is_some_and(|p| !p.todo.is_empty()) {
                    self.wakeup_proc(proc);
                }
                Ok(Outcome {
                    write_consumed,
                    read,
                })
            }
        };
        if let Some(t) = self.threads.get_mut(&thread) {
            t.need_return = false;
        }
        self.run_woken();
        result
    }

    /// Reads that completed for threads that were blocked, in completion order.
    pub fn take_completed(&mut self) -> Vec<(ThreadId, ReadOut<F>)> {
        std::mem::take(&mut self.completed)
    }

    fn proc(&self, p: ProcId) -> &Proc {
        &self.procs[&p]
    }
    fn proc_mut(&mut self, p: ProcId) -> &mut Proc {
        self.procs.get_mut(&p).expect("live proc")
    }
    fn thread_mut(&mut self, t: ThreadId) -> &mut Thread {
        self.threads.get_mut(&t).expect("live thread")
    }
    fn node_mut(&mut self, n: NodeId) -> &mut Node {
        self.nodes.get_mut(&n).expect("live node")
    }

    fn new_node(&mut self, proc: ProcId, ptr: Uptr, cookie: Uptr, flags: u32) -> NodeId {
        let id = NodeId(self.id());
        self.nodes.insert(
            id,
            Node {
                proc: Some(proc),
                ptr,
                cookie,
                accept_fds: flags & uapi::FLAT_BINDER_FLAG_ACCEPTS_FDS != 0,
                internal_strong: 0,
                local_strong: 0,
                local_weak: 0,
                has_strong: false,
                has_weak: false,
                pending_strong: false,
                pending_weak: false,
                refs: BTreeSet::new(),
                queued: false,
                has_async: false,
                async_todo: VecDeque::new(),
            },
        );
        self.proc_mut(proc).nodes.insert(ptr, id);
        id
    }

    // ---- queues and wakeups ------------------------------------------------------------------------

    /// `binder_enqueue_thread_work_ilocked`: queued, and the thread's read may return for it.
    fn enqueue_thread(&mut self, t: ThreadId, w: Work) {
        let th = self.thread_mut(t);
        th.todo.push_back(w);
        th.process_todo = true;
        self.wake(t);
    }

    /// `binder_enqueue_deferred_thread_work_ilocked`: queued without waking the read.
    fn enqueue_thread_deferred(&mut self, t: ThreadId, w: Work) {
        self.thread_mut(t).todo.push_back(w);
    }

    fn enqueue_proc(&mut self, p: ProcId, w: Work) {
        self.proc_mut(p).todo.push_back(w);
        self.wakeup_proc(p);
    }

    fn wake(&mut self, t: ThreadId) {
        if self.threads[&t].blocked.is_some() && !self.woken.contains(&t) {
            self.woken.push_back(t);
        }
    }

    /// `binder_select_thread_ilocked`: the most recently waiting thread, removed from the waiting list.
    fn select_thread(&mut self, p: ProcId) -> Option<ThreadId> {
        self.proc_mut(p).waiting.pop_front()
    }

    fn wakeup_proc(&mut self, p: ProcId) {
        if let Some(t) = self.select_thread(p) {
            self.wake(t);
        }
    }

    fn block(&mut self, t: ThreadId, read_size: usize) {
        let proc_work = self.available_for_proc_work(t);
        let th = self.thread_mut(t);
        th.blocked = Some(read_size);
        let p = th.proc;
        if proc_work {
            self.proc_mut(p).waiting.push_front(t);
        }
    }

    fn run_woken(&mut self) {
        while let Some(t) = self.woken.pop_front() {
            let Some(read_size) = self.threads.get(&t).and_then(|th| th.blocked) else {
                continue;
            };
            let p = self.threads[&t].proc;
            self.thread_mut(t).blocked = None;
            self.proc_mut(p).waiting.retain(|w| *w != t);
            match self.thread_read(t, read_size) {
                Some(out) => {
                    self.completed.push((t, out));
                    if !self.proc(p).todo.is_empty() {
                        self.wakeup_proc(p);
                    }
                }
                None => self.block(t, read_size),
            }
        }
    }

    // ---- read (binder_thread_read, binder.c:4652) --------------------------------------------------

    /// `binder_available_for_proc_work_ilocked` (binder.c:570).
    fn available_for_proc_work(&self, t: ThreadId) -> bool {
        let th = &self.threads[&t];
        th.stack.is_none()
            && th.todo.is_empty()
            && th.looper & (LOOPER_REGISTERED | LOOPER_ENTERED) != 0
    }

    /// The read, or `None` where the kernel would sleep.
    fn thread_read(&mut self, t: ThreadId, read_size: usize) -> Option<ReadOut<F>> {
        let proc_work = self.available_for_proc_work(t);
        let p = self.threads[&t].proc;
        // binder_has_work_ilocked (binder.c:550): deferred work alone does not end the wait
        let th = &self.threads[&t];
        let has_work =
            th.process_todo || th.need_return || (proc_work && !self.proc(p).todo.is_empty());
        if !has_work {
            return None;
        }
        let mut out = ReadOut {
            returns: Vec::new(),
            copies: Vec::new(),
            fds: Vec::new(),
        };
        // every read starts with BR_NOOP (binder.c:4664); BR_SPAWN_LOOPER may overwrite it below
        if read_size >= atl_binder_wire::CODE_SIZE {
            Return::Noop.encode(&mut out.returns);
        }
        loop {
            let th = &self.threads[&t];
            let from_thread = !th.todo.is_empty();
            if !from_thread && !(proc_work && !self.proc(p).todo.is_empty()) {
                if out.returns.len() == atl_binder_wire::CODE_SIZE && !th.need_return {
                    // nothing but the BR_NOOP: back to sleep
                    return None;
                }
                break;
            }
            // stop while the largest item might not fit (binder.c:4728)
            if read_size.saturating_sub(out.returns.len()) < MAX_RETURN_SIZE {
                break;
            }
            let w = if from_thread {
                let th = self.thread_mut(t);
                let w = th.todo.pop_front();
                if th.todo.is_empty() {
                    th.process_todo = false;
                }
                w
            } else {
                self.proc_mut(p).todo.pop_front()
            }
            .expect("checked non-empty");
            match w {
                Work::TransactionComplete => {
                    Return::TransactionComplete.encode(&mut out.returns);
                }
                Work::Error(cmd, slot) => {
                    if slot == ErrorSlot::Return {
                        self.thread_mut(t).return_error_pending = false;
                    }
                    out.returns.extend_from_slice(&cmd.to_ne_bytes());
                }
                Work::Node(n) => self.read_node_work(n, &mut out.returns),
                Work::Death(d) => {
                    let death = &self.deaths[&d];
                    let cookie = death.cookie;
                    let kind = death.kind;
                    if kind == DeathKind::Clear {
                        self.deaths.remove(&d);
                        Return::ClearDeathNotificationDone(cookie).encode(&mut out.returns);
                    } else {
                        self.deaths.get_mut(&d).expect("live death").queued = false;
                        self.proc_mut(p).delivered_death.push(d);
                        Return::DeadBinder(cookie).encode(&mut out.returns);
                        // a death notification can cause transactions: return now (binder.c:4900)
                        break;
                    }
                }
                Work::Transaction(id) => {
                    self.deliver(t, id, &mut out);
                    // one transaction per read
                    break;
                }
            }
        }
        // binder.c:5079-5097: ask for another looper when none is waiting and the pool has room
        let proc = self.proc(p);
        let looper = self.threads[&t].looper & (LOOPER_REGISTERED | LOOPER_ENTERED) != 0;
        if proc.requested_threads == 0
            && proc.waiting.is_empty()
            && proc.requested_threads_started < proc.max_threads
            && looper
            && out.returns.len() >= atl_binder_wire::CODE_SIZE
        {
            self.proc_mut(p).requested_threads += 1;
            out.returns[..atl_binder_wire::CODE_SIZE]
                .copy_from_slice(&uapi::BR_SPAWN_LOOPER.to_ne_bytes());
        }
        Some(out)
    }

    /// `BINDER_WORK_NODE` (binder.c:4777): tell the owner which refs it must now hold or may drop.
    fn read_node_work(&mut self, id: NodeId, out: &mut Vec<u8>) {
        let n = self.node_mut(id);
        n.queued = false;
        let strong = n.internal_strong > 0 || n.local_strong > 0;
        let weak = !n.refs.is_empty() || n.local_weak > 0 || strong;
        let (has_strong, has_weak) = (n.has_strong, n.has_weak);
        let pc = PtrCookie {
            ptr: n.ptr,
            cookie: n.cookie,
        };
        if weak && !has_weak {
            n.has_weak = true;
            n.pending_weak = true;
            n.local_weak += 1;
        }
        if strong && !has_strong {
            n.has_strong = true;
            n.pending_strong = true;
            n.local_strong += 1;
        }
        if !strong && has_strong {
            n.has_strong = false;
        }
        if !weak && has_weak {
            n.has_weak = false;
        }
        if !weak && !strong {
            let proc = n.proc;
            let ptr = n.ptr;
            self.nodes.remove(&id);
            if let Some(p) = proc {
                self.proc_mut(p).nodes.remove(&ptr);
            }
        }
        if weak && !has_weak {
            Return::IncRefs(pc).encode(out);
        }
        if strong && !has_strong {
            Return::Acquire(pc).encode(out);
        }
        if !strong && has_strong {
            Return::Release(pc).encode(out);
        }
        if !weak && has_weak {
            Return::DecRefs(pc).encode(out);
        }
    }

    /// BR_TRANSACTION / BR_REPLY for `id` (binder.c:4925-5060).
    fn deliver(&mut self, t: ThreadId, id: TxnId, out: &mut ReadOut<F>) {
        let p = self.threads[&t].proc;
        let mut txn = self.txns.remove(&id).expect("queued txn");
        let receiver = self.proc(p).tag;
        let sender_pid = match txn.from {
            Some(from) if self.threads.contains_key(&from) => txn.sender.pid_seen_by(&receiver),
            _ => 0,
        };
        let buffer = txn.buffer.expect("delivered txn has a buffer");
        let target_node = self.proc(p).buffers[&buffer].target_node;
        let (target, cookie) = match target_node {
            Some(n) => (self.nodes[&n].ptr, self.nodes[&n].cookie),
            None => (0, 0),
        };
        let data = TransactionData {
            target,
            cookie,
            code: txn.code,
            flags: txn.flags,
            sender_pid,
            sender_euid: txn.sender.uid,
            data_size: txn.data_size,
            offsets_size: txn.offsets_size,
            buffer,
            offsets: buffer + alloc::align(txn.data_size).expect("checked at send"),
        };
        if target_node.is_some() {
            Return::Transaction(data).encode(&mut out.returns);
        } else {
            Return::Reply(data).encode(&mut out.returns);
        }
        out.copies.push(RegionCopy {
            offset: buffer,
            bytes: std::mem::take(&mut txn.bytes),
        });
        out.fds
            .extend(txn.fds.drain(..).map(|(offset, fd)| FdFixup { offset, fd }));
        let sync_call = !txn.is_reply && txn.flags & uapi::TF_ONE_WAY == 0;
        let b = self
            .proc_mut(p)
            .buffers
            .get_mut(&buffer)
            .expect("txn buffer");
        b.allow_user_free = true;
        if !sync_call {
            // replies and one-way calls end here; the buffer lives until BC_FREE_BUFFER
            b.txn = None;
            return;
        }
        // the receiving thread now owes the reply
        txn.to_thread = Some(t);
        txn.to_parent = self.threads[&t].stack;
        self.thread_mut(t).stack = Some(id);
        self.txns.insert(id, txn);
    }

    // ---- write (binder_thread_write, binder.c:4041) ------------------------------------------------

    fn thread_write(
        &mut self,
        t: ThreadId,
        commands: &[u8],
        payload: &[u8],
        fds: &mut std::vec::IntoIter<F>,
    ) -> Result<usize, WriteError> {
        let mut at = 0;
        while at < commands.len() && !self.threads[&t].return_error_pending {
            let fail = |errno| WriteError {
                errno,
                write_consumed: at,
            };
            let (cmd, next) = match decode_command(commands, at) {
                Ok(c) => c,
                Err(_) => {
                    self.reject(Reject::TruncatedCommand);
                    return Err(fail(Errno::Fault));
                }
            };
            use atl_binder_wire::Command as C;
            match cmd {
                C::Unknown(_) => {
                    self.reject(Reject::UnknownCommand);
                    return Err(fail(Errno::Inval));
                }
                C::Unsupported(_) => {
                    self.reject(Reject::UnsupportedCommand);
                    return Err(fail(Errno::Inval));
                }
                C::IncRefs(h) => self.bc_refcount(t, h, true, false),
                C::Acquire(h) => self.bc_refcount(t, h, true, true),
                C::Release(h) => self.bc_refcount(t, h, false, true),
                C::DecRefs(h) => self.bc_refcount(t, h, false, false),
                C::IncRefsDone(pc) => self.bc_node_done(t, pc, false),
                C::AcquireDone(pc) => self.bc_node_done(t, pc, true),
                C::FreeBuffer(off) => self.bc_free_buffer(t, off),
                C::Transaction(td) => self.transaction(t, td, false, payload, fds),
                C::Reply(td) => self.transaction(t, td, true, payload, fds),
                C::RegisterLooper => self.bc_looper(t, LOOPER_REGISTERED),
                C::EnterLooper => self.bc_looper(t, LOOPER_ENTERED),
                C::ExitLooper => self.bc_looper(t, LOOPER_EXITED),
                C::RequestDeathNotification(hc) => self.bc_request_death(t, hc),
                C::ClearDeathNotification(hc) => self.bc_clear_death(t, hc),
                C::DeadBinderDone(cookie) => self.bc_dead_binder_done(t, cookie),
            }
            at = next;
        }
        Ok(at)
    }

    /// BC_REGISTER_LOOPER / ENTER / EXIT (binder.c:4266-4302).
    fn bc_looper(&mut self, t: ThreadId, bit: u8) {
        let p = self.threads[&t].proc;
        let looper = self.threads[&t].looper;
        let mut invalid = false;
        if bit == LOOPER_REGISTERED {
            if looper & LOOPER_ENTERED != 0 || self.proc(p).requested_threads == 0 {
                invalid = true;
            } else {
                let proc = self.proc_mut(p);
                proc.requested_threads -= 1;
                proc.requested_threads_started += 1;
            }
        } else if bit == LOOPER_ENTERED && looper & LOOPER_REGISTERED != 0 {
            invalid = true;
        }
        let th = self.thread_mut(t);
        th.looper |= bit;
        if invalid {
            th.looper |= LOOPER_INVALID;
            self.reject(Reject::InvalidLooper);
        }
    }
}
