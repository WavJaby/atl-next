// SPDX-License-Identifier: MIT
//! Reference counting (nodes and refs), death notifications, thread and process exit. Kernel:
//! `binder_inc_node_nilocked` binder.c:815, `binder_dec_node_nilocked` :876, refs :1000-1240,
//! BC_* refcount commands :4065-4209, death commands :4304-4470, `binder_thread_release` :5253,
//! `binder_node_release` :6120, `binder_deferred_release` :6185.

use atl_binder_wire::{HandleCookie, PtrCookie, Uptr};

use crate::{
    Binderd, Death, DeathId, DeathKind, LOOPER_ENTERED, LOOPER_REGISTERED, NodeId, ProcId, Ref,
    Reject, ThreadId, Work,
};

impl<F> Binderd<F> {
    // ---- nodes -------------------------------------------------------------------------------------

    /// `binder_inc_node_nilocked`. `target`: the thread whose queue carries the owner's BR_INCREFS /
    /// BR_ACQUIRE, when the owner has not been told yet. False where the kernel returns `-EINVAL`
    /// (counts are still raised for local refs, as there).
    pub(crate) fn inc_node(
        &mut self,
        n: NodeId,
        strong: bool,
        internal: bool,
        target: Option<ThreadId>,
    ) -> bool {
        let is_cm = self.context_manager == Some(n);
        let node = self.node_mut(n);
        if strong {
            if internal {
                if target.is_none() && node.internal_strong == 0 && !(is_cm && node.has_strong) {
                    return false;
                }
                node.internal_strong += 1;
            } else {
                node.local_strong += 1;
            }
            if !node.has_strong
                && let Some(t) = target
            {
                self.dequeue_node_work(n);
                self.node_mut(n).queued = true;
                self.enqueue_thread_deferred(t, Work::Node(n));
            }
        } else {
            if !internal {
                node.local_weak += 1;
            }
            if !node.has_weak && !node.queued {
                let Some(t) = target else {
                    return false;
                };
                node.queued = true;
                self.enqueue_thread_deferred(t, Work::Node(n));
            }
        }
        true
    }

    /// `binder_dec_node_nilocked`: when the last ref of a kind goes, the owner is told
    /// (BR_RELEASE / BR_DECREFS via its process queue) or the node is freed.
    pub(crate) fn dec_node(&mut self, n: NodeId, strong: bool, internal: bool) {
        let node = self.node_mut(n);
        if strong {
            if internal {
                node.internal_strong -= 1;
            } else {
                node.local_strong -= 1;
            }
            if node.local_strong > 0 || node.internal_strong > 0 {
                return;
            }
        } else {
            if !internal {
                node.local_weak -= 1;
            }
            if node.local_weak > 0 || !node.refs.is_empty() {
                return;
            }
        }
        match node.proc {
            Some(p) if node.has_strong || node.has_weak => {
                if !node.queued {
                    node.queued = true;
                    self.enqueue_proc(p, Work::Node(n));
                }
            }
            proc => {
                if node.refs.is_empty() && node.local_strong == 0 && node.local_weak == 0 {
                    let ptr = node.ptr;
                    self.dequeue_node_work(n);
                    self.nodes.remove(&n);
                    if let Some(p) = proc {
                        self.proc_mut(p).nodes.remove(&ptr);
                    }
                }
            }
        }
    }

    /// Takes a node's work item out of whichever queue of its owner holds it.
    fn dequeue_node_work(&mut self, n: NodeId) {
        let Some(p) = self.nodes.get(&n).and_then(|node| node.proc) else {
            return;
        };
        let w = Work::Node(n);
        self.proc_mut(p).todo.retain(|x| *x != w);
        let threads: Vec<ThreadId> = self.proc(p).threads.iter().copied().collect();
        for t in threads {
            self.thread_mut(t).todo.retain(|x| *x != w);
        }
        self.node_mut(n).queued = false;
    }

    // ---- refs --------------------------------------------------------------------------------------

    /// Handle of `p`'s ref to `node`, created if needed (lowest free number from 1; 0 only for the
    /// context manager, binder.c:1050-1062), with one more strong or weak count.
    pub(crate) fn inc_ref_for_node(
        &mut self,
        p: ProcId,
        node: NodeId,
        strong: bool,
        target: Option<ThreadId>,
    ) -> Option<u32> {
        let (handle, created) = match self.proc(p).ref_by_node.get(&node) {
            Some(&h) => (h, false),
            None => {
                let first = u32::from(self.context_manager != Some(node));
                let mut h = first;
                for &used in self.proc(p).refs.range(first..).map(|(k, _)| k) {
                    if used != h {
                        break;
                    }
                    h += 1;
                }
                let proc = self.proc_mut(p);
                proc.refs.insert(
                    h,
                    Ref {
                        node,
                        strong: 0,
                        weak: 0,
                        death: None,
                    },
                );
                proc.ref_by_node.insert(node, h);
                self.node_mut(node).refs.insert((p, h));
                (h, true)
            }
        };
        if self.inc_ref(p, handle, strong, target) {
            Some(handle)
        } else {
            if created {
                self.cleanup_ref(p, handle);
            }
            None
        }
    }

    fn inc_ref(&mut self, p: ProcId, handle: u32, strong: bool, target: Option<ThreadId>) -> bool {
        let r = &self.proc(p).refs[&handle];
        let (node, first) = (r.node, if strong { r.strong } else { r.weak } == 0);
        if first && !self.inc_node(node, strong, true, target) {
            return false;
        }
        let r = self.proc_mut(p).refs.get_mut(&handle).expect("ref");
        if strong {
            r.strong += 1;
        } else {
            r.weak += 1;
        }
        true
    }

    /// One strong or weak count off `p`'s ref `handle`; the ref goes with its last count. False when
    /// there is no such ref or count (`binder_get_ref_olocked` needs a strong count for strong ops).
    pub(crate) fn dec_ref(&mut self, p: ProcId, handle: u32, strong: bool) -> bool {
        let Some(r) = self.proc_mut(p).refs.get_mut(&handle) else {
            return false;
        };
        let node = r.node;
        if strong {
            if r.strong == 0 {
                return false;
            }
            r.strong -= 1;
            if r.strong == 0 {
                self.dec_node(node, true, true);
            }
        } else {
            if r.weak == 0 {
                return false;
            }
            r.weak -= 1;
        }
        let r = &self.proc(p).refs[&handle];
        if r.strong == 0 && r.weak == 0 {
            self.cleanup_ref(p, handle);
        }
        true
    }

    /// `binder_cleanup_ref_olocked` + `binder_free_ref`.
    fn cleanup_ref(&mut self, p: ProcId, handle: u32) {
        let Some(r) = self.proc_mut(p).refs.remove(&handle) else {
            return;
        };
        self.proc_mut(p).ref_by_node.remove(&r.node);
        self.node_mut(r.node).refs.remove(&(p, handle));
        if r.strong > 0 {
            self.dec_node(r.node, true, true);
        }
        if self.nodes.contains_key(&r.node) {
            self.dec_node(r.node, false, true);
        }
        if let Some(d) = r.death {
            self.free_death(d);
        }
    }

    /// BC_INCREFS / BC_ACQUIRE / BC_RELEASE / BC_DECREFS (binder.c:4065-4133).
    pub(crate) fn bc_refcount(&mut self, t: ThreadId, handle: u32, increment: bool, strong: bool) {
        let p = self.threads[&t].proc;
        if increment
            && handle == 0
            && let Some(cm) = self.context_manager
        {
            if self.nodes[&cm].proc == Some(p) {
                // the context manager cannot hold a ref to itself
                self.reject(Reject::InvalidRefcount);
                return;
            }
            if self.inc_ref_for_node(p, cm, strong, None).is_some() {
                return;
            }
        }
        let ok = if increment {
            match self.proc(p).refs.get(&handle) {
                // a strong count needs an existing strong one: weak -> strong goes through
                // BC_ATTEMPT_ACQUIRE, which no driver implements
                Some(r) if !strong || r.strong > 0 => self.inc_ref(p, handle, strong, None),
                _ => false,
            }
        } else {
            self.dec_ref(p, handle, strong)
        };
        if !ok {
            self.reject(Reject::InvalidRefcount);
        }
    }

    /// BC_INCREFS_DONE / BC_ACQUIRE_DONE (binder.c:4135-4208): the owner took the ref it was given, so
    /// the extra one binderd held while waiting goes.
    pub(crate) fn bc_node_done(&mut self, t: ThreadId, pc: PtrCookie, strong: bool) {
        let p = self.threads[&t].proc;
        let Some(&n) = self.proc(p).nodes.get(&pc.ptr) else {
            self.reject(Reject::InvalidNodeDone);
            return;
        };
        let node = self.node_mut(n);
        let pending = if strong {
            &mut node.pending_strong
        } else {
            &mut node.pending_weak
        };
        if node.cookie != pc.cookie || !*pending {
            self.reject(Reject::InvalidNodeDone);
            return;
        }
        *pending = false;
        self.dec_node(n, strong, false);
    }

    // ---- death notifications -----------------------------------------------------------------------

    fn free_death(&mut self, d: DeathId) {
        let Some(death) = self.deaths.remove(&d) else {
            return;
        };
        let w = Work::Death(d);
        if let Some(proc) = self.procs.get_mut(&death.proc) {
            proc.todo.retain(|x| *x != w);
            proc.delivered_death.retain(|x| *x != d);
            let threads: Vec<ThreadId> = proc.threads.iter().copied().collect();
            for t in threads {
                self.thread_mut(t).todo.retain(|x| *x != w);
            }
        }
    }

    /// Queues a death work item on the thread if it is a looper, else on the process.
    fn queue_death_for(&mut self, t: ThreadId, d: DeathId) {
        self.deaths.get_mut(&d).expect("death").queued = true;
        if self.threads[&t].looper & (LOOPER_REGISTERED | LOOPER_ENTERED) != 0 {
            self.enqueue_thread(t, Work::Death(d));
        } else {
            let p = self.threads[&t].proc;
            self.enqueue_proc(p, Work::Death(d));
        }
    }

    /// BC_REQUEST_DEATH_NOTIFICATION.
    pub(crate) fn bc_request_death(&mut self, t: ThreadId, hc: HandleCookie) {
        let p = self.threads[&t].proc;
        let Some(r) = self.proc(p).refs.get(&hc.handle) else {
            self.reject(Reject::InvalidDeath);
            return;
        };
        if r.death.is_some() {
            self.reject(Reject::InvalidDeath);
            return;
        }
        let node_dead = self.nodes[&r.node].proc.is_none();
        let d = DeathId(self.id());
        self.deaths.insert(
            d,
            Death {
                proc: p,
                cookie: hc.cookie,
                kind: DeathKind::DeadBinder,
                queued: false,
            },
        );
        self.proc_mut(p)
            .refs
            .get_mut(&hc.handle)
            .expect("ref")
            .death = Some(d);
        if node_dead {
            self.deaths.get_mut(&d).expect("death").queued = true;
            self.enqueue_proc(p, Work::Death(d));
        }
    }

    /// BC_CLEAR_DEATH_NOTIFICATION.
    pub(crate) fn bc_clear_death(&mut self, t: ThreadId, hc: HandleCookie) {
        let p = self.threads[&t].proc;
        let Some(d) = self.proc(p).refs.get(&hc.handle).and_then(|r| r.death) else {
            self.reject(Reject::InvalidDeath);
            return;
        };
        if self.deaths[&d].cookie != hc.cookie {
            self.reject(Reject::InvalidDeath);
            return;
        }
        self.proc_mut(p)
            .refs
            .get_mut(&hc.handle)
            .expect("ref")
            .death = None;
        let in_list = self.deaths[&d].queued || self.proc(p).delivered_death.contains(&d);
        if in_list {
            // fired already: the clear is confirmed once the BR_DEAD_BINDER is acknowledged
            self.deaths.get_mut(&d).expect("death").kind = DeathKind::DeadBinderAndClear;
        } else {
            self.deaths.get_mut(&d).expect("death").kind = DeathKind::Clear;
            self.queue_death_for(t, d);
        }
    }

    /// BC_DEAD_BINDER_DONE.
    pub(crate) fn bc_dead_binder_done(&mut self, t: ThreadId, cookie: Uptr) {
        let p = self.threads[&t].proc;
        let Some(pos) = self
            .proc(p)
            .delivered_death
            .iter()
            .position(|d| self.deaths[d].cookie == cookie)
        else {
            self.reject(Reject::InvalidDeath);
            return;
        };
        let d = self.proc_mut(p).delivered_death.remove(pos);
        if self.deaths[&d].kind == DeathKind::DeadBinderAndClear {
            self.deaths.get_mut(&d).expect("death").kind = DeathKind::Clear;
            self.queue_death_for(t, d);
        }
    }

    // ---- exit --------------------------------------------------------------------------------------

    /// `BINDER_THREAD_EXIT`, or the thread's connection closed.
    pub fn thread_exit(&mut self, t: ThreadId) {
        if !self.threads.contains_key(&t) {
            return;
        }
        self.thread_release(t);
        let p = self.threads[&t].proc;
        let proc = self.proc_mut(p);
        proc.threads.remove(&t);
        proc.waiting.retain(|w| *w != t);
        self.threads.remove(&t);
        self.woken.retain(|w| *w != t);
        self.run_woken();
    }

    /// `binder_thread_release`: the caller of a call this thread was serving gets BR_DEAD_REPLY; calls
    /// it made lose their sender; its queued work is released.
    fn thread_release(&mut self, t: ThreadId) {
        let top = self.threads[&t].stack;
        let send_reply = top.filter(|c| self.txns[c].to_thread == Some(t));
        let mut cur = top;
        while let Some(c) = cur {
            let tx = self.txns.get_mut(&c).expect("stacked txn");
            if tx.to_thread == Some(t) {
                tx.to_thread = None;
                let buffer = tx.buffer.take();
                let (to_proc, next) = (tx.to_proc, tx.to_parent);
                if let Some(off) = buffer
                    && let Some(b) = self
                        .procs
                        .get_mut(&to_proc)
                        .and_then(|p| p.buffers.get_mut(&off))
                {
                    b.txn = None;
                }
                cur = next;
            } else if tx.from == Some(t) {
                tx.from = None;
                cur = tx.from_parent;
            } else {
                break;
            }
        }
        self.thread_mut(t).stack = None;
        if let Some(s) = send_reply {
            self.send_failed_reply(s, atl_binder_wire::uapi::BR_DEAD_REPLY);
        }
        let todo: Vec<Work> = self.thread_mut(t).todo.drain(..).collect();
        self.release_work(todo);
    }

    /// `binder_release_work`: undelivered calls fail back to their callers with BR_DEAD_REPLY; the rest
    /// is dropped.
    fn release_work(&mut self, work: Vec<Work>) {
        for w in work {
            match w {
                Work::Transaction(id) => {
                    let Some(tx) = self.txns.get(&id) else {
                        continue;
                    };
                    let sync_call =
                        !tx.is_reply && tx.flags & atl_binder_wire::uapi::TF_ONE_WAY == 0;
                    if sync_call {
                        self.send_failed_reply(id, atl_binder_wire::uapi::BR_DEAD_REPLY);
                    } else {
                        self.drop_txn(id);
                    }
                }
                Work::Node(n) => {
                    if let Some(node) = self.nodes.get_mut(&n) {
                        node.queued = false;
                    }
                }
                Work::Death(d) => {
                    if let Some(death) = self.deaths.get_mut(&d) {
                        if death.kind == DeathKind::DeadBinder {
                            death.queued = false;
                        } else {
                            self.deaths.remove(&d);
                        }
                    }
                }
                Work::TransactionComplete | Work::Error(..) => {}
            }
        }
    }

    /// The process's control connection closed (`binder_deferred_release`): threads released, nodes
    /// die (holders with death notifications get BR_DEAD_BINDER), refs dropped, buffers freed.
    pub fn process_exit(&mut self, p: ProcId) {
        if !self.procs.contains_key(&p) {
            return;
        }
        if self
            .context_manager
            .is_some_and(|cm| self.nodes[&cm].proc == Some(p))
        {
            self.context_manager = None;
        }
        let threads: Vec<ThreadId> = self.proc(p).threads.iter().copied().collect();
        for t in &threads {
            self.thread_release(*t);
        }
        for t in &threads {
            self.threads.remove(t);
            self.woken.retain(|w| w != t);
        }
        self.proc_mut(p).threads.clear();
        self.proc_mut(p).waiting.clear();
        // queued calls release their buffers' refs while the nodes still count them
        let todo: Vec<Work> = self.proc_mut(p).todo.drain(..).collect();
        self.release_work(todo);
        let nodes: Vec<NodeId> = self.proc(p).nodes.values().copied().collect();
        for n in nodes {
            self.node_release(n);
        }
        let handles: Vec<u32> = self.proc(p).refs.keys().copied().collect();
        for h in handles {
            self.cleanup_ref(p, h);
        }
        let delivered = std::mem::take(&mut self.proc_mut(p).delivered_death);
        for d in delivered {
            self.deaths.remove(&d);
        }
        let buffers: Vec<u64> = self.proc(p).buffers.keys().copied().collect();
        for off in buffers {
            let b = self.proc_mut(p).buffers.remove(&off).expect("listed");
            if let Some(id) = b.txn {
                // a call still queued elsewhere or on a stack: its buffer is gone with the region
                if let Some(tx) = self.txns.get_mut(&id) {
                    tx.buffer = None;
                }
            }
        }
        self.procs.remove(&p);
        self.run_woken();
    }

    /// `binder_node_release`.
    fn node_release(&mut self, n: NodeId) {
        let p = self.nodes[&n].proc;
        self.dequeue_node_work(n);
        let pending: Vec<_> = self.node_mut(n).async_todo.drain(..).collect();
        for id in pending {
            self.drop_txn(id);
        }
        let node = self.node_mut(n);
        node.proc = None;
        node.local_strong = 0;
        node.local_weak = 0;
        let ptr = node.ptr;
        if let Some(p) = p {
            self.proc_mut(p).nodes.remove(&ptr);
        }
        let holders: Vec<(ProcId, u32)> = self.nodes[&n].refs.iter().copied().collect();
        if holders.is_empty() {
            self.nodes.remove(&n);
            return;
        }
        for (rp, h) in holders {
            if let Some(d) = self.proc(rp).refs.get(&h).and_then(|r| r.death) {
                self.deaths.get_mut(&d).expect("death").kind = DeathKind::DeadBinder;
                self.deaths.get_mut(&d).expect("death").queued = true;
                self.enqueue_proc(rp, Work::Death(d));
            }
        }
    }
}
