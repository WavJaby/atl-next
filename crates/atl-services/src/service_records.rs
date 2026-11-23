// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActiveServices.java, ServiceRecord.java, ActivityManagerConstants.java and core/java/android/app/Service.java
//! ActiveServices' service records as plain state: one record per component (ServiceRecord, reduced). Bound half: bindServiceLocked, publishServiceLocked, removeConnectionLocked, unbindFinishedLocked, bringDownServiceLocked. Started half: startServiceInnerLocked, stopServiceLocked, stopServiceTokenLocked, serviceDoneExecutingLocked (START), sendServiceArgsLocked. Restart half: scheduleServiceRestartLocked, performServiceRestartLocked, unscheduleServiceRestartLocked, killServicesLocked, and the restart kick in attachApplicationLocked.
//! No binder here: each step returns the [`Call`]s the activity service sends after releasing its lock (all are oneway: IApplicationThread is a oneway interface, IServiceConnection.connected oneway), in order. A scheduled restart is returned as a [`Restart`]; the caller's timer calls [`Services::restart`] at its time.
//! Records are keyed by component (ActiveServices' ServiceMap by ComponentName, one user); bindings by Intent.FilterComparison ([`IntentKey`]). Times are ms on the activity service's clock (`Inner::now_ms`, the clock of am_handler and uid_state): only differences and order matter.
//! A record waiting for its process (`pending`, AS mPendingServices) is not brought down by a stop or an unbind; attach starts it and drops it if nothing needs it any more (AS bringDownServiceIfNeededLocked :6290, attachApplicationLocked :6914-6925).
//! Not modelled: instanceName / isolated / sdk-sandbox services, per-client AppBindRecord (a binding's clients are its connections), persistent apps (FLAG_PERSISTENT restarts at once), crash counts (BOUND_SERVICE_MAX_CRASH_RETRY, crashCount), short FGS (canStopIfKilled's isShortFgs), restartCount (dump only), mEarliestRestartTime and the reschedule of pending restarts (memory-pressure delay: normal level, 0), per-package backoff off (always on), delayed stops and the background-caller delayed start (mDelayedStartList, counted in active_services.rs), startCommandResult, ANR timeouts (serviceDoneExecuting is accepted, not waited for), oom-adj and process-state accounting.

use crate::broadcast::IntentKey;

/// Context.BIND_AUTO_CREATE.
pub const BIND_AUTO_CREATE: i64 = 0x0001;

/// Service.START_STICKY_COMPATIBILITY (Service.java:389): the start is not kept for a restart.
pub const START_STICKY_COMPATIBILITY: i32 = 0;
/// Service.START_STICKY (:413): a restart with a null Intent when killed.
pub const START_STICKY: i32 = 1;
/// Service.START_NOT_STICKY (:437): no restart without more work.
pub const START_NOT_STICKY: i32 = 2;
/// Service.START_REDELIVER_INTENT (:452): the start is redelivered after a kill.
pub const START_REDELIVER_INTENT: i32 = 3;
/// Service.START_TASK_REMOVED_COMPLETE (:469): onTaskRemoved's done, not an onStartCommand result.
pub const START_TASK_REMOVED_COMPLETE: i32 = 1000;
/// Service.START_FLAG_REDELIVERY (:477): the start is a redelivery.
pub const START_FLAG_REDELIVERY: i32 = 0x0001;
/// Service.START_FLAG_RETRY (:484): a delivery after the first.
pub const START_FLAG_RETRY: i32 = 0x0002;

/// DEFAULT_SERVICE_RESTART_DURATION (ActivityManagerConstants.java:223): the first restart delay.
const SERVICE_RESTART_DURATION: i64 = 1_000;
/// DEFAULT_SERVICE_RESET_RUN_DURATION (:224): a kill more than this after the service's last start resets the delay.
const SERVICE_RESET_RUN_DURATION: i64 = 60_000;
/// DEFAULT_SERVICE_RESTART_DURATION_FACTOR (:225): the delay multiplier for a kill inside the reset window.
const SERVICE_RESTART_DURATION_FACTOR: i64 = 4;
/// DEFAULT_SERVICE_MIN_RESTART_TIME_BETWEEN (:226): the spacing of restarts of services that die together.
const SERVICE_MIN_RESTART_TIME_BETWEEN: i64 = 10_000;
/// DEFAULT_EXTRA_SERVICE_RESTART_DELAY_ON_NORMAL_MEM (:296): no memory-pressure model, so the normal level's extra delay.
const EXTRA_RESTART_DELAY: i64 = 0;
/// ServiceRecord.MAX_DELIVERY_COUNT (ServiceRecord.java:85): deliveries of one start before it is cancelled.
const MAX_DELIVERY_COUNT: u32 = 3;
/// ServiceRecord.MAX_DONE_EXECUTING_COUNT (:88): failed results of one start before it is cancelled.
const MAX_DONE_EXECUTING_COUNT: u32 = 6;

/// The handle types the rules carry without reading them (binder objects in the service, plain values in the tests).
pub trait Kinds {
    /// ServiceRecord: the Binder that identifies the service to its process
    type Token: Clone + PartialEq;
    /// IServiceConnection, equal when the binder is (mServiceConnections is keyed by `connection.asBinder()`)
    type Conn: Clone + PartialEq;
    /// the IBinder the service's onBind returned
    type Binder: Clone;
    /// IApplicationThread of the hosting process
    type Thread: Clone;
    /// the Intent as sent to the app
    type Intent: Clone;
    /// the ServiceInfo scheduleCreateService sends
    type Info: Clone;
}

/// One outgoing oneway call.
pub enum Call<K: Kinds> {
    /// IApplicationThread.scheduleCreateService
    Create {
        thread: K::Thread,
        token: K::Token,
        info: K::Info,
    },
    /// IApplicationThread.scheduleBindService
    Bind {
        thread: K::Thread,
        token: K::Token,
        intent: K::Intent,
        rebind: bool,
        seq: i64,
    },
    /// IApplicationThread.scheduleUnbindService
    Unbind {
        thread: K::Thread,
        token: K::Token,
        intent: K::Intent,
    },
    /// IApplicationThread.scheduleStopService
    Stop { thread: K::Thread, token: K::Token },
    /// IApplicationThread.scheduleServiceArgs (ParceledListSlice<ServiceStartArgs>): the delivered starts, in order
    Args {
        thread: K::Thread,
        token: K::Token,
        args: Vec<StartArgs<K>>,
    },
    /// IServiceConnection.connected(name, service, dead)
    Connected {
        conn: K::Conn,
        component: (String, String),
        binder: Option<K::Binder>,
        dead: bool,
    },
}

/// ServiceStartArgs: one delivered start.
pub struct StartArgs<K: Kinds> {
    pub task_removed: bool,
    pub start_id: i32,
    /// START_FLAG_REDELIVERY / START_FLAG_RETRY
    pub flags: i32,
    /// None: the sticky restart's null Intent
    pub intent: Option<K::Intent>,
}

/// ConnectionRecord, reduced.
struct Connection<K: Kinds> {
    conn: K::Conn,
    /// the client's process (binderd pid): its death removes the connection (cleanUpApplicationRecordLocked)
    client_pid: i32,
    flags: i64,
}

/// IntentBindRecord.
struct IntentBind<K: Kinds> {
    key: IntentKey,
    intent: K::Intent,
    binder: Option<K::Binder>,
    requested: bool,
    received: bool,
    has_bound: bool,
    do_rebind: bool,
    conns: Vec<Connection<K>>,
}

/// ServiceRecord.StartItem, reduced: one start, pending until delivered, delivered until its result comes back.
struct StartItem<K: Kinds> {
    /// startId (ServiceRecord.makeNextStartId)
    id: i32,
    /// None: the sticky restart's null Intent (a fake item, the start has no Intent)
    intent: Option<K::Intent>,
    task_removed: bool,
    delivery_count: u32,
    done_executing_count: u32,
    /// deliveredTime, uptime ms
    delivered_at: i64,
}

impl<K: Kinds> StartItem<K> {
    fn new(id: i32, intent: Option<K::Intent>, task_removed: bool) -> Self {
        StartItem {
            id,
            intent,
            task_removed,
            delivery_count: 0,
            done_executing_count: 0,
            delivered_at: 0,
        }
    }
}

/// ServiceRecord, reduced.
struct Record<K: Kinds> {
    token: K::Token,
    component: (String, String),
    uid: u32,
    process: String,
    info: K::Info,
    /// r.app: the hosting process (pid, thread) once scheduleCreateService was sent
    app: Option<(i32, K::Thread)>,
    bindings: Vec<IntentBind<K>>,
    /// startRequested: someone called startService and has not stopped it
    start_requested: bool,
    /// callStart: the last onStartCommand asked to be called again on restart (its result is not START_STICKY_COMPATIBILITY)
    call_start: bool,
    /// stopIfKilled: the last onStartCommand asked not to run again without more work
    stop_if_killed: bool,
    /// pendingStarts: waiting for delivery (the process attach, or the running service)
    pending_starts: Vec<StartItem<K>>,
    /// deliveredStarts: delivered, waiting for their result
    delivered_starts: Vec<StartItem<K>>,
    /// lastStartId: the id of the most recent start (stopSelf compares it)
    last_start_id: i32,
    /// restartDelay: the delay of the pending restart; 0 = no backoff running
    restart_delay: i64,
    /// restartTime: when the service was last created (base of the reset rule)
    restart_time: i64,
    /// nextRestartTime: when the pending restart fires
    next_restart_time: i64,
    /// mPendingServices: brought up, waiting for the process to attach (the record's app is None until then)
    pending: bool,
}

impl<K: Kinds> Record<K> {
    fn new(
        token: K::Token,
        component: (String, String),
        uid: u32,
        process: String,
        info: K::Info,
    ) -> Self {
        Record {
            token,
            component,
            uid,
            process,
            info,
            app: None,
            bindings: Vec::new(),
            start_requested: false,
            call_start: false,
            stop_if_killed: false,
            pending_starts: Vec::new(),
            delivered_starts: Vec::new(),
            last_start_id: 0,
            restart_delay: 0,
            restart_time: 0,
            next_restart_time: 0,
            pending: false,
        }
    }

    /// ServiceRecord.hasAutoCreateConnections
    fn has_auto_create(&self) -> bool {
        self.bindings
            .iter()
            .flat_map(|b| &b.conns)
            .any(|c| c.flags & BIND_AUTO_CREATE != 0)
    }

    /// isServiceNeededLocked: what keeps the service alive, a start or a BIND_AUTO_CREATE connection.
    fn needed(&self) -> bool {
        self.start_requested || self.has_auto_create()
    }

    /// ServiceRecord.canStopIfKilled (short FGS not modelled): the started service has no reason to run again.
    fn can_stop_if_killed(&self, start_canceled: bool) -> bool {
        self.start_requested
            && (self.stop_if_killed || start_canceled)
            && self.pending_starts.is_empty()
    }

    /// ServiceRecord.makeNextStartId: 1, 2, ..., wrapping past i32::MAX back to 1.
    fn make_next_start_id(&mut self) -> i32 {
        let next = self.last_start_id.wrapping_add(1);
        self.last_start_id = if next < 1 { 1 } else { next };
        self.last_start_id
    }

    /// ServiceRecord.findDeliveredStart(id, taskRemoved, remove=true)
    fn take_delivered(&mut self, id: i32, task_removed: bool) {
        if let Some(p) = self
            .delivered_starts
            .iter()
            .position(|s| s.id == id && s.task_removed == task_removed)
        {
            self.delivered_starts.remove(p);
        }
    }

    /// ServiceRecord.resetRestartCounter (restartCount and the earliest-time fields not modelled).
    fn reset_restart_counter(&mut self) {
        self.restart_delay = 0;
        self.restart_time = 0;
    }

    /// sendServiceArgsLocked: the pending starts go out in order as one scheduleServiceArgs; a null Intent among others is dropped.
    fn send_args(&mut self, now: i64, out: &mut Vec<Call<K>>) {
        let Some((_, thread)) = &self.app else {
            return;
        };
        let thread = thread.clone();
        let n = self.pending_starts.len();
        let mut args = Vec::with_capacity(n);
        for mut si in std::mem::take(&mut self.pending_starts) {
            if si.intent.is_none() && n > 1 {
                continue;
            }
            si.delivered_at = now;
            si.delivery_count += 1;
            let mut flags = 0;
            if si.delivery_count > 1 {
                flags |= START_FLAG_RETRY;
            }
            if si.done_executing_count > 0 {
                flags |= START_FLAG_REDELIVERY;
            }
            args.push(StartArgs {
                task_removed: si.task_removed,
                start_id: si.id,
                flags,
                intent: si.intent.clone(),
            });
            self.delivered_starts.push(si);
        }
        if !args.is_empty() {
            out.push(Call::Args {
                thread,
                token: self.token.clone(),
                args,
            });
        }
    }
}

/// What a process holds through the services ([`Services::held_by`]): a started service, a BIND_AUTO_CREATE binding (uid_state::Holdings' two fields).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    pub started: bool,
    pub bound: bool,
}

/// What the activity service must do for a bind beyond the calls.
#[derive(Debug, PartialEq, Eq)]
pub enum Host {
    /// nothing: the service runs, or the binding waits for a later BIND_AUTO_CREATE bind
    Ready,
    /// the service's process must be running for it to start: start it unless it is starting (bringUpServiceLocked -> startProcessLocked + mPendingServices)
    NeedsProcess,
}

/// A bind request after the lookup and permission check (retrieveServiceLocked passed).
pub struct Bind<K: Kinds> {
    pub component: (String, String),
    /// the service's app uid
    pub uid: u32,
    pub process: String,
    pub info: K::Info,
    pub intent: K::Intent,
    pub key: IntentKey,
    pub conn: K::Conn,
    pub client_pid: i32,
    /// the caller's uid: a bind from another uid resets the service's restart backoff
    pub caller_uid: u32,
    pub flags: i64,
}

/// A start request after the lookup and permission check (retrieveServiceLocked passed).
pub struct Start<K: Kinds> {
    pub component: (String, String),
    /// the service's app uid
    pub uid: u32,
    pub process: String,
    pub info: K::Info,
    pub intent: K::Intent,
    /// the caller's uid: a start from another uid resets the service's restart backoff
    pub caller_uid: u32,
}

/// A restart scheduled by [`Services::process_died`]: the caller's timer calls [`Services::restart`] for `component` at uptime `at` (ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    pub component: (String, String),
    pub at: i64,
}

/// onStartCommand's result is not a START_* value (serviceDoneExecutingLocked: IllegalArgumentException).
#[derive(Debug, PartialEq, Eq)]
pub struct BadResult(pub i32);

/// The valid results of onStartCommand, and onTaskRemoved's done (serviceDoneExecutingLocked START).
#[derive(Clone, Copy)]
enum DoneStart {
    Sticky,
    NotSticky,
    Redeliver,
    TaskRemoved,
}

/// The service records (ActiveServices' ServiceMap + mServiceConnections + mRestartingServices).
pub struct Services<K: Kinds> {
    records: Vec<Record<K>>,
    /// mBindServiceSeqCounter
    bind_seq: i64,
    /// mRestartingServices: components waiting for a restart, by nextRestartTime (a new one goes before the first later one)
    restarting: Vec<(String, String)>,
}

impl<K: Kinds> Default for Services<K> {
    fn default() -> Self {
        Services {
            records: Vec::new(),
            bind_seq: 0,
            restarting: Vec::new(),
        }
    }
}

impl<K: Kinds> Services<K> {
    /// requestServiceBindingLocked: a binding with clients that was not requested (or a rebind) is sent to the running service.
    fn request(seq: &mut i64, r: &mut Record<K>, bi: usize, rebind: bool, out: &mut Vec<Call<K>>) {
        let Some((_, thread)) = &r.app else { return };
        let b = &mut r.bindings[bi];
        if (!b.requested || rebind) && !b.conns.is_empty() {
            *seq += 1;
            out.push(Call::Bind {
                thread: thread.clone(),
                token: r.token.clone(),
                intent: b.intent.clone(),
                rebind,
                seq: *seq,
            });
            if !rebind {
                b.requested = true;
            }
            b.has_bound = true;
            b.do_rebind = false;
        }
    }

    /// realStartServiceLocked: scheduleCreateService, every binding's request, the sticky null start when nothing is pending, then the pending starts.
    fn real_start(
        seq: &mut i64,
        r: &mut Record<K>,
        pid: i32,
        thread: K::Thread,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) {
        out.push(Call::Create {
            thread: thread.clone(),
            token: r.token.clone(),
            info: r.info.clone(),
        });
        r.app = Some((pid, thread));
        r.pending = false;
        r.restart_time = now;
        for bi in 0..r.bindings.len() {
            Self::request(seq, r, bi, false, out);
        }
        // a started service that asked for calls on restart: onStartCommand(null) (realStartServiceLocked :6141)
        if r.start_requested && r.call_start && r.pending_starts.is_empty() {
            let id = r.make_next_start_id();
            r.pending_starts.push(StartItem::new(id, None, false));
        }
        r.send_args(now, out);
    }

    /// bringUpServiceLocked, reduced: a running service gets its pending starts; else its process is needed (NeedsProcess) or the service is created in the attached one.
    fn bring_up(
        &mut self,
        ri: usize,
        host: Option<(i32, K::Thread)>,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) -> Host {
        let seq = &mut self.bind_seq;
        let r = &mut self.records[ri];
        if r.app.is_some() {
            r.send_args(now, out);
            return Host::Ready;
        }
        match host {
            Some((pid, thread)) => {
                Self::real_start(seq, r, pid, thread, now, out);
                Host::Ready
            }
            None => {
                r.pending = true;
                Host::NeedsProcess
            }
        }
    }

    /// The record of `component`, created unstarted under a new token when absent (retrieveServiceLocked's create).
    fn record_for(
        &mut self,
        component: &(String, String),
        uid: u32,
        process: &str,
        info: K::Info,
        new_token: impl FnOnce() -> K::Token,
    ) -> usize {
        match self.records.iter().position(|r| &r.component == component) {
            Some(i) => i,
            None => {
                self.records.push(Record::new(
                    new_token(),
                    component.clone(),
                    uid,
                    process.to_owned(),
                    info,
                ));
                self.records.len() - 1
            }
        }
    }

    /// unscheduleServiceRestartLocked (force false): a start or bind cancels the restart pending for the record. The backoff resets when a restart was cancelled or the caller is another uid.
    fn unschedule(&mut self, ri: usize, caller_uid: u32) {
        if self.records[ri].restart_delay == 0 {
            return;
        }
        let key = self.records[ri].component.clone();
        let before = self.restarting.len();
        self.restarting.retain(|k| k != &key);
        let removed = self.restarting.len() != before;
        let r = &mut self.records[ri];
        if removed || caller_uid != r.uid {
            r.reset_restart_counter();
        }
    }

    /// bindServiceLocked after retrieveServiceLocked: the connection joins the record's binding for its Intent, a restart pending for the record is cancelled, BIND_AUTO_CREATE brings the service up (`host`: its attached process, if any).
    /// Then an already published binding answers at once (connected), else an unrequested one is requested. Returns what the caller must still do for the process.
    pub fn bind(
        &mut self,
        req: Bind<K>,
        host: Option<(i32, K::Thread)>,
        new_token: impl FnOnce() -> K::Token,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) -> Host {
        let ri = self.record_for(&req.component, req.uid, &req.process, req.info, new_token);
        self.unschedule(ri, req.caller_uid);
        let r = &mut self.records[ri];
        let bi = match r.bindings.iter().position(|b| b.key == req.key) {
            Some(i) => i,
            None => {
                r.bindings.push(IntentBind {
                    key: req.key,
                    intent: req.intent,
                    binder: None,
                    requested: false,
                    received: false,
                    has_bound: false,
                    do_rebind: false,
                    conns: Vec::new(),
                });
                r.bindings.len() - 1
            }
        };
        r.bindings[bi].conns.push(Connection {
            conn: req.conn.clone(),
            client_pid: req.client_pid,
            flags: req.flags,
        });
        let host_needed = if req.flags & BIND_AUTO_CREATE != 0 {
            self.bring_up(ri, host, now, out)
        } else {
            Host::Ready
        };
        let seq = &mut self.bind_seq;
        let r = &mut self.records[ri];
        if r.app.is_some() {
            let b = &r.bindings[bi];
            if b.received {
                out.push(Call::Connected {
                    conn: req.conn,
                    component: r.component.clone(),
                    binder: b.binder.clone(),
                    dead: false,
                });
                if b.conns.len() == 1 && b.do_rebind {
                    Self::request(seq, r, bi, true, out);
                }
            } else if !b.requested {
                Self::request(seq, r, bi, false, out);
            }
        }
        host_needed
    }

    /// startServiceInnerLocked after retrieveServiceLocked: the service is started with a new StartItem, a restart pending for it is cancelled. The start goes out to the running service, or with the process attach (bring_up).
    pub fn start(
        &mut self,
        req: Start<K>,
        host: Option<(i32, K::Thread)>,
        new_token: impl FnOnce() -> K::Token,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) -> Host {
        let ri = self.record_for(&req.component, req.uid, &req.process, req.info, new_token);
        self.unschedule(ri, req.caller_uid);
        let r = &mut self.records[ri];
        r.start_requested = true;
        r.call_start = false;
        let id = r.make_next_start_id();
        r.pending_starts
            .push(StartItem::new(id, Some(req.intent), false));
        self.bring_up(ri, host, now, out)
    }

    /// attachApplicationLocked: the services of `process` (uid) that wait for it start in the new process: the pending ones (mPendingServices), and a restart pending for one is kicked
    /// (AS :6938-6952 posts its restarter at once). A waiting service nothing needs any more is then brought down (AS :6914-6925).
    pub fn attach(
        &mut self,
        pid: i32,
        process: &str,
        uid: u32,
        thread: &K::Thread,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) {
        let waiting: Vec<usize> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.app.is_none()
                    && r.process == process
                    && r.uid == uid
                    && (r.pending || self.restarting.contains(&r.component))
            })
            .map(|(i, _)| i)
            .collect();
        let seq = &mut self.bind_seq;
        for &ri in &waiting {
            let r = &mut self.records[ri];
            self.restarting.retain(|k| k != &r.component);
            Self::real_start(seq, r, pid, thread.clone(), now, out);
        }
        for &ri in waiting.iter().rev() {
            if !self.records[ri].needed() {
                let r = self.records.remove(ri);
                self.bring_down(r, out);
            }
        }
    }

    /// The hosting pid of the service `token`, if it runs (publishService and unbindFinished come from it).
    pub fn host_of(&self, token: &K::Token) -> Option<i32> {
        self.records
            .iter()
            .find(|r| &r.token == token)
            .and_then(|r| r.app.as_ref().map(|(pid, _)| *pid))
    }

    /// publishServiceLocked: the binding of `key` takes the binder; each of its connections gets connected(name, service, false). False when no such record or binding.
    pub fn publish(
        &mut self,
        token: &K::Token,
        key: &IntentKey,
        binder: Option<K::Binder>,
        out: &mut Vec<Call<K>>,
    ) -> bool {
        let Some(r) = self.records.iter_mut().find(|r| &r.token == token) else {
            return false;
        };
        let Some(b) = r.bindings.iter_mut().find(|b| &b.key == key) else {
            return false;
        };
        if b.received {
            return true;
        }
        b.binder = binder;
        b.requested = true;
        b.received = true;
        for c in &b.conns {
            out.push(Call::Connected {
                conn: c.conn.clone(),
                component: r.component.clone(),
                binder: b.binder.clone(),
                dead: false,
            });
        }
        true
    }

    /// unbindFinishedLocked: onUnbind returned true. Clients that bound again meanwhile get a rebind now, else the next client does (doRebind).
    pub fn unbind_finished(&mut self, token: &K::Token, key: &IntentKey, out: &mut Vec<Call<K>>) {
        let seq = &mut self.bind_seq;
        let Some(r) = self.records.iter_mut().find(|r| &r.token == token) else {
            return;
        };
        let Some(bi) = r.bindings.iter().position(|b| &b.key == key) else {
            return;
        };
        if r.bindings[bi].conns.is_empty() {
            r.bindings[bi].do_rebind = true;
        } else {
            Self::request(seq, r, bi, true, out);
        }
    }

    /// unbindServiceLocked: every connection of `conn` goes (removeConnectionLocked). False when it had none.
    pub fn unbind(&mut self, conn: &K::Conn, out: &mut Vec<Call<K>>) -> bool {
        self.remove_connections(|c| &c.conn == conn, out)
    }

    /// stopServiceLocked (Context.stopService): the start is dropped; the service goes down unless something else needs it. False when there is no such record.
    pub fn stop_service(&mut self, component: &(String, String), out: &mut Vec<Call<K>>) -> bool {
        let Some(ri) = self.records.iter().position(|r| &r.component == component) else {
            return false;
        };
        let r = &mut self.records[ri];
        r.start_requested = false;
        r.call_start = false;
        self.bring_down_if_needed(ri, out);
        true
    }

    /// stopServiceTokenLocked: stopSelf(startId) stops only when startId is the most recent start (a negative startId stops unconditionally); the delivered starts up to it are dropped. False when refused or no such record.
    pub fn stop_self(&mut self, token: &K::Token, start_id: i32, out: &mut Vec<Call<K>>) -> bool {
        let Some(ri) = self.records.iter().position(|r| &r.token == token) else {
            return false;
        };
        let r = &mut self.records[ri];
        if start_id >= 0 {
            if let Some(p) = r
                .delivered_starts
                .iter()
                .position(|s| s.id == start_id && !s.task_removed)
            {
                r.delivered_starts.drain(..=p);
            }
            if r.last_start_id != start_id {
                return false;
            }
        }
        r.start_requested = false;
        r.call_start = false;
        self.bring_down_if_needed(ri, out);
        true
    }

    /// serviceDoneExecutingLocked (START): onStartCommand returned `res` for `start_id`. Sticky results drop the delivered start; NOT_STICKY marks it stop-if-killed when it was the last; REDELIVER_INTENT keeps it for redelivery. An unknown result is an error with no state change; a token with no record is a no-op (AOSP warns).
    pub fn done_executing_start(
        &mut self,
        token: &K::Token,
        start_id: i32,
        res: i32,
    ) -> Result<(), BadResult> {
        let done = match res {
            START_STICKY_COMPATIBILITY | START_STICKY => DoneStart::Sticky,
            START_NOT_STICKY => DoneStart::NotSticky,
            START_REDELIVER_INTENT => DoneStart::Redeliver,
            START_TASK_REMOVED_COMPLETE => DoneStart::TaskRemoved,
            _ => return Err(BadResult(res)),
        };
        let Some(ri) = self.records.iter().position(|r| &r.token == token) else {
            return Ok(());
        };
        let r = &mut self.records[ri];
        match done {
            DoneStart::Sticky => {
                r.take_delivered(start_id, false);
                r.stop_if_killed = false;
            }
            DoneStart::NotSticky => {
                r.take_delivered(start_id, false);
                if r.last_start_id == start_id {
                    r.stop_if_killed = true;
                }
            }
            DoneStart::Redeliver => {
                if let Some(si) = r
                    .delivered_starts
                    .iter_mut()
                    .find(|s| s.id == start_id && !s.task_removed)
                {
                    si.delivery_count = 0;
                    si.done_executing_count += 1;
                    r.stop_if_killed = true;
                }
            }
            DoneStart::TaskRemoved => r.take_delivered(start_id, true),
        }
        r.call_start = res != START_STICKY_COMPATIBILITY;
        Ok(())
    }

    /// removeConnectionLocked for each matching connection: a binding left without clients is unbound (scheduleUnbindService when it had bound); a service whose last BIND_AUTO_CREATE connection went is brought down (bringDownServiceIfNeededLocked).
    fn remove_connections(
        &mut self,
        matches: impl Fn(&Connection<K>) -> bool,
        out: &mut Vec<Call<K>>,
    ) -> bool {
        let mut found = false;
        let mut down = Vec::new();
        for (ri, r) in self.records.iter_mut().enumerate() {
            let mut auto_removed = false;
            for b in r.bindings.iter_mut() {
                let before = b.conns.len();
                auto_removed |= b
                    .conns
                    .iter()
                    .any(|c| matches(c) && c.flags & BIND_AUTO_CREATE != 0);
                b.conns.retain(|c| !matches(c));
                if b.conns.len() == before {
                    continue;
                }
                found = true;
                if b.conns.is_empty() && b.has_bound {
                    b.has_bound = false;
                    b.do_rebind = false;
                    if let Some((_, thread)) = &r.app {
                        out.push(Call::Unbind {
                            thread: thread.clone(),
                            token: r.token.clone(),
                            intent: b.intent.clone(),
                        });
                    }
                }
            }
            if auto_removed && !r.needed() {
                down.push(ri);
            }
        }
        for ri in down.into_iter().rev() {
            let r = self.records.remove(ri);
            self.bring_down(r, out);
        }
        found
    }

    /// bringDownServiceIfNeededLocked: a record that nothing needs goes down, unless it waits for its process (AS :6290: the attach decides).
    fn bring_down_if_needed(&mut self, ri: usize, out: &mut Vec<Call<K>>) {
        if self.records[ri].needed() || self.records[ri].pending {
            return;
        }
        let r = self.records.remove(ri);
        self.bring_down(r, out);
    }

    /// bringDownServiceLocked: the remaining connections hear the service is gone (connected(name, null, true)), bound bindings are unbound, a running service is stopped (scheduleStopService). A restart pending for it is cancelled (unscheduleServiceRestartLocked, force).
    fn bring_down(&mut self, mut r: Record<K>, out: &mut Vec<Call<K>>) {
        self.restarting.retain(|k| k != &r.component);
        for b in &r.bindings {
            for c in &b.conns {
                out.push(Call::Connected {
                    conn: c.conn.clone(),
                    component: r.component.clone(),
                    binder: None,
                    dead: true,
                });
            }
        }
        if let Some((_, thread)) = r.app.take() {
            for b in r.bindings.iter_mut().filter(|b| b.has_bound) {
                b.has_bound = false;
                b.requested = false;
                out.push(Call::Unbind {
                    thread: thread.clone(),
                    token: r.token.clone(),
                    intent: b.intent.clone(),
                });
            }
            out.push(Call::Stop {
                thread,
                token: r.token.clone(),
            });
        }
    }

    /// nextRestartTime of the record `key`, while it is restarting.
    fn restart_at(&self, key: &(String, String)) -> Option<i64> {
        self.records
            .iter()
            .find(|r| &r.component == key)
            .map(|r| r.next_restart_time)
    }

    /// scheduleServiceRestartLocked (allowCancel): the killed record `ri` gets its restart. False when nothing needs one (the caller brings it down).
    /// Delivered starts go back to pending, except a start past its delivery or done-executing budget, which is cancelled. The delay doubles for a kill inside the reset window of the last start; a restart is spaced SERVICE_MIN_RESTART_TIME_BETWEEN after the ones of services that die together.
    fn schedule_restart(&mut self, ri: usize, now: i64) -> bool {
        let r = &mut self.records[ri];
        let mut min_duration = SERVICE_RESTART_DURATION;
        let mut reset_time = SERVICE_RESET_RUN_DURATION;
        let mut canceled = false;
        for si in std::mem::take(&mut r.delivered_starts).into_iter().rev() {
            if si.intent.is_none() {
                continue;
            }
            if si.delivery_count < MAX_DELIVERY_COUNT
                && si.done_executing_count < MAX_DONE_EXECUTING_COUNT
            {
                let dur = (now - si.delivered_at) * 2;
                min_duration = min_duration.max(dur);
                reset_time = reset_time.max(dur);
                r.pending_starts.insert(0, si);
            } else {
                canceled = true;
            }
        }
        if r.can_stop_if_killed(canceled) && !r.has_auto_create() {
            return false;
        }
        r.restart_delay = if r.restart_delay != 0 && now <= r.restart_time + reset_time {
            (r.restart_delay * SERVICE_RESTART_DURATION_FACTOR).max(min_duration)
        } else {
            min_duration
        };
        let key = r.component.clone();
        let mut next = now + r.restart_delay;
        if self.restarting.is_empty() {
            next = next.max(now + EXTRA_RESTART_DELAY);
        } else {
            let between = EXTRA_RESTART_DELAY + SERVICE_MIN_RESTART_TIME_BETWEEN;
            loop {
                let mut repeat = false;
                for k in self.restarting.iter().rev() {
                    let Some(other) = self.restart_at(k) else {
                        continue;
                    };
                    if (other - between..other + between).contains(&next) {
                        next = other + between;
                        repeat = true;
                        break;
                    }
                    if next >= other + between {
                        break;
                    }
                }
                if !repeat {
                    break;
                }
            }
        }
        self.restarting.retain(|k| k != &key);
        let pos = self
            .restarting
            .iter()
            .position(|k| self.restart_at(k).is_some_and(|t| t > next))
            .unwrap_or(self.restarting.len());
        self.restarting.insert(pos, key);
        let r = &mut self.records[ri];
        r.restart_delay = next - now;
        r.next_restart_time = next;
        true
    }

    /// What the services hold of process `pid` (named `process`, uid `uid`), for uid_state::Holdings: a started service, a BIND_AUTO_CREATE binding. A service waiting for that
    /// process (pending) holds it too, so the process is not ended while its start is on its way.
    pub fn held_by(&self, pid: i32, process: &str, uid: u32) -> Held {
        let mut held = Held::default();
        for r in &self.records {
            let hosted = r.app.as_ref().is_some_and(|(p, _)| *p == pid);
            let waiting = r.pending && r.process == process && r.uid == uid;
            if hosted || waiting {
                held.started |= r.start_requested;
                held.bound |= r.has_auto_create();
            }
        }
        held
    }

    /// The components of `uid` that are started (startRequested): stopInBackgroundLocked's candidates (AS :1685).
    pub fn started_of_uid(&self, uid: u32) -> Vec<(String, String)> {
        self.records
            .iter()
            .filter(|r| r.uid == uid && r.start_requested)
            .map(|r| r.component.clone())
            .collect()
    }

    /// The process name and uid a record runs in (retrieveServiceLocked's processName), while the record exists.
    pub fn process_of(&self, component: &(String, String)) -> Option<(String, u32)> {
        self.records
            .iter()
            .find(|r| &r.component == component)
            .map(|r| (r.process.clone(), r.uid))
    }

    /// startRequested of the record of `component` (false when there is none).
    pub fn start_requested(&self, component: &(String, String)) -> bool {
        self.records
            .iter()
            .any(|r| &r.component == component && r.start_requested)
    }

    /// The component of the record `token` names (findServiceLocked checks the ComponentName against the token).
    pub fn component_of(&self, token: &K::Token) -> Option<(String, String)> {
        self.records
            .iter()
            .find(|r| &r.token == token)
            .map(|r| r.component.clone())
    }

    /// killServicesLocked + the client half of cleanUpApplicationRecordLocked for a dead or killed process: clients hear dead, and each hosted record is restarted (`allow_restart`) or brought down. Returns the restarts scheduled, in record order.
    pub fn process_died(
        &mut self,
        pid: i32,
        allow_restart: bool,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) -> Vec<Restart> {
        self.remove_connections(|c| c.client_pid == pid, out);
        let mut restarts = Vec::new();
        let mut i = 0;
        while i < self.records.len() {
            if !self.records[i].app.as_ref().is_some_and(|(p, _)| *p == pid) {
                i += 1;
                continue;
            }
            let r = &mut self.records[i];
            r.app = None;
            for b in r.bindings.iter_mut() {
                for c in &b.conns {
                    out.push(Call::Connected {
                        conn: c.conn.clone(),
                        component: r.component.clone(),
                        binder: None,
                        dead: true,
                    });
                }
                b.binder = None;
                b.requested = false;
                b.received = false;
                b.has_bound = false;
            }
            if allow_restart && self.records[i].needed() && self.schedule_restart(i, now) {
                let r = &self.records[i];
                restarts.push(Restart {
                    component: r.component.clone(),
                    at: r.next_restart_time,
                });
                i += 1;
            } else {
                let r = self.records.remove(i);
                self.restarting.retain(|k| k != &r.component);
            }
        }
        restarts
    }

    /// performServiceRestartLocked: the restart timer of `component` fired. It leaves the restarting list and comes up (bring_up): Host::NeedsProcess when its process must start, the attach then creates it. Nothing when no restart is pending (a start or bind cancelled it).
    pub fn restart(
        &mut self,
        component: &(String, String),
        host: Option<(i32, K::Thread)>,
        now: i64,
        out: &mut Vec<Call<K>>,
    ) -> Host {
        let before = self.restarting.len();
        self.restarting.retain(|k| k != component);
        if self.restarting.len() == before {
            return Host::Ready;
        }
        match self.records.iter().position(|r| &r.component == component) {
            Some(ri) => self.bring_up(ri, host, now, out),
            None => Host::Ready,
        }
    }

    /// Records of `uid` waiting in `process` for a start that failed: brought down (bringUpServiceLocked's failure path).
    pub fn start_failed(&mut self, process: &str, uid: u32, out: &mut Vec<Call<K>>) {
        let (failed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.records)
            .into_iter()
            .partition(|r| r.pending && r.process == process && r.uid == uid);
        self.records = kept;
        for r in failed {
            self.bring_down(r, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain values for the handles: tokens and connections are numbers, binders and threads strings.
    struct T;
    impl Kinds for T {
        type Token = u32;
        type Conn = u32;
        type Binder = &'static str;
        type Thread = &'static str;
        type Intent = &'static str;
        type Info = ();
    }

    fn key(action: &str) -> IntentKey {
        IntentKey {
            action: Some(action.into()),
            ..Default::default()
        }
    }

    fn component(name: &str) -> (String, String) {
        ("p".into(), format!("p.{name}"))
    }

    /// A bind of service `name` (uid 10_001, process "p") by a client of the same uid.
    fn req(name: &str, conn: u32, client: i32, action: &'static str, flags: i64) -> Bind<T> {
        req_from(name, conn, client, action, flags, 10_001)
    }

    fn req_from(
        name: &str,
        conn: u32,
        client: i32,
        action: &'static str,
        flags: i64,
        caller_uid: u32,
    ) -> Bind<T> {
        Bind {
            component: component(name),
            uid: 10_001,
            process: "p".into(),
            info: (),
            intent: action,
            key: key(action),
            conn,
            client_pid: client,
            caller_uid,
            flags,
        }
    }

    /// A start of service `name` by a client of the same uid.
    fn start_req(name: &str, action: &'static str) -> Start<T> {
        Start {
            component: component(name),
            uid: 10_001,
            process: "p".into(),
            info: (),
            intent: action,
            caller_uid: 10_001,
        }
    }

    /// A compact trace of the calls, to compare against the AOSP sequence. Args: start_id/flags/intent (null for the sticky restart's null Intent).
    fn trace(out: &[Call<T>]) -> Vec<String> {
        out.iter()
            .map(|c| match c {
                Call::Create { .. } => "create".to_owned(),
                Call::Bind { intent, rebind, .. } => format!("bind {intent} rebind={rebind}"),
                Call::Unbind { intent, .. } => format!("unbind {intent}"),
                Call::Stop { .. } => "stop".to_owned(),
                Call::Args { args, .. } => format!(
                    "args {}",
                    args.iter()
                        .map(|a| format!(
                            "{}/{}/{}",
                            a.start_id,
                            a.flags,
                            a.intent.unwrap_or("null")
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                Call::Connected {
                    conn, binder, dead, ..
                } => format!("connected {conn} {binder:?} dead={dead}"),
            })
            .collect()
    }

    /// Kills the process (pid 60) hosting "S" at `died`; returns the restart time.
    fn kill(s: &mut Services<T>, died: i64) -> i64 {
        let mut out = Vec::new();
        let restarts = s.process_died(60, true, died, &mut out);
        assert_eq!(restarts.len(), 1);
        restarts[0].at
    }

    /// The restart timer of "S" fires: it comes up in the attached process (pid 60) and rebinds its binding "a".
    fn revive(s: &mut Services<T>, at: i64) {
        let mut out = Vec::new();
        assert_eq!(
            s.restart(&component("S"), Some((60, "t")), at, &mut out),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
    }

    /// The bindService -> onServiceConnected -> unbindService -> onUnbind -> onDestroy sequence of one client (ActiveServices bindServiceLocked, publishServiceLocked, removeConnectionLocked).
    #[test]
    fn bind_publish_unbind() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        let host = s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        assert_eq!(host, Host::Ready);
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
        out.clear();
        assert!(s.publish(&9, &key("a"), Some("svc"), &mut out));
        assert_eq!(trace(&out), ["connected 1 Some(\"svc\") dead=false"]);
        out.clear();
        assert!(s.unbind(&1, &mut out));
        assert_eq!(trace(&out), ["unbind a", "stop"]);
        out.clear();
        assert!(!s.unbind(&1, &mut out));
        assert!(out.is_empty());
    }

    /// A second client of a published binding is answered at once, without a second onBind (bindServiceLocked `b.intent.received`); the service stays until the last auto-create client goes.
    #[test]
    fn second_client_gets_published_binder() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        s.bind(
            req("S", 2, 51, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            0,
            &mut out,
        );
        assert_eq!(trace(&out), ["connected 2 Some(\"svc\") dead=false"]);
        out.clear();
        s.unbind(&1, &mut out);
        assert!(out.is_empty());
        s.unbind(&2, &mut out);
        assert_eq!(trace(&out), ["unbind a", "stop"]);
    }

    /// No attached process: the bind asks for one and the start waits for attach (mPendingServices).
    #[test]
    fn cold_start_waits_for_attach() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.bind(
                req("S", 1, 50, "a", BIND_AUTO_CREATE),
                None,
                || 9,
                0,
                &mut out
            ),
            Host::NeedsProcess
        );
        assert!(out.is_empty());
        s.attach(60, "p", 10_001, &"t", 0, &mut out);
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
    }

    /// Without BIND_AUTO_CREATE nothing starts: the binding waits for a service brought up by another bind.
    #[test]
    fn no_auto_create_waits() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.bind(req("S", 1, 50, "a", 0), None, || 9, 0, &mut out),
            Host::Ready
        );
        assert!(out.is_empty());
        s.bind(
            req("S", 2, 51, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            0,
            &mut out,
        );
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
        out.clear();
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        assert_eq!(
            trace(&out),
            [
                "connected 1 Some(\"svc\") dead=false",
                "connected 2 Some(\"svc\") dead=false"
            ]
        );
        out.clear();
        // the last auto-create client goes: the other hears the service died (bringDownServiceLocked)
        s.unbind(&2, &mut out);
        assert_eq!(
            trace(&out),
            ["connected 1 None dead=true", "unbind a", "stop"]
        );
    }

    /// onUnbind returned true (unbindFinished): the next client gets onRebind instead of onBind.
    #[test]
    fn rebind_after_unbind_finished() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(req("S", 1, 50, "a", 0), None, || 9, 0, &mut out);
        s.bind(
            req("S", 2, 51, "b", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        s.unbind(&1, &mut out);
        out.clear();
        s.unbind_finished(&9, &key("a"), &mut out);
        assert!(out.is_empty());
        s.bind(req("S", 3, 52, "a", 0), None, || 7, 0, &mut out);
        assert_eq!(
            trace(&out),
            ["connected 3 Some(\"svc\") dead=false", "bind a rebind=true"]
        );
    }

    /// The hosting process dies: clients hear dead; the needed record is restarted (1 s after the death, ATL schedules it); a dying client is unbound and its record goes.
    #[test]
    fn process_death() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        let restarts = s.process_died(60, true, 0, &mut out);
        assert_eq!(
            restarts,
            [Restart {
                component: component("S"),
                at: 1_000
            }]
        );
        assert_eq!(trace(&out), ["connected 1 None dead=true"]);
        out.clear();
        // the client dies: the restarting record goes (no client left), nothing to send (no process)
        assert!(s.process_died(50, true, 0, &mut out).is_empty());
        assert!(out.is_empty());
        assert_eq!(s.host_of(&9), None);
    }

    /// allow_restart false (killServicesLocked !allowRestart): the hosted service is brought down, its clients hear dead.
    #[test]
    fn process_death_without_restart_brings_down() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        assert!(s.process_died(60, false, 0, &mut out).is_empty());
        assert_eq!(trace(&out), ["connected 1 None dead=true"]);
        assert_eq!(s.host_of(&9), None);
    }

    /// A started STICKY service (onStartCommand returned START_STICKY) dies with its process. A crash restarts it at SERVICE_RESTART_DURATION (1 s); a force-stop
    /// (allow_restart false, forceStopPackageInternalLocked -> killServicesLocked !allowRestart, ActiveServices :7343-7345) brings it down and nothing restarts it.
    #[test]
    fn force_stop_does_not_restart_a_sticky_start() {
        for allow_restart in [true, false] {
            let mut s = Services::<T>::default();
            let mut out = Vec::new();
            assert_eq!(
                s.start(start_req("S", "a"), None, || 9, 0, &mut out),
                Host::NeedsProcess
            );
            s.attach(60, "p", 10_001, &"t", 200, &mut out);
            assert_eq!(s.done_executing_start(&9, 1, START_STICKY), Ok(()));
            out.clear();
            let restarts = s.process_died(60, allow_restart, 5_000, &mut out);
            if allow_restart {
                assert_eq!(
                    restarts,
                    [Restart {
                        component: component("S"),
                        at: 6_000
                    }]
                );
                assert!(s.process_of(&component("S")).is_some());
            } else {
                assert!(restarts.is_empty());
                assert_eq!(s.process_of(&component("S")), None);
            }
        }
    }

    /// processStartTimedOutLocked (ActiveServices :6969, failed to attach, also a process that died before attach): the services waiting in that process
    /// (uid + process name) come down; a service waiting in another process stays.
    #[test]
    fn start_failed_brings_down_only_the_waiting_records_of_its_process() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.start(start_req("S", "a"), None, || 9, 0, &mut out),
            Host::NeedsProcess
        );
        let elsewhere = Start {
            process: "q".into(),
            ..start_req("T", "b")
        };
        assert_eq!(
            s.start(elsewhere, None, || 10, 0, &mut out),
            Host::NeedsProcess
        );
        s.start_failed("p", 10_001, &mut out);
        assert_eq!(s.process_of(&component("S")), None);
        assert_eq!(s.process_of(&component("T")), Some(("q".into(), 10_001)));
    }

    /// scheduleServiceRestartLocked backoff: a kill 1 s after each restart multiplies the delay by SERVICE_RESTART_DURATION_FACTOR; a run past SERVICE_RESET_RUN_DURATION resets it to SERVICE_RESTART_DURATION.
    #[test]
    fn restart_delays_follow_aosp_backoff() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        assert_eq!(kill(&mut s, 10_000), 11_000);
        revive(&mut s, 11_000);
        assert_eq!(kill(&mut s, 12_000), 16_000);
        revive(&mut s, 16_000);
        assert_eq!(kill(&mut s, 17_000), 33_000);
        revive(&mut s, 33_000);
        // 61 s after the last start (33 000 + 60 000 < 94 000): back to the first delay
        assert_eq!(kill(&mut s, 94_000), 95_000);
    }

    /// Two services of one process die together: the second restart waits SERVICE_MIN_RESTART_TIME_BETWEEN after the first.
    #[test]
    fn two_services_restart_ten_seconds_apart() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("A", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.bind(
            req("B", 2, 50, "b", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            0,
            &mut out,
        );
        out.clear();
        let restarts = s.process_died(60, true, 20_000, &mut out);
        assert_eq!(
            restarts.iter().map(|r| r.at).collect::<Vec<_>>(),
            [21_000, 31_000]
        );
    }

    /// onStartCommand START_STICKY: the delivered start is done and dropped; the killed service is restarted.
    #[test]
    fn sticky_result_drops_start_and_restarts() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.start(start_req("S", "a"), Some((60, "t")), || 9, 1_000, &mut out),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "args 1/0/a"]);
        assert_eq!(s.done_executing_start(&9, 1, START_STICKY), Ok(()));
        out.clear();
        let restarts = s.process_died(60, true, 5_000, &mut out);
        assert_eq!(
            restarts,
            [Restart {
                component: component("S"),
                at: 6_000
            }]
        );
    }

    /// onStartCommand START_NOT_STICKY for the last start: stop-if-killed, so the killed service is not restarted.
    #[test]
    fn not_sticky_last_result_is_not_restarted() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 1_000, &mut out);
        assert_eq!(s.done_executing_start(&9, 1, START_NOT_STICKY), Ok(()));
        assert!(s.process_died(60, true, 5_000, &mut out).is_empty());
        assert_eq!(s.host_of(&9), None);
    }

    /// onStartCommand START_REDELIVER_INTENT: the start stays pending for redelivery, delivered again with START_FLAG_REDELIVERY; its restart waits twice the time since its delivery (1 000 ms sent, 4 000 ms before the kill).
    #[test]
    fn redeliver_result_keeps_start_with_redelivery_flag() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 1_000, &mut out);
        assert_eq!(
            s.done_executing_start(&9, 1, START_REDELIVER_INTENT),
            Ok(())
        );
        let restarts = s.process_died(60, true, 5_000, &mut out);
        assert_eq!(
            restarts,
            [Restart {
                component: component("S"),
                at: 13_000
            }]
        );
        out.clear();
        assert_eq!(
            s.restart(&component("S"), Some((61, "t")), 13_000, &mut out),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "args 1/1/a"]);
    }

    /// A start delivered a second time (its first delivery never answered) carries START_FLAG_RETRY.
    #[test]
    fn second_delivery_sets_retry_flag() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        assert_eq!(trace(&out), ["create", "args 1/0/a"]);
        out.clear();
        let restarts = s.process_died(60, true, 500, &mut out);
        assert_eq!(restarts[0].at, 1_500);
        assert_eq!(
            s.restart(&component("S"), Some((61, "t")), 1_500, &mut out),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "args 1/2/a"]);
    }

    /// onStartCommand START_STICKY, then the kill: the restart delivers onStartCommand(null) with a new start id.
    #[test]
    fn sticky_restart_delivers_null_intent() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 1_000, &mut out);
        assert_eq!(s.done_executing_start(&9, 1, START_STICKY), Ok(()));
        assert_eq!(s.process_died(60, true, 5_000, &mut out)[0].at, 6_000);
        out.clear();
        assert_eq!(
            s.restart(&component("S"), Some((61, "t")), 6_000, &mut out),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "args 2/0/null"]);
    }

    /// An unknown result is an error and changes nothing; a token with no record is a no-op.
    #[test]
    fn done_rejects_unknown_result() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        assert_eq!(s.done_executing_start(&9, 1, 7), Err(BadResult(7)));
        assert_eq!(s.records[0].delivered_starts.len(), 1);
        assert!(!s.records[0].stop_if_killed);
        assert_eq!(s.done_executing_start(&99, 1, START_STICKY), Ok(()));
    }

    /// TASK_REMOVED_COMPLETE drops only the start delivered with its task removed (its producer, onTaskRemoved, is not wired yet).
    #[test]
    fn done_task_removed_drops_only_task_removed_start() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        s.records[0]
            .delivered_starts
            .push(StartItem::new(1, Some("a"), true));
        assert_eq!(
            s.done_executing_start(&9, 1, START_TASK_REMOVED_COMPLETE),
            Ok(())
        );
        assert_eq!(s.records[0].delivered_starts.len(), 1);
        assert!(!s.records[0].delivered_starts[0].task_removed);
    }

    /// stopSelf(startId) with a stale id is refused (the service keeps running); the current id stops it.
    #[test]
    fn stop_self_refuses_stale_start_id() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        assert_eq!(trace(&out), ["create", "args 1/0/a"]);
        out.clear();
        s.start(start_req("S", "a"), Some((60, "t")), || 8, 0, &mut out);
        assert_eq!(trace(&out), ["args 2/0/a"]);
        out.clear();
        assert!(!s.stop_self(&9, 1, &mut out));
        assert!(out.is_empty());
        assert!(s.stop_self(&9, 2, &mut out));
        assert_eq!(trace(&out), ["stop"]);
    }

    /// A start with no process waits for attach; the attach creates the service and delivers the start.
    #[test]
    fn start_waits_for_attach() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.start(start_req("S", "a"), None, || 9, 0, &mut out),
            Host::NeedsProcess
        );
        assert!(out.is_empty());
        s.attach(60, "p", 10_001, &"t", 200, &mut out);
        assert_eq!(trace(&out), ["create", "args 1/0/a"]);
    }

    /// stopService drops the start only: a bound service stays until its last BIND_AUTO_CREATE client unbinds.
    #[test]
    fn stop_service_keeps_a_bound_service() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.start(start_req("S", "a"), Some((60, "t")), || 8, 0, &mut out);
        out.clear();
        assert!(s.stop_service(&component("S"), &mut out));
        assert!(out.is_empty());
        assert!(s.unbind(&1, &mut out));
        assert_eq!(trace(&out), ["unbind a", "stop"]);
    }

    /// A bind from another uid while the restart waits: the restart is cancelled, the service comes up now, and the backoff restarts from the first delay.
    #[test]
    fn bind_during_restart_cancels_it() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        assert_eq!(s.process_died(60, true, 10_000, &mut out).len(), 1);
        out.clear();
        assert_eq!(
            s.bind(
                req_from("S", 2, 51, "a", BIND_AUTO_CREATE, 10_002),
                Some((61, "t")),
                || 8,
                10_500,
                &mut out
            ),
            Host::Ready
        );
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
        out.clear();
        assert_eq!(
            s.restart(&component("S"), Some((61, "t")), 11_000, &mut out),
            Host::Ready
        );
        assert!(out.is_empty());
        // the other uid reset the backoff: a kill now restarts after the first delay, not 4 000 ms
        assert_eq!(s.process_died(61, true, 20_000, &mut out)[0].at, 21_000);
    }

    /// Another process of the same process name and uid attaches while a restart waits: the waiting service starts there, and its own restart timer does nothing.
    #[test]
    fn attach_kicks_a_pending_restart() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req("S", 1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            0,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        assert_eq!(s.process_died(60, true, 10_000, &mut out).len(), 1);
        out.clear();
        s.attach(61, "p", 10_001, &"t", 10_200, &mut out);
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
        out.clear();
        assert_eq!(
            s.restart(&component("S"), Some((61, "t")), 11_000, &mut out),
            Host::Ready
        );
        assert!(out.is_empty());
    }

    /// stopService while the process starts: the record waits (bringDownServiceIfNeededLocked :6290 returns for a pending record), and the attach creates it, delivers its
    /// pending start, then brings it down because nothing needs it (attachApplicationLocked :6914-6925): create, the start, stop.
    #[test]
    fn stop_before_attach_runs_pending_start_then_stops() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.start(start_req("S", "a"), None, || 9, 0, &mut out),
            Host::NeedsProcess
        );
        assert!(s.stop_service(&component("S"), &mut out));
        assert!(out.is_empty());
        s.attach(60, "p", 10_001, &"t", 200, &mut out);
        assert_eq!(trace(&out), ["create", "args 1/0/a", "stop"]);
        assert_eq!(s.host_of(&9), None);
    }

    /// held_by: a started service holds the pid it runs in; a pending bind holds the process it waits for, and nothing else is held by another pid.
    #[test]
    fn held_by_reports_started_and_bound_per_process() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        assert_eq!(
            s.held_by(60, "p", 10_001),
            Held {
                started: true,
                bound: false
            }
        );
        assert_eq!(s.held_by(61, "p", 10_001), Held::default());
        s.bind(
            req("B", 1, 50, "b", BIND_AUTO_CREATE),
            None,
            || 8,
            0,
            &mut out,
        );
        assert_eq!(
            s.held_by(61, "p", 10_001),
            Held {
                started: false,
                bound: true
            }
        );
    }

    /// stopInBackgroundLocked's candidates: the started components of the uid (a bound-only one is not started).
    #[test]
    fn started_of_uid_lists_started_components() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.start(start_req("S", "a"), Some((60, "t")), || 9, 0, &mut out);
        s.bind(
            req("B", 1, 50, "b", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            0,
            &mut out,
        );
        assert_eq!(s.started_of_uid(10_001), vec![component("S")]);
        assert!(s.started_of_uid(10_002).is_empty());
    }
}
