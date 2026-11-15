// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActiveServices.java
//! The bound-service half of ActiveServices as plain state: bindServiceLocked, requestServiceBindingLocked, realStartServiceLocked, publishServiceLocked, removeConnectionLocked,
//! bringDownServiceIfNeededLocked / bringDownServiceLocked, unbindFinishedLocked, killServicesLocked. No binder here: each step returns the [`Call`]s the activity service sends after releasing its
//! lock (all are oneway: IApplicationThread is a oneway interface, IServiceConnection.connected oneway), in order.
//! Records are keyed by component (ActiveServices' ServiceMap by ComponentName, one user); bindings by Intent.FilterComparison ([`IntentKey`]).
//! Not modelled: startService / stopService (a service is never "started", so it lives exactly as long as its BIND_AUTO_CREATE connections), instanceName / isolated / sdk-sandbox services,
//! per-client AppBindRecord (a binding's clients are its connections), service restart after its process dies (killServicesLocked's scheduleServiceRestartLocked: the record waits for the next bind),
//! ANR timeouts on scheduled calls (serviceDoneExecuting is accepted, not waited for), oom-adj and process-state accounting.

use crate::broadcast::IntentKey;

/// Context.BIND_AUTO_CREATE.
pub const BIND_AUTO_CREATE: i64 = 0x0001;

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
    /// the bound Intent as sent to the app
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
    /// IServiceConnection.connected(name, service, dead)
    Connected {
        conn: K::Conn,
        component: (String, String),
        binder: Option<K::Binder>,
        dead: bool,
    },
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
}

impl<K: Kinds> Record<K> {
    /// ServiceRecord.hasAutoCreateConnections: what keeps a never-started service alive (isServiceNeededLocked).
    fn needed(&self) -> bool {
        self.bindings
            .iter()
            .flat_map(|b| &b.conns)
            .any(|c| c.flags & BIND_AUTO_CREATE != 0)
    }
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
    pub flags: i64,
}

/// The bound services (ActiveServices' ServiceMap + mServiceConnections).
pub struct Services<K: Kinds> {
    records: Vec<Record<K>>,
    /// mBindServiceSeqCounter
    bind_seq: i64,
}

impl<K: Kinds> Default for Services<K> {
    fn default() -> Self {
        Services {
            records: Vec::new(),
            bind_seq: 0,
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

    /// realStartServiceLocked: scheduleCreateService, then every binding (requestServiceBindingsLocked).
    fn real_start(
        seq: &mut i64,
        r: &mut Record<K>,
        pid: i32,
        thread: K::Thread,
        out: &mut Vec<Call<K>>,
    ) {
        out.push(Call::Create {
            thread: thread.clone(),
            token: r.token.clone(),
            info: r.info.clone(),
        });
        r.app = Some((pid, thread));
        for bi in 0..r.bindings.len() {
            Self::request(seq, r, bi, false, out);
        }
    }

    /// bindServiceLocked after retrieveServiceLocked: the connection joins the record's binding for its Intent; BIND_AUTO_CREATE brings the service up (`host`: its attached process, if any).
    /// Then an already published binding answers at once (connected), else an unrequested one is requested. Returns what the caller must still do for the process.
    pub fn bind(
        &mut self,
        req: Bind<K>,
        host: Option<(i32, K::Thread)>,
        new_token: impl FnOnce() -> K::Token,
        out: &mut Vec<Call<K>>,
    ) -> Host {
        let ri = match self
            .records
            .iter()
            .position(|r| r.component == req.component)
        {
            Some(i) => i,
            None => {
                self.records.push(Record {
                    token: new_token(),
                    component: req.component.clone(),
                    uid: req.uid,
                    process: req.process.clone(),
                    info: req.info,
                    app: None,
                    bindings: Vec::new(),
                });
                self.records.len() - 1
            }
        };
        let seq = &mut self.bind_seq;
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
        let mut host_needed = Host::Ready;
        if req.flags & BIND_AUTO_CREATE != 0 && r.app.is_none() {
            match host {
                Some((pid, thread)) => Self::real_start(seq, r, pid, thread, out),
                None => host_needed = Host::NeedsProcess,
            }
        }
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

    /// attachApplicationLocked's mPendingServices pass: services of `process` (uid) waiting for it, still needed, start in the new process.
    pub fn attach(
        &mut self,
        pid: i32,
        process: &str,
        uid: u32,
        thread: &K::Thread,
        out: &mut Vec<Call<K>>,
    ) {
        let seq = &mut self.bind_seq;
        for r in self.records.iter_mut() {
            if r.app.is_none() && r.process == process && r.uid == uid && r.needed() {
                Self::real_start(seq, r, pid, thread.clone(), out);
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

    /// removeConnectionLocked for each matching connection: a binding left without clients is unbound (scheduleUnbindService when it had bound); a service whose last BIND_AUTO_CREATE connection
    /// went is brought down (bringDownServiceIfNeededLocked).
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
            Self::bring_down(r, out);
        }
        found
    }

    /// bringDownServiceLocked: the remaining connections hear the service is gone (connected(name, null, true)), bound bindings are unbound, then scheduleStopService.
    fn bring_down(mut r: Record<K>, out: &mut Vec<Call<K>>) {
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

    /// A process died: its client connections are removed as unbinds (cleanUpApplicationRecordLocked); the services it hosted lose their process (killServicesLocked): every connection hears
    /// connected(name, null, true), and the record forgets its bindings' state. A record still needed stays and waits for the next bind (no restart); the others go. Returns how many stayed.
    pub fn process_died(&mut self, pid: i32, out: &mut Vec<Call<K>>) -> usize {
        self.remove_connections(|c| c.client_pid == pid, out);
        let mut waiting = 0;
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
            if r.needed() {
                waiting += 1;
                i += 1;
            } else {
                self.records.remove(i);
            }
        }
        waiting
    }

    /// Records of `uid` hosted in `process` that are needed but not running: what a failed process start leaves waiting (bringUpServiceLocked's failure path brings them down).
    pub fn start_failed(&mut self, process: &str, uid: u32, out: &mut Vec<Call<K>>) {
        let (failed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.records)
            .into_iter()
            .partition(|r| r.app.is_none() && r.process == process && r.uid == uid);
        self.records = kept;
        for r in failed {
            Self::bring_down(r, out);
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

    fn req(conn: u32, client: i32, action: &'static str, flags: i64) -> Bind<T> {
        Bind {
            component: ("p".into(), "p.S".into()),
            uid: 10_001,
            process: "p".into(),
            info: (),
            intent: action,
            key: key(action),
            conn,
            client_pid: client,
            flags,
        }
    }

    /// A compact trace of the calls, to compare against the AOSP sequence.
    fn trace(out: &[Call<T>]) -> Vec<String> {
        out.iter()
            .map(|c| match c {
                Call::Create { .. } => "create".to_owned(),
                Call::Bind { intent, rebind, .. } => format!("bind {intent} rebind={rebind}"),
                Call::Unbind { intent, .. } => format!("unbind {intent}"),
                Call::Stop { .. } => "stop".to_owned(),
                Call::Connected {
                    conn, binder, dead, ..
                } => format!("connected {conn} {binder:?} dead={dead}"),
            })
            .collect()
    }

    /// The bindService -> onServiceConnected -> unbindService -> onUnbind -> onDestroy sequence of one client (ActiveServices bindServiceLocked, publishServiceLocked, removeConnectionLocked).
    #[test]
    fn bind_publish_unbind() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        let host = s.bind(
            req(1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
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
            req(1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        s.bind(
            req(2, 51, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
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
            s.bind(req(1, 50, "a", BIND_AUTO_CREATE), None, || 9, &mut out),
            Host::NeedsProcess
        );
        assert!(out.is_empty());
        s.attach(60, "p", 10_001, &"t", &mut out);
        assert_eq!(trace(&out), ["create", "bind a rebind=false"]);
    }

    /// Without BIND_AUTO_CREATE nothing starts: the binding waits for a service brought up by another bind.
    #[test]
    fn no_auto_create_waits() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        assert_eq!(
            s.bind(req(1, 50, "a", 0), None, || 9, &mut out),
            Host::Ready
        );
        assert!(out.is_empty());
        s.bind(
            req(2, 51, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
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
        s.bind(req(1, 50, "a", 0), None, || 9, &mut out);
        s.bind(
            req(2, 51, "b", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 8,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        s.unbind(&1, &mut out);
        out.clear();
        s.unbind_finished(&9, &key("a"), &mut out);
        assert!(out.is_empty());
        s.bind(req(3, 52, "a", 0), None, || 7, &mut out);
        assert_eq!(
            trace(&out),
            ["connected 3 Some(\"svc\") dead=false", "bind a rebind=true"]
        );
    }

    /// The hosting process dies: clients hear dead, a still-needed record waits; a dying client is unbound.
    #[test]
    fn process_death() {
        let mut s = Services::<T>::default();
        let mut out = Vec::new();
        s.bind(
            req(1, 50, "a", BIND_AUTO_CREATE),
            Some((60, "t")),
            || 9,
            &mut out,
        );
        s.publish(&9, &key("a"), Some("svc"), &mut out);
        out.clear();
        assert_eq!(s.process_died(60, &mut out), 1);
        assert_eq!(trace(&out), ["connected 1 None dead=true"]);
        out.clear();
        // the client dies: the waiting record goes, nothing to send (no process)
        assert_eq!(s.process_died(50, &mut out), 0);
        assert!(out.is_empty());
        assert_eq!(s.host_of(&9), None);
    }
}
