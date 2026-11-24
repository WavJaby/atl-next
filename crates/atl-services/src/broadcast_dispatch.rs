// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/BroadcastRecord.java, BroadcastProcessQueue.java, BroadcastQueueImpl.java (the delivery rules; the threads and binder calls are the caller's).
//! The receivers of one or many broadcasts in delivery-list order: a blocked head holds its process queue, one delivery in flight per process, one cold start at a time, ordered results and abort, resultTo, the cold-start abandon.
//! The caller feeds events (enqueue, attach, death, start failure, finish, timeouts, cold-start abandon, force-stop) and performs the returned [`Action`]s in order. Lives under the activity service's state lock; nothing here calls out except the caller's [`Policy`].
//! Not modelled: DEFERRED deliveries (deferUntilActive), the flag-off priority tranches (BR:855-875, unreachable at the pin), timeout exemptions.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use atl_services_types::hw_timeouts::{
    BROADCAST_BG_TIMEOUT_BASE_MS, BROADCAST_FG_TIMEOUT_BASE_MS, HwTimeouts,
    PENDING_COLD_START_ABANDON_BASE_MS,
};

use crate::broadcast::{FLAG_RECEIVER_FOREGROUND, FLAG_RECEIVER_NO_ABORT};

/// Identity of an enqueued broadcast; never reused.
pub type BroadcastId = u64;

/// (processName, uid): the key of a process queue (BroadcastProcessQueue). The pid is attached later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcKey {
    pub name: String,
    pub uid: u32,
}

/// A manifest receiver's component (ResolveInfo.activityInfo): its package starts the process, the pair is named in messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub package: String,
    pub class: String,
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.package, self.class)
    }
}

/// A receiver: a registered IIntentReceiver (BroadcastFilter) or a manifest component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target<R> {
    Registered(R),
    Manifest(Component),
}

/// One receiver in delivery-list order (merged by priority, BroadcastController.java:1608-1643). `proc` hosts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver<R> {
    pub target: Target<R>,
    pub priority: i32,
    pub proc: ProcKey,
}

/// The intent facts the rules read: `flags` (Intent flags), `explicit` (component or package set), `signature_perms` (isSignaturePerm of the required permissions),
/// `max_manifest_api` (BroadcastOptions.maxManifestReceiverApiLevel; None = no limit): a manifest receiver whose targetSdk is above it is skipped (BroadcastSkipPolicy.shouldSkipMessage :105-109).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub flags: i32,
    pub explicit: bool,
    pub signature_perms: bool,
    pub max_manifest_api: Option<i32>,
    /// The system boot action (LOCKED / BOOT_COMPLETED) and who the send is for: its receivers are gated by broadcast::boot_refusal at dispatch.
    pub boot: Option<(crate::broadcast::BootScope, crate::broadcast::BootAudience)>,
}

/// A result: code, data and extras (BroadcastRecord.resultCode, resultData, resultExtras). `E` is the caller's extras type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome<E> {
    pub code: i32,
    pub data: Option<String>,
    pub extras: Option<E>,
}

/// What a receiver reports back (finishReceiverLocked): its result, and whether it aborts an ordered broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish<E> {
    pub result: Outcome<E>,
    pub abort: bool,
}

/// A broadcast to enqueue. `result_to` = the sender's resultTo handle and its process; `payload` is what the caller sends to each receiver (its intent, user, ...).
#[derive(Debug, Clone)]
pub struct NewBroadcast<R, E, P> {
    pub facts: Facts,
    pub receivers: Vec<Receiver<R>>,
    pub ordered: bool,
    pub result: Outcome<E>,
    pub result_to: Option<(R, ProcKey)>,
    pub payload: P,
}

/// One delivery to a live process (scheduleReceiver, scheduleRegisteredReceiver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job<R, E, P> {
    pub id: BroadcastId,
    pub index: usize,
    pub receiver: Target<R>,
    pub ordered: bool,
    /// Registered receiver of an unordered broadcast with no resultTo: nothing comes back (BroadcastRecord.isAssumedDelivered).
    pub assume_delivered: bool,
    pub result: Outcome<E>,
    pub payload: P,
}

/// What the caller performs, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<R, E, P> {
    /// Deliver to the live process `pid`.
    Send { pid: i32, job: Job<R, E, P> },
    /// Start the process for `key` of `component`; its one delivery goes out when it attaches. The only cold start in flight. Armed by an `ArmColdStart` with the same key.
    StartProcess { key: ProcKey, component: Component },
    /// The final result of an ordered broadcast, to the resultTo handle in process `pid`.
    ResultTo {
        pid: i32,
        handle: R,
        result: Outcome<E>,
        payload: P,
    },
    /// Arm the delivery timer; on expiry call [`Dispatcher::timeout`] with the same ids.
    ArmTimeout {
        pid: i32,
        id: BroadcastId,
        index: usize,
        after_ms: u64,
    },
    /// Arm the cold-start abandon for the start of `key`; on expiry call [`Dispatcher::cold_start_timeout`] with the same token.
    ArmColdStart {
        key: ProcKey,
        token: u64,
        after_ms: u64,
    },
    /// The cold-start backstop for `key` fired: the slot is released and its delivery re-enqueued or failed. The caller ends no process; an unattached one ends at its start timeout.
    ColdStartAbandoned { key: ProcKey },
    /// A receiver ended without delivery; `reason` is for logs and counters.
    Skip {
        id: BroadcastId,
        index: usize,
        reason: String,
    },
    /// Every receiver is terminal and the result, if asked, went out: the broadcast is gone.
    Retired { id: BroadcastId },
}

/// The caller's dispatch-time skip (BroadcastSkipPolicy, consulted at dispatch): Some(reason) skips the receiver. Consulted before each start and each send; passed to each call because it reads the caller's state.
pub type Policy<'a, R> = dyn FnMut(&Facts, &Receiver<R>) -> Option<String> + 'a;

/// Delivery state of one receiver: Pending until terminal (BroadcastRecord.DELIVERY_*; SCHEDULED is [`Active`]; DEFERRED not modelled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Delivered,
    Skipped,
    Timeout,
    Failure,
}

/// A process queue's delivery in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    /// Holds the cold-start slot; the delivery goes out when the process attaches. `token` identifies this start for its abandon timer.
    Starting {
        id: BroadcastId,
        index: usize,
        token: u64,
    },
    /// Sent; waits for finish or timeout.
    InFlight(BroadcastId, usize),
}

#[derive(Debug, Default)]
struct Queue {
    pid: Option<i32>,
    /// (broadcast, receiver index) in enqueue order.
    pending: VecDeque<(BroadcastId, usize)>,
    active: Option<Active>,
}

struct Record<R, E, P> {
    facts: Facts,
    receivers: Vec<Receiver<R>>,
    ordered: bool,
    /// Per receiver: the beyond count it waits for; 0 = never (AOSP -1).
    blocked_until: Vec<usize>,
    state: Vec<State>,
    /// Per receiver: its cold start was abandoned once and re-enqueued (BroadcastQueueImpl.clearInvalidPendingColdStart).
    retried: Vec<bool>,
    /// High-water mark: the leading receivers in a terminal state (BroadcastRecord.beyondCount).
    beyond: usize,
    result: Outcome<E>,
    result_abort: bool,
    result_to: Option<(R, ProcKey)>,
    payload: P,
}

/// The dispatcher: the broadcasts not yet retired, and the process queues.
pub struct Dispatcher<R, E, P> {
    next_id: BroadcastId,
    next_token: u64,
    records: BTreeMap<BroadcastId, Record<R, E, P>>,
    queues: BTreeMap<ProcKey, Queue>,
    /// The process whose cold start holds the slot (BroadcastQueueImpl.mRunningColdStart). Set only while its queue is Starting.
    cold: Option<ProcKey>,
    /// the timers' multiplier (`ro.hw_timeout_multiplier`): see [`Dispatcher::with_hw`]
    hw: HwTimeouts,
}

// no bounds on R, E, P: a derive would require them to be Default
impl<R, E, P> Default for Dispatcher<R, E, P> {
    fn default() -> Self {
        Dispatcher {
            next_id: 0,
            next_token: 0,
            records: BTreeMap::new(),
            queues: BTreeMap::new(),
            cold: None,
            hw: HwTimeouts::default(),
        }
    }
}

impl<R, E, P> Dispatcher<R, E, P> {
    /// The dispatcher whose timers scale by `hw` (the BASE values x the multiplier). [`Default`] is the unscaled one (multiplier 1).
    pub fn with_hw(hw: HwTimeouts) -> Self {
        Dispatcher {
            hw,
            ..Self::default()
        }
    }
}

impl<R: Clone, E: Clone, P: Clone> Dispatcher<R, E, P> {
    /// Queues a broadcast and performs what is runnable (BroadcastQueueImpl.enqueueBroadcastLocked). Returns its id.
    pub fn enqueue(
        &mut self,
        b: NewBroadcast<R, E, P>,
        policy: &mut Policy<'_, R>,
    ) -> (BroadcastId, Vec<Action<R, E, P>>) {
        let id = self.next_id;
        self.next_id += 1;
        let mut out = Vec::new();
        let priorities: Vec<i32> = b.receivers.iter().map(|r| r.priority).collect();
        // limit-priority-scope is on for every app: no compat change is disabled (no EnabledSince, no overrides in ATL)
        let enabled = vec![true; b.receivers.len()];
        let blocked_until = blocked_until_beyond_count(&priorities, &enabled, b.ordered);
        for (index, r) in b.receivers.iter().enumerate() {
            self.queues
                .entry(r.proc.clone())
                .or_default()
                .pending
                .push_back((id, index));
        }
        let n = b.receivers.len();
        self.records.insert(
            id,
            Record {
                facts: b.facts,
                receivers: b.receivers,
                ordered: b.ordered,
                blocked_until,
                state: vec![State::Pending; n],
                retried: vec![false; n],
                beyond: 0,
                result: b.result,
                result_abort: false,
                result_to: b.result_to,
                payload: b.payload,
            },
        );
        if n == 0 {
            self.complete(id, &mut out);
        }
        self.drive(policy, &mut out);
        (id, out)
    }

    /// The process for `key` attached with `pid` (onApplicationAttachedLocked): its queue resumes; a cold start in flight is released.
    pub fn process_attached(
        &mut self,
        key: &ProcKey,
        pid: i32,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        self.queues.entry(key.clone()).or_default().pid = Some(pid);
        if self.cold.as_ref() == Some(key) {
            self.cold = None;
        }
        let mut out = Vec::new();
        self.drive(policy, &mut out);
        out
    }

    /// The process with `pid` died (onApplicationCleanupLocked): its in-flight delivery fails and its registered deliveries are skipped. Manifest deliveries wait for a new process.
    pub fn process_died(&mut self, pid: i32, policy: &mut Policy<'_, R>) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        let Some(key) = self.key_of(pid) else {
            return out;
        };
        let Some(queue) = self.queues.get_mut(&key) else {
            return out;
        };
        queue.pid = None;
        let failed = match queue.active {
            Some(Active::InFlight(id, index)) => {
                queue.active = None;
                Some((id, index))
            }
            _ => None,
        };
        let pending: Vec<(BroadcastId, usize)> = queue.pending.iter().copied().collect();
        if let Some((id, index)) = failed {
            self.set_state(
                id,
                index,
                State::Failure,
                "onApplicationCleanupLocked",
                &mut out,
            );
        }
        for (id, index) in pending {
            if self.is_registered(id, index) {
                self.set_state(id, index, State::Skipped, "process died", &mut out);
            }
        }
        self.drive(policy, &mut out);
        out
    }

    /// The cold start for `key` failed before attach (startProcessLocked returned nothing, or the start timed out: onApplicationTimeoutLocked): its delivery fails and the slot is released.
    pub fn start_failed(
        &mut self,
        key: &ProcKey,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        if self.cold.as_ref() != Some(key) {
            return out;
        }
        self.cold = None;
        let starting = self.queues.get_mut(key).and_then(|q| match q.active {
            Some(Active::Starting { id, index, .. }) => {
                q.active = None;
                Some((id, index))
            }
            _ => None,
        });
        if let Some((id, index)) = starting {
            self.set_state(
                id,
                index,
                State::Failure,
                "startProcessLocked failed",
                &mut out,
            );
        }
        self.drive(policy, &mut out);
        out
    }

    /// The cold-start abandon timer for `key` fired (BroadcastQueueImpl.checkPendingColdStartValidityLocked, :620-640; the 5 min of BroadcastProcessQueue.hasProcessStartInitiationTimedout).
    /// A start that attached, or a stale `token`, does nothing. The first abandon of a receiver re-enqueues it (clearInvalidPendingColdStart); the second fails it.
    pub fn cold_start_timeout(
        &mut self,
        key: &ProcKey,
        token: u64,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        if self.cold.as_ref() != Some(key) {
            return out;
        }
        let Some(Active::Starting {
            id,
            index,
            token: current,
        }) = self.queues.get(key).and_then(|q| q.active)
        else {
            return out;
        };
        if current != token {
            return out;
        }
        self.cold = None;
        if let Some(q) = self.queues.get_mut(key) {
            q.active = None;
        }
        out.push(Action::ColdStartAbandoned { key: key.clone() });
        let retry = match self.records.get_mut(&id) {
            Some(rec) if !rec.retried[index] => {
                rec.retried[index] = true;
                true
            }
            _ => false,
        };
        if retry {
            if let Some(q) = self.queues.get_mut(key) {
                q.pending.push_front((id, index));
            }
        } else {
            self.set_state(id, index, State::Failure, "cold start abandoned", &mut out);
        }
        self.drive(policy, &mut out);
        out
    }

    /// The in-flight receiver of process `pid` finished (finishReceiverLocked). An ordered result is stored; an abort skips the rest unless NO_ABORT.
    pub fn finish(
        &mut self,
        pid: i32,
        f: Finish<E>,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        let Some((id, index)) = self.in_flight(pid) else {
            return out;
        };
        self.clear_active(pid);
        let Some(rec) = self.records.get_mut(&id) else {
            return out;
        };
        if rec.ordered {
            rec.result = f.result;
            if rec.facts.flags & FLAG_RECEIVER_NO_ABORT == 0 {
                rec.result_abort = f.abort;
            }
        }
        let aborted = rec.result_abort;
        let n = rec.state.len();
        self.set_state(id, index, State::Delivered, "remote app", &mut out);
        if aborted {
            for later in index + 1..n {
                self.set_state(id, later, State::Skipped, "resultAbort", &mut out);
            }
        }
        self.drive(policy, &mut out);
        out
    }

    /// The delivery timer for (`id`, `index`) in process `pid` fired (deliveryTimeoutLocked). A timer for a delivery that already ended does nothing.
    pub fn timeout(
        &mut self,
        pid: i32,
        id: BroadcastId,
        index: usize,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        if self.in_flight(pid) != Some((id, index)) {
            return out;
        }
        self.clear_active(pid);
        self.set_state(id, index, State::Timeout, "deliveryTimeout", &mut out);
        self.drive(policy, &mut out);
        out
    }

    /// Force-stop of `package` (cleanupDisabledPackageReceiversLocked): its manifest deliveries still waiting are skipped. A delivery in flight ends with its process.
    pub fn drop_package(
        &mut self,
        package: &str,
        policy: &mut Policy<'_, R>,
    ) -> Vec<Action<R, E, P>> {
        let mut out = Vec::new();
        let waiting: Vec<(BroadcastId, usize)> = self
            .queues
            .values()
            .flat_map(|q| q.pending.iter().copied())
            .filter(|&(id, index)| {
                self.records.get(&id).is_some_and(|r| {
                    r.state[index] == State::Pending
                        && matches!(&r.receivers[index].target, Target::Manifest(c) if c.package == package)
                })
            })
            .collect();
        for (id, index) in waiting {
            self.set_state(id, index, State::Skipped, "package force-stopped", &mut out);
        }
        self.drive(policy, &mut out);
        out
    }

    /// Delivery (`id`, `index`) is in flight in process `pid`: a timer for it is current (a stale one is ignored by [`Self::timeout`]).
    pub fn delivering(&self, pid: i32, id: BroadcastId, index: usize) -> bool {
        self.in_flight(pid) == Some((id, index))
    }

    /// Some delivery is active or waiting in the queue of `key`: the process is wanted (a manifest receiver holds it, S5's end rule reads this).
    pub fn holds(&self, key: &ProcKey) -> bool {
        self.queues
            .get(key)
            .is_some_and(|q| q.active.is_some() || !q.pending.is_empty())
    }

    fn new_token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    /// Runs every process queue until nothing moves, then drops the idle ones.
    fn drive(&mut self, policy: &mut Policy<'_, R>, out: &mut Vec<Action<R, E, P>>) {
        loop {
            let keys: Vec<ProcKey> = self.queues.keys().cloned().collect();
            let mut progressed = false;
            for key in &keys {
                while self.run_queue(key, policy, out) {
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        self.queues
            .retain(|_, q| q.pid.is_some() || q.active.is_some() || !q.pending.is_empty());
    }

    /// One step of process queue `key`: a delivery goes out, a process starts, or an item is skipped or dropped. False when nothing can move.
    fn run_queue(
        &mut self,
        key: &ProcKey,
        policy: &mut Policy<'_, R>,
        out: &mut Vec<Action<R, E, P>>,
    ) -> bool {
        let Some(queue) = self.queues.get(key) else {
            return false;
        };
        let (active, pid, head) = (queue.active, queue.pid, queue.pending.front().copied());
        match active {
            Some(Active::InFlight(..)) => return false,
            Some(Active::Starting { id, index, .. }) => {
                let Some(pid) = pid else {
                    return false;
                };
                // attached: the cold start's delivery goes out after the policy's second look (BroadcastQueueImpl.scheduleReceiverWarmLocked)
                self.set_active(key, None);
                match self.policy_skip(id, index, policy) {
                    Some(reason) => self.set_state(id, index, State::Skipped, &reason, out),
                    None => self.send(key, pid, id, index, out),
                }
                return true;
            }
            None => {}
        }
        let Some((id, index)) = head else {
            return false;
        };
        // a terminal receiver, or one whose broadcast is gone, leaves the queue
        if self.state_of(id, index) != Some(State::Pending) {
            self.pop(key);
            return true;
        }
        if self.blocked(id, index) {
            return false;
        }
        // one cold start at a time: another queue holds the slot
        if pid.is_none() && self.cold.is_some() {
            return false;
        }
        self.pop(key);
        if let Some(reason) = self.policy_skip(id, index, policy) {
            self.set_state(id, index, State::Skipped, &reason, out);
            return true;
        }
        match pid {
            Some(pid) => self.send(key, pid, id, index, out),
            None if self.is_registered(id, index) => {
                self.set_state(
                    id,
                    index,
                    State::Skipped,
                    "BroadcastFilter for cold app",
                    out,
                );
            }
            None => {
                let Some(Target::Manifest(component)) = self.target_of(id, index) else {
                    return true;
                };
                let token = self.new_token();
                self.cold = Some(key.clone());
                self.set_active(key, Some(Active::Starting { id, index, token }));
                out.push(Action::ArmColdStart {
                    key: key.clone(),
                    token,
                    after_ms: self.hw.scale(PENDING_COLD_START_ABANDON_BASE_MS),
                });
                out.push(Action::StartProcess {
                    key: key.clone(),
                    component,
                });
            }
        }
        true
    }

    /// Sends (`id`, `index`) to `pid`. An assumed delivery is done at once; any other is in flight until finish or timeout.
    fn send(
        &mut self,
        key: &ProcKey,
        pid: i32,
        id: BroadcastId,
        index: usize,
        out: &mut Vec<Action<R, E, P>>,
    ) {
        let assume = self.assumed(id, index);
        let Some(rec) = self.records.get(&id) else {
            return;
        };
        let after_ms = if rec.facts.flags & FLAG_RECEIVER_FOREGROUND != 0 {
            self.hw.scale(BROADCAST_FG_TIMEOUT_BASE_MS)
        } else {
            self.hw.scale(BROADCAST_BG_TIMEOUT_BASE_MS)
        };
        let job = Job {
            id,
            index,
            receiver: rec.receivers[index].target.clone(),
            ordered: rec.ordered,
            assume_delivered: assume,
            result: rec.result.clone(),
            payload: rec.payload.clone(),
        };
        out.push(Action::Send { pid, job });
        if assume {
            self.set_state(id, index, State::Delivered, "assuming delivered", out);
        } else {
            self.set_active(key, Some(Active::InFlight(id, index)));
            out.push(Action::ArmTimeout {
                pid,
                id,
                index,
                after_ms,
            });
        }
    }

    /// Sets a receiver's terminal state and advances the beyond count. Completes the broadcast when every receiver is terminal.
    fn set_state(
        &mut self,
        id: BroadcastId,
        index: usize,
        state: State,
        reason: &str,
        out: &mut Vec<Action<R, E, P>>,
    ) {
        let Some(rec) = self.records.get_mut(&id) else {
            return;
        };
        if rec.state[index] != State::Pending {
            return;
        }
        rec.state[index] = state;
        if state == State::Skipped {
            out.push(Action::Skip {
                id,
                index,
                reason: reason.to_owned(),
            });
        }
        while rec.beyond < rec.state.len() && rec.state[rec.beyond] != State::Pending {
            rec.beyond += 1;
        }
        if rec.beyond == rec.state.len() {
            self.complete(id, out);
        }
    }

    /// Every receiver is terminal: the final result goes to resultTo (scheduleResultTo; a dead resultTo process gets nothing) and the broadcast is retired.
    fn complete(&mut self, id: BroadcastId, out: &mut Vec<Action<R, E, P>>) {
        let Some(rec) = self.records.remove(&id) else {
            return;
        };
        if let Some((handle, key)) = rec.result_to
            && let Some(pid) = self.queues.get(&key).and_then(|q| q.pid)
        {
            out.push(Action::ResultTo {
                pid,
                handle,
                result: rec.result,
                payload: rec.payload,
            });
        }
        out.push(Action::Retired { id });
    }

    fn policy_skip(
        &mut self,
        id: BroadcastId,
        index: usize,
        policy: &mut Policy<'_, R>,
    ) -> Option<String> {
        let rec = self.records.get(&id)?;
        let (facts, receiver) = (rec.facts.clone(), rec.receivers[index].clone());
        policy(&facts, &receiver)
    }

    fn key_of(&self, pid: i32) -> Option<ProcKey> {
        self.queues
            .iter()
            .find(|(_, q)| q.pid == Some(pid))
            .map(|(k, _)| k.clone())
    }

    /// The receiver in flight in process `pid`, if any.
    fn in_flight(&self, pid: i32) -> Option<(BroadcastId, usize)> {
        match self.queues.values().find(|q| q.pid == Some(pid))?.active? {
            Active::InFlight(id, index) => Some((id, index)),
            Active::Starting { .. } => None,
        }
    }

    fn clear_active(&mut self, pid: i32) {
        if let Some(q) = self.queues.values_mut().find(|q| q.pid == Some(pid)) {
            q.active = None;
        }
    }

    fn set_active(&mut self, key: &ProcKey, active: Option<Active>) {
        self.queues.entry(key.clone()).or_default().active = active;
    }

    fn pop(&mut self, key: &ProcKey) {
        if let Some(q) = self.queues.get_mut(key) {
            q.pending.pop_front();
        }
    }

    fn state_of(&self, id: BroadcastId, index: usize) -> Option<State> {
        self.records.get(&id).map(|r| r.state[index])
    }

    fn blocked(&self, id: BroadcastId, index: usize) -> bool {
        self.records
            .get(&id)
            .is_some_and(|r| r.beyond < r.blocked_until[index])
    }

    fn is_registered(&self, id: BroadcastId, index: usize) -> bool {
        self.records
            .get(&id)
            .is_some_and(|r| matches!(r.receivers[index].target, Target::Registered(_)))
    }

    fn target_of(&self, id: BroadcastId, index: usize) -> Option<Target<R>> {
        self.records
            .get(&id)
            .map(|r| r.receivers[index].target.clone())
    }

    /// BroadcastRecord.isAssumedDelivered (:271): a registered receiver of an unordered broadcast with no resultTo.
    fn assumed(&self, id: BroadcastId, index: usize) -> bool {
        self.records.get(&id).is_some_and(|r| {
            !r.ordered
                && r.result_to.is_none()
                && matches!(r.receivers[index].target, Target::Registered(_))
        })
    }
}

/// BroadcastRecord.calculateBlockedUntilBeyondCount (:786-878), limit-priority-scope branch (flag ENABLED at the pin). Entry i = the beyond count receiver i waits for; 0 = never waits (AOSP -1).
/// `change_enabled[i]` = the change applies to receiver i's app; `priorities` and `change_enabled` have one entry per receiver.
/// Ordered: receiver i waits for 0..i (:790-795). Unordered with every change enabled: nothing waits (:850-854), so priority orders only inside one process queue.
pub fn blocked_until_beyond_count(
    priorities: &[i32],
    change_enabled: &[bool],
    ordered: bool,
) -> Vec<usize> {
    let n = priorities.len();
    if ordered {
        return (0..n).collect();
    }
    if n == 0 {
        return Vec::new();
    }
    // AOSP's int arithmetic with -1 as "none"; blocked values are beyond counts, cast at the end
    let mut blocked = vec![0_isize; n];
    let mut current_priority = priorities[0];
    let mut current_disabled: isize = if change_enabled[0] { -1 } else { 0 };
    let mut last_priority = 0;
    let mut last_priority_index: isize = -1;
    let mut last_disabled: isize = -1;
    for (i, (&priority, &enabled)) in priorities.iter().zip(change_enabled).enumerate().skip(1) {
        if priority != current_priority {
            if current_disabled != -1 {
                last_disabled = current_disabled;
            }
            last_priority = current_priority;
            last_priority_index = i as isize - 1;
            current_priority = priority;
            current_disabled = -1;
        }
        if !enabled {
            current_disabled = i as isize;
            blocked[i] = last_priority_index + 1;
        } else if priority != last_priority && last_disabled != -1 {
            blocked[i] = last_disabled + 1;
        }
    }
    if last_priority_index == -1 || (last_disabled == -1 && current_disabled == -1) {
        blocked.fill(-1);
    }
    blocked
        .into_iter()
        .map(|b| usize::try_from(b).unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast::{
        FLAG_RECEIVER_FOREGROUND, FLAG_RECEIVER_INCLUDE_BACKGROUND, FLAG_RECEIVER_NO_ABORT,
        disallow_background_start,
    };

    type D = Dispatcher<u32, (), ()>;
    type Act = Action<u32, (), ()>;

    const APP_UID: u32 = 10_123;

    /// The policy of a dispatcher with no restriction.
    fn allow(_: &Facts, _: &Receiver<u32>) -> Option<String> {
        None
    }

    fn key(name: &str) -> ProcKey {
        ProcKey {
            name: name.into(),
            uid: APP_UID,
        }
    }

    fn manifest(name: &str, priority: i32) -> Receiver<u32> {
        Receiver {
            target: Target::Manifest(Component {
                package: name.into(),
                class: ".R".into(),
            }),
            priority,
            proc: key(name),
        }
    }

    fn registered(handle: u32, name: &str) -> Receiver<u32> {
        Receiver {
            target: Target::Registered(handle),
            priority: 0,
            proc: key(name),
        }
    }

    /// Activity.RESULT_OK (-1) with no data or extras: the initial result of a sendOrderedBroadcast.
    fn initial() -> Outcome<()> {
        Outcome {
            code: -1,
            data: None,
            extras: None,
        }
    }

    fn result(code: i32, data: &str) -> Outcome<()> {
        Outcome {
            code,
            data: Some(data.into()),
            extras: None,
        }
    }

    fn finish(code: i32, data: &str, abort: bool) -> Finish<()> {
        Finish {
            result: result(code, data),
            abort,
        }
    }

    fn broadcast(receivers: Vec<Receiver<u32>>, ordered: bool) -> NewBroadcast<u32, (), ()> {
        NewBroadcast {
            facts: Facts::default(),
            receivers,
            ordered,
            result: initial(),
            result_to: None,
            payload: (),
        }
    }

    /// Makes `name` live with `pid`; nothing is pending for it yet, so nothing is sent.
    fn attach(d: &mut D, name: &str, pid: i32) {
        assert!(d.process_attached(&key(name), pid, &mut allow).is_empty());
    }

    /// (pid, receiver index) of each Send.
    fn dest(actions: &[Act]) -> Vec<(i32, usize)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { pid, job } => Some((*pid, job.index)),
                _ => None,
            })
            .collect()
    }

    /// (pid, receiver index, result code, result data) of each Send.
    fn sent_results(actions: &[Act]) -> Vec<(i32, usize, i32, Option<String>)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { pid, job } => {
                    Some((*pid, job.index, job.result.code, job.result.data.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// (pid, receiver index, delay) of each ArmTimeout.
    fn timers(actions: &[Act]) -> Vec<(i32, usize, u64)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ArmTimeout {
                    pid,
                    index,
                    after_ms,
                    ..
                } => Some((*pid, *index, *after_ms)),
                _ => None,
            })
            .collect()
    }

    /// Process names of each StartProcess.
    fn started(actions: &[Act]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::StartProcess { key, .. } => Some(key.name.clone()),
                _ => None,
            })
            .collect()
    }

    /// The token of each ArmColdStart.
    fn cold_tokens(actions: &[Act]) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::ArmColdStart { token, .. } => Some(*token),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn blocked_until_follows_ordering_and_limit_priority_scope() {
        // ordered: receiver i waits for 0..i (BroadcastRecord.java:790-795)
        assert_eq!(
            blocked_until_beyond_count(&[10, 0, 0], &[true; 3], true),
            [0, 1, 2]
        );
        // unordered, every change enabled: the tail fills -1 (:850-854), nothing waits; priority orders within a process only
        assert_eq!(
            blocked_until_beyond_count(&[10, 0, 0], &[true; 3], false),
            [0, 0, 0]
        );
        // change disabled for receiver 1 (lower tranche): it waits for receiver 0; receiver 2 shares 1's tranche and is not blocked
        assert_eq!(
            blocked_until_beyond_count(&[10, 0, 0], &[true, false, true], false),
            [0, 1, 0]
        );
        // change disabled for receiver 0: the lower tranche waits for it
        assert_eq!(
            blocked_until_beyond_count(&[10, 5, 5], &[false, true, true], false),
            [0, 1, 1]
        );
        assert_eq!(
            blocked_until_beyond_count(&[], &[], true),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn ordered_receivers_run_one_at_a_time_and_resultto_gets_the_final_result() {
        let mut d = D::default();
        for (name, pid) in [("p1", 101), ("p2", 102), ("p3", 103), ("sender", 90)] {
            attach(&mut d, name, pid);
        }
        let mut b = broadcast(
            vec![manifest("p1", 10), manifest("p2", 0), manifest("p3", 0)],
            true,
        );
        b.result_to = Some((9, key("sender")));
        let (id, a) = d.enqueue(b, &mut allow);
        assert_eq!(dest(&a), [(101, 0)]);
        assert_eq!(timers(&a), [(101, 0, BROADCAST_BG_TIMEOUT_BASE_MS)]);
        let a = d.finish(101, finish(0, "a", false), &mut allow);
        assert_eq!(sent_results(&a), [(102, 1, 0, Some("a".into()))]);
        let a = d.finish(102, finish(0, "b", false), &mut allow);
        assert_eq!(sent_results(&a), [(103, 2, 0, Some("b".into()))]);
        let a = d.finish(103, finish(5, "c", false), &mut allow);
        assert_eq!(
            a,
            [
                Action::ResultTo {
                    pid: 90,
                    handle: 9,
                    result: result(5, "c"),
                    payload: (),
                },
                Action::Retired { id },
            ]
        );
    }

    #[test]
    fn abort_skips_the_later_receivers_and_resultto_gets_the_aborters_result() {
        let mut d = D::default();
        for (name, pid) in [("p1", 101), ("p2", 102), ("p3", 103), ("sender", 90)] {
            attach(&mut d, name, pid);
        }
        let mut b = broadcast(
            vec![manifest("p1", 10), manifest("p2", 0), manifest("p3", 0)],
            true,
        );
        b.result_to = Some((9, key("sender")));
        let (id, a) = d.enqueue(b, &mut allow);
        assert_eq!(dest(&a), [(101, 0)]);
        let a = d.finish(101, finish(0, "a", false), &mut allow);
        assert_eq!(dest(&a), [(102, 1)]);
        // receiver 1 aborts: receiver 2 is skipped, never sent; the result is receiver 1's
        let a = d.finish(102, finish(7, "x", true), &mut allow);
        assert_eq!(
            a,
            [
                Action::Skip {
                    id,
                    index: 2,
                    reason: "resultAbort".into(),
                },
                Action::ResultTo {
                    pid: 90,
                    handle: 9,
                    result: result(7, "x"),
                    payload: (),
                },
                Action::Retired { id },
            ]
        );
    }

    #[test]
    fn no_abort_flag_ignores_the_abort_and_keeps_the_result() {
        let mut d = D::default();
        for (name, pid) in [("p1", 101), ("p2", 102), ("p3", 103)] {
            attach(&mut d, name, pid);
        }
        let mut b = broadcast(
            vec![manifest("p1", 0), manifest("p2", 0), manifest("p3", 0)],
            true,
        );
        b.facts.flags = FLAG_RECEIVER_NO_ABORT;
        let (_, a) = d.enqueue(b, &mut allow);
        assert_eq!(dest(&a), [(101, 0)]);
        let a = d.finish(101, finish(0, "a", false), &mut allow);
        assert_eq!(dest(&a), [(102, 1)]);
        // receiver 1 asks to abort, but NO_ABORT: receiver 2 still goes, with receiver 1's result
        let a = d.finish(102, finish(7, "x", true), &mut allow);
        assert_eq!(sent_results(&a), [(103, 2, 7, Some("x".into()))]);
        assert!(!a.iter().any(|x| matches!(x, Action::Skip { .. })));
    }

    #[test]
    fn unordered_receivers_run_in_parallel_across_processes_and_in_priority_order_within_one() {
        let mut d = D::default();
        attach(&mut d, "p1", 101);
        attach(&mut d, "p2", 102);
        // list order is priority order: A (p1, 10), B (p2, 5), C (p1, 0)
        let (id, a) = d.enqueue(
            broadcast(
                vec![manifest("p1", 10), manifest("p2", 5), manifest("p1", 0)],
                false,
            ),
            &mut allow,
        );
        // B does not wait for A; C waits for A in their shared process
        assert_eq!(dest(&a), [(101, 0), (102, 1)]);
        assert_eq!(
            timers(&a),
            [
                (101, 0, BROADCAST_BG_TIMEOUT_BASE_MS),
                (102, 1, BROADCAST_BG_TIMEOUT_BASE_MS)
            ]
        );
        let a = d.finish(101, finish(0, "", false), &mut allow);
        assert_eq!(dest(&a), [(101, 2)]);
        assert!(d.finish(102, finish(0, "", false), &mut allow).is_empty());
        let a = d.finish(101, finish(0, "", false), &mut allow);
        assert_eq!(a, [Action::Retired { id }]);
    }

    #[test]
    fn registered_receivers_of_unordered_broadcasts_do_not_hold_their_process() {
        let mut d = D::default();
        attach(&mut d, "p1", 101);
        let (_, a) = d.enqueue(
            broadcast(vec![registered(7, "p1"), manifest("p1", 0)], false),
            &mut allow,
        );
        // the registered receiver is assumed delivered: the manifest one goes out in the same step, and only it has a timer
        assert_eq!(dest(&a), [(101, 0), (101, 1)]);
        assert!(matches!(&a[0], Action::Send { job, .. } if job.assume_delivered));
        assert_eq!(timers(&a), [(101, 1, BROADCAST_BG_TIMEOUT_BASE_MS)]);
        // the manifest delivery in flight holds the process; the assumed registered one held nothing
        assert!(d.holds(&key("p1")));
    }

    #[test]
    fn second_cold_start_waits_for_the_first_attach() {
        let mut d = D::default();
        let (_, a) = d.enqueue(
            broadcast(vec![manifest("p1", 0), manifest("p2", 0)], false),
            &mut allow,
        );
        assert_eq!(started(&a), ["p1"]);
        assert!(dest(&a).is_empty());
        // p1 attaches: its delivery goes out, then the slot passes to p2
        let a = d.process_attached(&key("p1"), 201, &mut allow);
        assert_eq!(dest(&a), [(201, 0)]);
        assert_eq!(started(&a), ["p2"]);
        let a = d.process_attached(&key("p2"), 202, &mut allow);
        assert_eq!(dest(&a), [(202, 1)]);
    }

    #[test]
    fn failed_cold_start_fails_its_delivery_and_releases_the_slot() {
        let mut d = D::default();
        let (id, a) = d.enqueue(
            broadcast(vec![manifest("p1", 0), manifest("p2", 0)], false),
            &mut allow,
        );
        assert_eq!(started(&a), ["p1"]);
        let a = d.start_failed(&key("p1"), &mut allow);
        assert_eq!(started(&a), ["p2"]);
        assert!(dest(&a).is_empty());
        let a = d.process_attached(&key("p2"), 202, &mut allow);
        assert_eq!(dest(&a), [(202, 1)]);
        assert_eq!(
            d.finish(202, finish(0, "", false), &mut allow),
            [Action::Retired { id }]
        );
    }

    #[test]
    fn cold_start_abandon_reenqueues_once_then_fails_and_releases_the_slot() {
        let mut d = D::default();
        let (id, a) = d.enqueue(
            broadcast(vec![manifest("p1", 0), manifest("p2", 0)], false),
            &mut allow,
        );
        assert_eq!(started(&a), ["p1"]);
        // the start armed its abandon with the 5 min of BroadcastConstants
        let tokens = cold_tokens(&a);
        assert_eq!(tokens.len(), 1);
        assert!(a.iter().any(|x| matches!(
            x,
            Action::ArmColdStart { after_ms, .. } if *after_ms == PENDING_COLD_START_ABANDON_BASE_MS
        )));
        // p1 never attaches: its slot goes, the receiver is re-enqueued and p1 is ended for a new start; p2 does not wait for p1's retry
        let a = d.cold_start_timeout(&key("p1"), tokens[0], &mut allow);
        assert_eq!(a[0], Action::ColdStartAbandoned { key: key("p1") });
        assert_eq!(started(&a), ["p1"]);
        let retry = cold_tokens(&a);
        assert_eq!(retry.len(), 1);
        assert_ne!(retry[0], tokens[0]);
        // a stale timer of the first start does nothing
        assert!(
            d.cold_start_timeout(&key("p1"), tokens[0], &mut allow)
                .is_empty()
        );
        // the retry is abandoned too: the second time the receiver fails (no Skip: a failure is not a skip), and p2 starts
        let a = d.cold_start_timeout(&key("p1"), retry[0], &mut allow);
        assert_eq!(a[0], Action::ColdStartAbandoned { key: key("p1") });
        assert_eq!(started(&a), ["p2"]);
        assert!(!a.iter().any(|x| matches!(x, Action::Retired { .. })));
        let a = d.process_attached(&key("p2"), 202, &mut allow);
        assert_eq!(dest(&a), [(202, 1)]);
        assert_eq!(
            d.finish(202, finish(0, "", false), &mut allow),
            [Action::Retired { id }]
        );
    }

    #[test]
    fn a_cold_start_that_attaches_is_not_abandoned() {
        let mut d = D::default();
        let (_, a) = d.enqueue(broadcast(vec![manifest("p1", 0)], false), &mut allow);
        let tokens = cold_tokens(&a);
        let a = d.process_attached(&key("p1"), 201, &mut allow);
        assert_eq!(dest(&a), [(201, 0)]);
        assert!(
            d.cold_start_timeout(&key("p1"), tokens[0], &mut allow)
                .is_empty()
        );
    }

    #[test]
    fn process_death_fails_the_delivery_and_the_ordered_chain_goes_on() {
        let mut d = D::default();
        attach(&mut d, "p1", 101);
        attach(&mut d, "p2", 102);
        let (id, a) = d.enqueue(
            broadcast(vec![manifest("p1", 0), manifest("p2", 0)], true),
            &mut allow,
        );
        assert_eq!(dest(&a), [(101, 0)]);
        // receiver 0 failed, so receiver 1 is no longer blocked
        let a = d.process_died(101, &mut allow);
        assert_eq!(dest(&a), [(102, 1)]);
        assert_eq!(
            d.finish(102, finish(0, "", false), &mut allow),
            [Action::Retired { id }]
        );
    }

    /// The multiplier scales what the dispatcher arms: a foreground delivery's timer and a cold start's abandon, AOSP's literals x 2.
    #[test]
    fn hw_multiplier_scales_the_armed_timers() {
        let mut d = D::with_hw(HwTimeouts::from_prop(Some("2")));
        attach(&mut d, "p1", 101);
        let mut b = broadcast(vec![manifest("p1", 0), manifest("p1", 0)], true);
        b.facts.flags = FLAG_RECEIVER_FOREGROUND;
        let (_, a) = d.enqueue(b, &mut allow);
        // foreground: 10 s x 2 (ActivityManagerService.java:584)
        assert_eq!(timers(&a), [(101, 0, 2 * 10 * 1000)]);
        let mut d = D::with_hw(HwTimeouts::from_prop(Some("2")));
        let (_, a) = d.enqueue(
            broadcast(vec![manifest("p1", 0), manifest("p2", 0)], false),
            &mut allow,
        );
        // the cold start's abandon: 5 min x 2 (BroadcastConstants.java:295)
        assert!(a.iter().any(|x| matches!(
            x,
            Action::ArmColdStart { after_ms, .. } if *after_ms == 2 * 5 * 60 * 1000
        )));
    }

    #[test]
    fn timeout_ends_the_delivery_and_a_stale_timer_does_nothing() {
        let mut d = D::default();
        attach(&mut d, "p1", 101);
        let mut b = broadcast(vec![manifest("p1", 0), manifest("p1", 0)], true);
        b.facts.flags = FLAG_RECEIVER_FOREGROUND;
        let (id, a) = d.enqueue(b, &mut allow);
        // foreground: 10 s (ActivityManagerService.java:584)
        assert_eq!(timers(&a), [(101, 0, BROADCAST_FG_TIMEOUT_BASE_MS)]);
        let a = d.timeout(101, id, 0, &mut allow);
        assert_eq!(dest(&a), [(101, 1)]);
        assert_eq!(timers(&a), [(101, 1, BROADCAST_FG_TIMEOUT_BASE_MS)]);
        // the timer of receiver 0 fires again after it ended
        assert!(d.timeout(101, id, 0, &mut allow).is_empty());
        assert_eq!(
            d.finish(101, finish(0, "", false), &mut allow),
            [Action::Retired { id }]
        );
    }

    #[test]
    fn force_stop_skips_the_package_waiting_deliveries_only() {
        let mut d = D::default();
        attach(&mut d, "p1", 101);
        attach(&mut d, "p2", 102);
        // receiver 0 is in flight in p1; receiver 1 waits behind it in p1's queue; receiver 2 goes to p2
        let (id, a) = d.enqueue(
            broadcast(
                vec![manifest("p1", 0), manifest("p1", 0), manifest("p2", 0)],
                false,
            ),
            &mut allow,
        );
        assert_eq!(dest(&a), [(101, 0), (102, 2)]);
        let a = d.drop_package("p1", &mut allow);
        assert_eq!(
            a,
            [Action::Skip {
                id,
                index: 1,
                reason: "package force-stopped".into(),
            }]
        );
        // the delivery in flight still holds p1 until its process ends
        assert!(d.holds(&key("p1")));
        assert!(d.finish(101, finish(0, "", false), &mut allow).is_empty());
        assert!(!d.holds(&key("p1")));
        assert_eq!(
            d.finish(102, finish(0, "", false), &mut allow),
            [Action::Retired { id }]
        );
    }

    #[test]
    fn implicit_manifest_receivers_of_restricted_apps_are_skipped_unless_include_background() {
        // targetSdk 34 >= 26 is DELAYED_RIGID (AMS:6279-6340) and restricted; targetSdk 25 is not
        let mut policy = |facts: &Facts, r: &Receiver<u32>| {
            let target_sdk = if r.proc.name == "a34" { 34 } else { 25 };
            (target_sdk >= 26
                && disallow_background_start(facts.flags, facts.explicit, facts.signature_perms))
            .then(|| "Background execution not allowed".to_owned())
        };
        let mut d = D::default();
        attach(&mut d, "a34", 101);
        attach(&mut d, "b25", 102);
        let (id, a) = d.enqueue(
            broadcast(vec![manifest("a34", 0), manifest("b25", 0)], false),
            &mut policy,
        );
        assert!(a.contains(&Action::Skip {
            id,
            index: 0,
            reason: "Background execution not allowed".into(),
        }));
        assert_eq!(dest(&a), [(102, 1)]);
        assert_eq!(
            d.finish(102, finish(0, "", false), &mut policy),
            [Action::Retired { id }]
        );
        // the same implicit broadcast with INCLUDE_BACKGROUND (BroadcastController.java:1106): both delivered
        let mut b = broadcast(vec![manifest("a34", 0), manifest("b25", 0)], false);
        b.facts.flags = FLAG_RECEIVER_INCLUDE_BACKGROUND;
        let (_, a) = d.enqueue(b, &mut policy);
        assert_eq!(dest(&a), [(101, 0), (102, 1)]);
    }

    #[test]
    fn registered_receiver_of_a_cold_process_is_skipped_without_a_start() {
        let mut d = D::default();
        let (id, a) = d.enqueue(broadcast(vec![registered(5, "p9")], false), &mut allow);
        assert!(started(&a).is_empty());
        assert_eq!(
            a,
            [
                Action::Skip {
                    id,
                    index: 0,
                    reason: "BroadcastFilter for cold app".into(),
                },
                Action::Retired { id },
            ]
        );
    }

    #[test]
    fn resultto_reaches_only_a_live_process() {
        let mut d = D::default();
        let mut b = broadcast(Vec::new(), true);
        b.result_to = Some((9, key("sender")));
        let (id, a) = d.enqueue(b, &mut allow);
        assert_eq!(a, [Action::Retired { id }]);
        attach(&mut d, "sender", 90);
        let mut b = broadcast(Vec::new(), true);
        b.result_to = Some((9, key("sender")));
        let (id, a) = d.enqueue(b, &mut allow);
        assert_eq!(
            a,
            [
                Action::ResultTo {
                    pid: 90,
                    handle: 9,
                    result: initial(),
                    payload: (),
                },
                Action::Retired { id },
            ]
        );
    }
}
