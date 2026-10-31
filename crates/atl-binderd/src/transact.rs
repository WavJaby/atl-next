// SPDX-License-Identifier: MIT
//! Transactions: target resolution, buffer allocation, object translation, delivery queues, failure
//! paths and buffer release (kernel `binder_transaction`, binder.c:2987-3860; `binder_proc_transaction`
//! :2842; `binder_send_failed_reply` :1685; `binder_free_buf` :4000-4040).

use atl_binder_wire::{FD_OBJECT_FD_OFFSET, Object, TransactionData, uapi};

use crate::alloc::AllocError;
use crate::{Binderd, Buffer, ErrorSlot, Held, NodeId, ProcId, Reject, ThreadId, Txn, TxnId, Work};

/// Why a transaction failed: what the sender's read reports.
struct Failure {
    /// BR_FAILED_REPLY or BR_DEAD_REPLY.
    cmd: u32,
    /// Set once a reply has popped the transaction it answers: that caller gets `cmd` instead, and the
    /// replying thread a BR_TRANSACTION_COMPLETE (binder.c:3828-3837).
    in_reply_to: Option<TxnId>,
}

fn failed() -> Failure {
    Failure {
        cmd: uapi::BR_FAILED_REPLY,
        in_reply_to: None,
    }
}

fn dead() -> Failure {
    Failure {
        cmd: uapi::BR_DEAD_REPLY,
        in_reply_to: None,
    }
}

impl<F> Binderd<F> {
    /// BC_TRANSACTION / BC_REPLY.
    pub(crate) fn transaction(
        &mut self,
        t: ThreadId,
        td: TransactionData,
        reply: bool,
        payload: &[u8],
        fds: &mut std::vec::IntoIter<F>,
    ) {
        if let Err(f) = self.try_transaction(t, td, reply, payload, fds) {
            if let Some(irt) = f.in_reply_to {
                self.thread_mut(t).return_error_pending = true;
                self.enqueue_thread(
                    t,
                    Work::Error(uapi::BR_TRANSACTION_COMPLETE, ErrorSlot::Return),
                );
                self.send_failed_reply(irt, f.cmd);
            } else {
                self.thread_mut(t).return_error_pending = true;
                self.enqueue_thread(t, Work::Error(f.cmd, ErrorSlot::Return));
            }
        }
    }

    fn fail(&mut self, r: Reject) -> Failure {
        self.reject(r);
        failed()
    }

    fn try_transaction(
        &mut self,
        t: ThreadId,
        td: TransactionData,
        reply: bool,
        payload: &[u8],
        fds: &mut std::vec::IntoIter<F>,
    ) -> Result<(), Failure> {
        let p = self.threads[&t].proc;
        let oneway = !reply && td.flags & uapi::TF_ONE_WAY != 0;
        let mut target_thread = None;
        let mut target_node = None;
        let mut in_reply_to = None;
        let target_proc;
        if reply {
            let Some(irt) = self.threads[&t].stack else {
                return Err(self.fail(Reject::ReplyWithoutTransaction));
            };
            if self.txns[&irt].to_thread != Some(t) {
                return Err(self.fail(Reject::BadTransactionStack));
            }
            self.thread_mut(t).stack = self.txns[&irt].to_parent;
            let Some(from) = self.txns[&irt]
                .from
                .filter(|f| self.threads.contains_key(f))
            else {
                return Err(Failure {
                    cmd: uapi::BR_DEAD_REPLY,
                    in_reply_to: Some(irt),
                });
            };
            if self.threads[&from].stack != Some(irt) {
                return Err(self.fail(Reject::BadTransactionStack));
            }
            in_reply_to = Some(irt);
            target_thread = Some(from);
            target_proc = self.threads[&from].proc;
        } else {
            let handle = td.target_handle();
            let node = if handle == 0 {
                self.context_manager.ok_or_else(dead)?
            } else {
                match self.proc(p).refs.get(&handle) {
                    Some(r) if r.strong > 0 => r.node,
                    _ => return Err(self.fail(Reject::InvalidHandle)),
                }
            };
            let np = self.nodes[&node].proc.ok_or_else(dead)?;
            if np == p {
                return Err(self.fail(Reject::SelfTransaction));
            }
            if !oneway {
                // a sync call while a transaction waits at the head of this thread's queue
                // (binder.c:3166-3186), or while this thread waits for a reply (:3192-3205)
                if matches!(self.threads[&t].todo.front(), Some(Work::Transaction(_))) {
                    return Err(self.fail(Reject::BadTransactionStack));
                }
                if let Some(top) = self.threads[&t].stack {
                    if self.txns[&top].to_thread != Some(t) {
                        return Err(self.fail(Reject::BadTransactionStack));
                    }
                    // nested call: deliver to the thread of the target process already in this call
                    // chain, which is blocked waiting for it (binder.c:3206-3220)
                    let mut cur = Some(top);
                    while let Some(c) = cur {
                        let tx = &self.txns[&c];
                        if let Some(f) = tx.from
                            && self.threads.get(&f).is_some_and(|th| th.proc == np)
                        {
                            target_thread = Some(f);
                            break;
                        }
                        cur = tx.from_parent;
                    }
                }
            }
            target_node = Some(node);
            target_proc = np;
        }

        let in_reply_failure = |f: Failure| Failure {
            cmd: f.cmd,
            in_reply_to: f.in_reply_to.or(in_reply_to),
        };
        if !td.offsets_size.is_multiple_of(size_of::<u64>() as u64) {
            return Err(in_reply_failure(self.fail(Reject::InvalidOffsets)));
        }
        let slice = |at: u64, len: u64| -> Option<&[u8]> {
            let start = usize::try_from(at).ok()?;
            let end = start.checked_add(usize::try_from(len).ok()?)?;
            payload.get(start..end)
        };
        let (Some(data), Some(offsets)) = (
            slice(td.buffer, td.data_size),
            slice(td.offsets, td.offsets_size),
        ) else {
            return Err(in_reply_failure(self.fail(Reject::PayloadOutOfBounds)));
        };
        let buffer =
            match self
                .proc_mut(target_proc)
                .alloc
                .alloc(td.data_size, td.offsets_size, oneway)
            {
                Ok(off) => off,
                Err(AllocError::NoSpace) => {
                    return Err(in_reply_failure(self.fail(Reject::NoSpace)));
                }
                Err(AllocError::Invalid) => {
                    return Err(in_reply_failure(self.fail(Reject::PayloadOutOfBounds)));
                }
            };
        if let Some(n) = target_node {
            // the buffer holds a strong ref on its target until freed (binder.c:2948-2967)
            self.inc_node(n, true, false, None);
        }
        self.proc_mut(target_proc).buffers.insert(
            buffer,
            Buffer {
                is_async: oneway,
                target_node,
                txn: None,
                allow_user_free: false,
                held: Vec::new(),
            },
        );
        let data_len = data.len();
        let offsets_at = data_len.next_multiple_of(size_of::<u64>());
        let mut bytes = vec![0u8; offsets_at + offsets.len()];
        bytes[..data_len].copy_from_slice(data);
        bytes[offsets_at..].copy_from_slice(offsets);

        let accept_fds = match (in_reply_to, target_node) {
            (Some(irt), _) => self.txns[&irt].flags & uapi::TF_ACCEPT_FDS != 0,
            (None, Some(n)) => self.nodes[&n].accept_fds,
            (None, None) => false,
        };
        let mut fixups = Vec::new();
        let translated = self.translate_objects(
            t,
            target_proc,
            buffer,
            &mut bytes,
            data_len,
            offsets,
            accept_fds,
            fds,
            &mut fixups,
        );
        if let Err(r) = translated {
            self.reject(r);
            self.free_buffer(target_proc, buffer);
            return Err(in_reply_failure(failed()));
        }

        let id = TxnId(self.id());
        self.txns.insert(
            id,
            Txn {
                from: (!reply && !oneway).then_some(t),
                sender: self.proc(p).tag,
                to_proc: target_proc,
                to_thread: None,
                from_parent: None,
                to_parent: None,
                is_reply: reply,
                code: td.code,
                flags: td.flags,
                data_size: td.data_size,
                offsets_size: td.offsets_size,
                buffer: Some(buffer),
                bytes,
                fds: fixups,
            },
        );
        self.proc_mut(target_proc)
            .buffers
            .get_mut(&buffer)
            .expect("just inserted")
            .txn = Some(id);

        if let (Some(irt), Some(to)) = (in_reply_to, target_thread) {
            self.enqueue_thread(t, Work::TransactionComplete);
            self.thread_mut(to).stack = self.txns[&irt].from_parent;
            self.enqueue_thread(to, Work::Transaction(id));
            self.drop_txn(irt);
        } else if !oneway {
            // the caller's TRANSACTION_COMPLETE waits for the reply (binder.c:3703-3712)
            self.enqueue_thread_deferred(t, Work::TransactionComplete);
            let parent = self.threads[&t].stack;
            self.txns.get_mut(&id).expect("just inserted").from_parent = parent;
            self.thread_mut(t).stack = Some(id);
            self.proc_transaction(id, target_proc, target_thread, None);
        } else {
            self.proc_transaction(id, target_proc, None, target_node);
            self.enqueue_thread(t, Work::TransactionComplete);
        }
        Ok(())
    }

    /// Walks the offsets array, rewriting each object as the receiver must see it and recording what
    /// the buffer holds (`binder_translate_binder` :2219, `_handle` :2272, `_fd` :2345).
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site; the arguments are the transaction being built"
    )]
    fn translate_objects(
        &mut self,
        t: ThreadId,
        target_proc: ProcId,
        buffer: u64,
        bytes: &mut [u8],
        data_len: usize,
        offsets: &[u8],
        accept_fds: bool,
        fds: &mut std::vec::IntoIter<F>,
        fixups: &mut Vec<(u64, F)>,
    ) -> Result<(), Reject> {
        let p = self.threads[&t].proc;
        let mut min = 0usize;
        for chunk in offsets.as_chunks::<{ size_of::<u64>() }>().0 {
            let off = u64::from_ne_bytes(*chunk);
            let off = usize::try_from(off).map_err(|_| Reject::InvalidObject)?;
            // objects must not overlap and come in ascending order (binder.c:3476-3490)
            if off < min {
                return Err(Reject::InvalidOffsets);
            }
            let obj = Object::read(&bytes[..data_len], off).ok_or(Reject::InvalidObject)?;
            let (new, held) = match obj {
                Object::Binder {
                    weak,
                    flags,
                    ptr,
                    cookie,
                } => {
                    let node = match self.proc(p).nodes.get(&ptr) {
                        Some(&n) => n,
                        None => self.new_node(p, ptr, cookie, flags),
                    };
                    if self.nodes[&node].cookie != cookie {
                        return Err(Reject::CookieMismatch);
                    }
                    let handle = self
                        .inc_ref_for_node(target_proc, node, !weak, Some(t))
                        .ok_or(Reject::InvalidObject)?;
                    (
                        Object::Handle {
                            weak,
                            flags,
                            handle,
                            cookie: 0,
                        },
                        Some(Held::Ref {
                            handle,
                            strong: !weak,
                        }),
                    )
                }
                Object::Handle {
                    weak,
                    flags,
                    handle,
                    ..
                } => {
                    let node = match self.proc(p).refs.get(&handle) {
                        Some(r) if weak || r.strong > 0 => r.node,
                        _ => return Err(Reject::InvalidHandle),
                    };
                    if self.nodes[&node].proc == Some(target_proc) {
                        // a handle sent back to its owner arrives as the owner's own binder
                        self.inc_node(node, !weak, false, None);
                        let n = &self.nodes[&node];
                        (
                            Object::Binder {
                                weak,
                                flags,
                                ptr: n.ptr,
                                cookie: n.cookie,
                            },
                            Some(Held::Node {
                                node,
                                strong: !weak,
                            }),
                        )
                    } else {
                        let handle = self
                            .inc_ref_for_node(target_proc, node, !weak, None)
                            .ok_or(Reject::InvalidObject)?;
                        (
                            Object::Handle {
                                weak,
                                flags,
                                handle,
                                cookie: 0,
                            },
                            Some(Held::Ref {
                                handle,
                                strong: !weak,
                            }),
                        )
                    }
                }
                Object::Fd { .. } => {
                    if !accept_fds {
                        return Err(Reject::FdsNotAccepted);
                    }
                    let fd = fds.next().ok_or(Reject::MissingFd)?;
                    fixups.push((buffer + (off + FD_OBJECT_FD_OFFSET) as u64, fd));
                    (obj, None)
                }
                Object::Other { .. } => return Err(Reject::UnsupportedObject),
            };
            if let Some(h) = held {
                self.proc_mut(target_proc)
                    .buffers
                    .get_mut(&buffer)
                    .expect("allocated")
                    .held
                    .push(h);
            }
            new.write(bytes, off);
            min = off + obj.len();
        }
        Ok(())
    }

    /// `binder_proc_transaction` (binder.c:2842-2925): to `thread` if given, else to a waiting looper,
    /// else the process queue; a one-way call waits behind its node's previous one-way call until that
    /// buffer is freed.
    fn proc_transaction(
        &mut self,
        id: TxnId,
        target_proc: ProcId,
        thread: Option<ThreadId>,
        oneway_node: Option<NodeId>,
    ) {
        let mut pending_async = false;
        if let Some(n) = oneway_node {
            let node = self.node_mut(n);
            if node.has_async {
                pending_async = true;
            } else {
                node.has_async = true;
            }
        }
        let thread = match thread {
            Some(th) => Some(th),
            None if !pending_async => self.select_thread(target_proc),
            None => None,
        };
        if let Some(th) = thread {
            self.enqueue_thread(th, Work::Transaction(id));
        } else if !pending_async {
            self.proc_mut(target_proc)
                .todo
                .push_back(Work::Transaction(id));
        } else if let Some(n) = oneway_node {
            self.node_mut(n).async_todo.push_back(id);
        }
    }

    /// `binder_send_failed_reply` (binder.c:1685): the caller waiting on `id` gets `cmd`; if it is gone,
    /// the error travels up to whoever called it.
    pub(crate) fn send_failed_reply(&mut self, id: TxnId, cmd: u32) {
        let mut cur = id;
        loop {
            let (from, parent) = {
                let tx = &self.txns[&cur];
                (tx.from, tx.from_parent)
            };
            if let Some(from) = from.filter(|f| self.threads.contains_key(f)) {
                self.thread_mut(from).stack = parent;
                self.enqueue_thread(from, Work::Error(cmd, ErrorSlot::Reply));
                self.drop_txn(cur);
                return;
            }
            self.drop_txn(cur);
            match parent {
                Some(p) => cur = p,
                None => return,
            }
        }
    }

    /// Frees a transaction. A buffer never delivered is released with it; a delivered one stays with
    /// the receiver until BC_FREE_BUFFER.
    pub(crate) fn drop_txn(&mut self, id: TxnId) {
        let Some(tx) = self.txns.remove(&id) else {
            return;
        };
        let Some(off) = tx.buffer else { return };
        let Some(b) = self
            .procs
            .get_mut(&tx.to_proc)
            .and_then(|p| p.buffers.get_mut(&off))
        else {
            return;
        };
        b.txn = None;
        if !b.allow_user_free {
            self.free_buffer(tx.to_proc, off);
        }
    }

    /// BC_FREE_BUFFER.
    pub(crate) fn bc_free_buffer(&mut self, t: ThreadId, off: u64) {
        let p = self.threads[&t].proc;
        if !self
            .proc(p)
            .buffers
            .get(&off)
            .is_some_and(|b| b.allow_user_free)
        {
            self.reject(Reject::InvalidFreeBuffer);
            return;
        }
        self.free_buffer(p, off);
    }

    /// `binder_free_buf` + `binder_transaction_buffer_release`: drops what the buffer holds and lets the
    /// node's next one-way call through.
    pub(crate) fn free_buffer(&mut self, p: ProcId, off: u64) {
        let Some(b) = self.proc_mut(p).buffers.remove(&off) else {
            return;
        };
        if let Some(id) = b.txn
            && let Some(tx) = self.txns.get_mut(&id)
        {
            tx.buffer = None;
        }
        if b.is_async
            && let Some(n) = b.target_node
        {
            match self.node_mut(n).async_todo.pop_front() {
                Some(next) => self.enqueue_proc(p, Work::Transaction(next)),
                None => self.node_mut(n).has_async = false,
            }
        }
        if let Some(n) = b.target_node {
            self.dec_node(n, true, false);
        }
        for h in b.held {
            match h {
                Held::Node { node, strong } => self.dec_node(node, strong, false),
                Held::Ref { handle, strong } => {
                    self.dec_ref(p, handle, strong);
                }
            }
        }
        self.proc_mut(p).alloc.free(off);
    }
}
