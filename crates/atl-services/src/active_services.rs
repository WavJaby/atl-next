// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActiveServices.java
//! The activity service's bound services (a child module of activity.rs, so it shares its state lock): the binder half of bindServiceLocked / retrieveServiceLocked (lookup + checkComponentPermission),
//! publishService, unbindService, unbindFinished, and the sender of the calls bound_services.rs returns. The rules themselves are bound_services.rs.
//! Calls leave on one thread in the order the state lock produced them (broadcast_queue.rs's reason: no call out of the system process with the lock held; in-process connections, the job runner's,
//! run their `connected` there too, so they may call back into the services).
//! Not modelled: implicit service Intents (a component-less Intent answers 0, counted: queryIntentServices is not served), instanceName (ignored, counted), bindings across users.

use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};

use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_aidl::aidl::android::app::IServiceConnection::IServiceConnection;
use atl_parcelables::android::content::pm::ServiceInfo;
use atl_parcelables::android::content::res::CompatibilityInfo;
use atl_parcelables::android::content::{ComponentName, Intent};
use binder::{SpIBinder, Strong};

use super::{AM, Inner, State, intent_key, log};
use crate::bound_services::{self, Bind, Call, Host, Kinds};
use crate::package;
use atl_services_types::permission::{self, ROOT_UID, SYSTEM_UID};

/// ActivityManager.PROCESS_STATE_UNKNOWN: the processState the scheduled calls carry (process states are not tracked).
const PROCESS_STATE_UNKNOWN: i32 = -1;
/// The client pid of a binding the services make for themselves (the job runner): no process record, never dies.
pub(crate) const SYSTEM_CLIENT: i32 = -1;

/// The binder handles of the rules.
pub(crate) struct Svc;

impl Kinds for Svc {
    type Token = SpIBinder;
    type Conn = ConnHandle;
    type Binder = SpIBinder;
    type Thread = Strong<dyn IApplicationThread>;
    type Intent = Intent;
    type Info = ServiceInfo;
}

pub(crate) type Services = bound_services::Services<Svc>;

/// An IServiceConnection, equal when the binder is.
#[derive(Clone)]
pub(crate) struct ConnHandle(pub Strong<dyn IServiceConnection>);

impl PartialEq for ConnHandle {
    fn eq(&self, o: &Self) -> bool {
        self.0.as_binder() == o.0.as_binder()
    }
}

/// The sender thread of the scheduled calls.
pub(crate) struct Outbox {
    tx: Mutex<Sender<Vec<Call<Svc>>>>,
}

impl Outbox {
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Vec<Call<Svc>>>();
        std::thread::Builder::new()
            .name("ActiveServices".into())
            .spawn(move || {
                for calls in rx {
                    for call in calls {
                        send(call);
                    }
                }
            })
            .expect("starting the ActiveServices thread");
        Outbox { tx: Mutex::new(tx) }
    }

    /// Queues `calls`, in order; called under the state lock so the queue's order is the lock's.
    pub(crate) fn post(&self, calls: Vec<Call<Svc>>) {
        if calls.is_empty() {
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        // the receiver ends with the process; a failed send means shutdown
        let _ = tx.send(calls);
    }
}

/// One call; a failure is logged and counted (the target process died: its death recipient cleans up).
fn send(call: Call<Svc>) {
    let (what, r) = match call {
        Call::Create {
            thread,
            token,
            info,
        } => (
            "scheduleCreateService",
            thread.r#scheduleCreateService(
                Some(&token),
                Some(&info),
                Some(&CompatibilityInfo::default()),
                PROCESS_STATE_UNKNOWN,
            ),
        ),
        Call::Bind {
            thread,
            token,
            intent,
            rebind,
            seq,
        } => (
            "scheduleBindService",
            thread.r#scheduleBindService(
                Some(&token),
                Some(&intent),
                rebind,
                PROCESS_STATE_UNKNOWN,
                seq,
            ),
        ),
        Call::Unbind {
            thread,
            token,
            intent,
        } => (
            "scheduleUnbindService",
            thread.r#scheduleUnbindService(Some(&token), Some(&intent)),
        ),
        Call::Stop { thread, token } => (
            "scheduleStopService",
            thread.r#scheduleStopService(Some(&token)),
        ),
        Call::Connected {
            conn,
            component,
            binder,
            dead,
        } => {
            let name = ComponentName {
                package: component.0,
                class: component.1,
            };
            (
                "IServiceConnection.connected",
                conn.0.r#connected(Some(&name), binder.as_ref(), dead),
            )
        }
    };
    if let Err(e) = r {
        atl_diag::answered(AM, &format!("{what} failed (target gone)"));
        log(format_args!("{what}: {e:?}"));
    }
}

/// bindServiceLocked's answer to the app (ContextImpl.bindServiceCommon reads it): 1 bound, 0 no such service, -1 permission denied (ContextImpl throws SecurityException).
pub(crate) enum Bound {
    Ok,
    NotFound,
    Denied,
}

impl Bound {
    pub(crate) fn code(&self) -> i32 {
        match self {
            Bound::Ok => 1,
            Bound::NotFound => 0,
            Bound::Denied => -1,
        }
    }
}

impl Inner {
    /// ActivityManager.checkComponentPermission (ActiveServices.retrieveServiceLocked's check): root and system pass, the owning app passes, a non-exported service refuses everyone else,
    /// then the service's permission (if any) must be held.
    fn may_bind(
        &self,
        caller_uid: u32,
        owner_uid: u32,
        exported: bool,
        perm: Option<&str>,
    ) -> bool {
        if caller_uid == ROOT_UID
            || caller_uid == SYSTEM_UID
            || permission::app_id(caller_uid) == permission::app_id(owner_uid)
        {
            return true;
        }
        if !exported {
            return false;
        }
        perm.is_none_or(|p| self.holds(caller_uid, p))
    }

    /// bindServiceLocked under the state lock: lookup, permission, the rules, and the process start the rules ask for (headless, as a broadcast's cold start).
    pub(crate) fn bind_service_locked(
        &self,
        st: &mut State,
        client_pid: i32,
        caller_uid: u32,
        intent: &Intent,
        conn: Strong<dyn IServiceConnection>,
        flags: i64,
    ) -> Bound {
        if !self.bind_services {
            atl_diag::answered(AM, "bindService: disabled (--no-bind-service)");
            return Bound::NotFound;
        }
        let Some(c) = &intent.component else {
            atl_diag::answered(AM, "bindService: implicit Intent (not resolved)");
            return Bound::NotFound;
        };
        let Some(p) = self.packages.by_name(&c.package) else {
            return Bound::NotFound;
        };
        let Some(decl) = p.service(&c.class).filter(|s| s.component.enabled) else {
            atl_diag::answered(AM, "bindService: no enabled <service>");
            return Bound::NotFound;
        };
        if !self.may_bind(
            caller_uid,
            p.uid,
            decl.component.exported,
            decl.permission.as_deref(),
        ) {
            log(format_args!(
                "Permission Denial: Accessing service {}/{} from uid={caller_uid} requires {:?} (exported={})",
                c.package, c.class, decl.permission, decl.component.exported
            ));
            return Bound::Denied;
        }
        let process = decl
            .component
            .process_name
            .as_deref()
            .unwrap_or(p.process_name())
            .to_owned();
        let running = st
            .processes
            .iter()
            .find(|r| r.process_name == process && r.uid == p.uid);
        let host = running
            .filter(|r| r.broadcasts_attached)
            .and_then(|r| r.attached.as_ref().map(|a| (r.pid, a.thread.clone())));
        let starting = running.is_some();
        // the bound Intent as the service gets it: the caller's, with its component (ServiceRecord's IntentBindRecord.intent)
        let mut calls = Vec::new();
        let req = Bind {
            component: (c.package.clone(), c.class.clone()),
            uid: p.uid,
            process: process.clone(),
            info: package::service_info(p, &self.packages, decl, package::STOCK_PM_FLAGS),
            intent: intent.clone(),
            key: intent_key(intent, None),
            conn: ConnHandle(conn),
            client_pid,
            flags,
        };
        let need = st.services.bind(req, host, crate::token::new, &mut calls);
        self.service_calls.post(calls);
        if need == Host::NeedsProcess && !starting {
            if let Err(e) = self.start_process_locked(st, p, &process, false, None) {
                log(format_args!(
                    "Unable to launch app {process} for service {}/{}: {e}",
                    c.package, c.class
                ));
                let mut calls = Vec::new();
                st.services.start_failed(&process, p.uid, &mut calls);
                self.service_calls.post(calls);
                atl_diag::answered(AM, "bindService: service process start failed");
                return Bound::Ok;
            }
            atl_diag::answered(AM, "bindService cold start (bringUpServiceLocked)");
        }
        Bound::Ok
    }

    /// unbindServiceLocked under the state lock.
    pub(crate) fn unbind_service_locked(
        &self,
        st: &mut State,
        conn: Strong<dyn IServiceConnection>,
    ) -> bool {
        let mut calls = Vec::new();
        let found = st.services.unbind(&ConnHandle(conn), &mut calls);
        self.service_calls.post(calls);
        found
    }

    /// killServicesLocked + the client half of cleanUpApplicationRecordLocked for a dead or killed process.
    pub(crate) fn services_died(&self, st: &mut State, pid: i32) {
        let mut calls = Vec::new();
        for _ in 0..st.services.process_died(pid, &mut calls) {
            atl_diag::answered(
                AM,
                "service process died: restart not modelled (waits for the next bind)",
            );
        }
        self.service_calls.post(calls);
    }

    /// attachApplicationLocked's pending services (mServices.attachApplicationLocked).
    pub(crate) fn services_attached(&self, st: &mut State, pid: i32) {
        let Some(r) = st.processes.iter().find(|r| r.pid == pid) else {
            return;
        };
        let Some(thread) = r.attached.as_ref().map(|a| a.thread.clone()) else {
            return;
        };
        let (process, uid) = (r.process_name.clone(), r.uid);
        let mut calls = Vec::new();
        st.services.attach(pid, &process, uid, &thread, &mut calls);
        self.service_calls.post(calls);
    }

    /// The services' own bind (JobServiceContext's bindServiceAsUser): as the system uid, no client process.
    pub(crate) fn bind_service_as_system(
        &self,
        intent: &Intent,
        conn: Strong<dyn IServiceConnection>,
        flags: i64,
    ) -> Bound {
        let mut st = self.state();
        self.bind_service_locked(&mut st, SYSTEM_CLIENT, SYSTEM_UID, intent, conn, flags)
    }
}
