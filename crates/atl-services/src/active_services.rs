// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActiveServices.java
//! The activity service's services (a child module of activity.rs, so it shares its state lock): the binder half of bindServiceLocked, startServiceLocked, stopServiceLocked,
//! stopServiceTokenLocked, serviceDoneExecutingLocked, publishService, unbindService, unbindFinished, retrieveServiceLocked (lookup + checkComponentPermission), stopInBackgroundLocked,
//! and the sender of the calls service_records.rs returns. The rules are service_records.rs (records, restarts) and uid_state.rs (the background-start check); the restart timers are
//! am_handler's (activity.rs on_timer, [`Inner::restart_service`]).
//! Calls leave on one thread in the order the state lock produced them (broadcast_queue.rs's reason: no call out of the system process with the lock held; in-process connections, the job runner's,
//! run their `connected` there too, so they may call back into the services).
//! Implicit service Intents resolve as retrieveServiceLocked's resolveService does (resolve_implicit: the first of package::query_intent_services, STOCK_PM_FLAGS); no match answers 0, counted. Foreground services (S7): the rules are service_records.rs; this file sends the startForeground wait, its timeout (stop, counted), setServiceForeground / getForegroundServiceType and the notice posts. Not modelled: AOSP's MATCH_DEBUG_TRIAGED_MISSING (ActiveServices.java:4877), instanceName (ignored, counted), bindings across users, the background caller's delayed start (counted), the background FGS start gate (AS :1036-1052, counted), the FGS ANR (counted).

use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};

use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_aidl::aidl::android::app::IServiceConnection::IServiceConnection;
use atl_parcelables::android::app::Notification;
use atl_parcelables::android::content::pm::{ParceledListSlice, ServiceInfo};
use atl_parcelables::android::content::res::CompatibilityInfo;
use atl_parcelables::android::content::{ComponentName, Intent};
use atl_parcelables::codecs::android::app::ServiceStartArgs::ServiceStartArgs;
use atl_parcelables::codecs::android::content::pm::ResolveInfo::Resolved;
use binder::{SpIBinder, Strong};

use super::{
    AM, Effects, Inner, LostWaiter, ProcessRecord, SERVICE_START_FOREGROUND_ANR_DELAY_MS,
    SERVICE_START_FOREGROUND_TIMEOUT_MS, State, Timer, intent_key, log, security,
};
use crate::broadcast_dispatch::ProcKey;
use crate::notification_display::FLAG_FOREGROUND_SERVICE;
use crate::package;
use crate::service_records::{self, Bind, Call, Foreground, Host, Kinds, Start};
use crate::uid_state::{self, StartMode};
use atl_services_types::permission::{self, ROOT_UID, SYSTEM_UID};

/// ActivityManager.PROCESS_STATE_UNKNOWN: the processState the scheduled calls carry (process states are not tracked).
const PROCESS_STATE_UNKNOWN: i32 = -1;
/// The client pid of a binding the services make for themselves (the job runner): no process record, never dies.
pub(crate) const SYSTEM_CLIENT: i32 = -1;
/// ActivityThread.SERVICE_DONE_EXECUTING_START (ActivityThread.java:337; ANON 0 :335, STOP 2 :339): the serviceDoneExecuting type of an onStartCommand result.
const SERVICE_DONE_EXECUTING_START: i32 = 1;
/// Build.VERSION_CODES.O: a startForegroundService below it is a plain start (AS :1120-1125 drops fgRequired).
const SDK_O: i32 = 26;
/// Build.VERSION_CODES.P: the FOREGROUND_SERVICE permission is enforced from here (AS :2197-2198).
const SDK_P: i32 = 28;
/// Build.VERSION_CODES.UPSIDE_DOWN_CAKE: the FGS type checks (NONE disabled, DATA_SYNC permission enforced) apply from here (ForegroundServiceTypePolicy :112-161, EnabledAfter TIRAMISU).
const SDK_UPSIDE_DOWN_CAKE: i32 = 34;
/// ServiceInfo.FOREGROUND_SERVICE_TYPE_NONE (ServiceInfo.java:139) and _MANIFEST (:604): the requested type that means "the manifest's".
const TYPE_NONE: i32 = 0;
const TYPE_MANIFEST: i32 = -1;
/// ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC (ServiceInfo.java:173).
const TYPE_DATA_SYNC: i32 = 1;
/// Service.STOP_FOREGROUND_REMOVE / STOP_FOREGROUND_DETACH (Service.java:336, :343): setServiceForeground's flags on a stop.
const STOP_FOREGROUND_REMOVE: i32 = 1;
const STOP_FOREGROUND_DETACH: i32 = 1 << 1;
/// Manifest.permission.FOREGROUND_SERVICE, and FOREGROUND_SERVICE_DATA_SYNC (the DATA_SYNC type's permission, ForegroundServiceTypePolicy FGS_TYPE_POLICY_DATA_SYNC).
const FOREGROUND_SERVICE_PERMISSION: &str = "android.permission.FOREGROUND_SERVICE";
const FOREGROUND_SERVICE_DATA_SYNC_PERMISSION: &str =
    "android.permission.FOREGROUND_SERVICE_DATA_SYNC";

/// A notification to post once the state lock is released (setServiceForegroundInnerLocked's postNotification, ServiceRecord.java).
pub(crate) struct Posting {
    pub(crate) uid: u32,
    pub(crate) package: String,
    pub(crate) id: i32,
    pub(crate) notification: atl_parcelables::android::app::Notification,
}

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

pub(crate) type Services = service_records::Services<Svc>;

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
        Call::Args {
            thread,
            token,
            args,
        } => {
            let items: Vec<ServiceStartArgs> = args
                .into_iter()
                .map(|a| ServiceStartArgs {
                    task_removed: a.task_removed,
                    start_id: a.start_id,
                    flags: a.flags,
                    args: a.intent,
                })
                .collect();
            (
                "scheduleServiceArgs",
                thread.r#scheduleServiceArgs(Some(&token), Some(&ParceledListSlice::new(items))),
            )
        }
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

/// retrieveServiceLocked's answer for a Service Intent: the service that passed (`Found`), no such service (`NotFound`, with the diagnostic the caller counts, if any), or a refused
/// permission (`Denied`, with the permission the service needs or "private to package").
pub(crate) enum Lookup {
    /// boxed: Looked carries a ServiceInfo, far larger than the refusals (clippy::large_enum_variant)
    Found(Box<Looked>),
    NotFound(Option<&'static str>),
    Denied(String),
}

/// The service a lookup found: its component, uid, process and ServiceInfo, and what the background-start check reads (targetSdk, FLAG_PERSISTENT).
pub(crate) struct Looked {
    pub(crate) component: (String, String),
    pub(crate) uid: u32,
    pub(crate) process: String,
    pub(crate) info: ServiceInfo,
    pub(crate) target_sdk: i32,
    pub(crate) persistent: bool,
}

/// A ComponentName the binder answers with, as ActiveServices builds its sentinels: "!" a permission, "?" a background start, "!!" a failed process start.
fn sentinel(code: &str, reason: String) -> ComponentName {
    ComponentName {
        package: code.to_owned(),
        class: reason,
    }
}

/// The attached process of (process, uid), if one runs: its pid and thread, the host a service call starts in (bringUpServiceLocked's `app != null`).
fn running_host(
    st: &State,
    process: &str,
    uid: u32,
) -> Option<(i32, Strong<dyn IApplicationThread>)> {
    st.processes
        .iter()
        .find(|r| r.process_name == process && r.uid == uid)
        .filter(|r| r.attach_finished)
        .and_then(|r| r.attached.as_ref().map(|a| (r.pid, a.thread.clone())))
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

    /// retrieveServiceLocked's resolveService for an implicit Intent (no component): the first of queryIntentServices (resolveServiceInternal:453, highest priority), STOCK_PM_FLAGS
    /// (no MATCH_DEFAULT_ONLY), as the system user. `resolvedType` is the Intent's own type: bindServiceInstance gets the caller's but does not pass it down (intent_key falls back to the Intent's type).
    fn resolve_implicit(&self, caller_uid: u32, intent: &Intent) -> Option<ComponentName> {
        let first = package::query_intent_services(
            &self.packages,
            caller_uid,
            intent,
            None,
            package::STOCK_PM_FLAGS,
            package::USER_SYSTEM,
        )
        .into_iter()
        .next()?;
        let Resolved::Service(s) = first.info else {
            return None;
        };
        Some(ComponentName {
            package: s.component.item.package_name?,
            class: s.component.item.name?,
        })
    }

    /// retrieveServiceLocked (AS :4711): the component of `intent` (an implicit one resolved first), its package and declaration (a disabled one is not found), then the permission check.
    /// The record is made by the caller's start or bind; a miss's diagnostic is the caller's to count.
    fn lookup_service(&self, caller_uid: u32, intent: &Intent) -> Lookup {
        let component = match &intent.component {
            Some(c) => c.clone(),
            None => match self.resolve_implicit(caller_uid, intent) {
                Some(c) => c,
                None => return Lookup::NotFound(Some("implicit Intent matched no <service>")),
            },
        };
        let Some(p) = self.packages.by_name(&component.package) else {
            return Lookup::NotFound(Some("no such package"));
        };
        let Some(decl) = p
            .service(&component.class)
            .filter(|s| self.packages.enabled(&s.component, 0))
        else {
            return Lookup::NotFound(Some("no enabled <service>"));
        };
        if !self.may_bind(
            caller_uid,
            p.uid,
            decl.component.exported,
            decl.permission.as_deref(),
        ) {
            log(format_args!(
                "Permission Denial: Accessing service {}/{} from uid={caller_uid} requires {:?} (exported={})",
                component.package, component.class, decl.permission, decl.component.exported
            ));
            atl_diag::refused(AM, "service permission denied");
            return Lookup::Denied(
                decl.permission
                    .clone()
                    .unwrap_or_else(|| "private to package".to_owned()),
            );
        }
        let process = decl
            .component
            .process_name
            .as_deref()
            .unwrap_or(p.process_name())
            .to_owned();
        Lookup::Found(Box::new(Looked {
            component: (component.package.clone(), component.class.clone()),
            uid: p.uid,
            process,
            info: package::service_info(p, &self.packages, decl, package::STOCK_PM_FLAGS),
            target_sdk: p.app().target_sdk_version,
            persistent: self.persistent_app(&component.package),
        }))
    }

    /// The one headless process start (ProcessList.startProcessLocked with no activity launch), shared by services (bringUpServiceLocked) and manifest receivers (a broadcast's cold start).
    /// Nothing starts while a record of (process, uid) exists, attached or starting: Ok(false). Ok(true) = a process was started.
    pub(crate) fn start_headless_locked(
        &self,
        st: &mut State,
        package: &str,
        process: &str,
        uid: u32,
    ) -> Result<bool, String> {
        if st
            .processes
            .iter()
            .any(|r| r.process_name == process && r.uid == uid)
        {
            return Ok(false);
        }
        match self.packages.by_name(package) {
            Some(p) => self
                .start_process_locked(st, p, process, false, None)
                .map(|()| true)
                .map_err(|e| e.to_string()),
            None => Err(format!("no package {package}")),
        }
    }

    /// bringUpServiceLocked's process start, reduced: a service call that needs a process starts it headless, once per process name and uid (`starting`: a record exists).
    /// A failed start brings the services waiting for it down (`start_failed`) and returns the reason.
    pub(crate) fn bring_up_locked(
        &self,
        st: &mut State,
        package: &str,
        process: &str,
        uid: u32,
        need: Host,
    ) -> Result<(), String> {
        if need != Host::NeedsProcess {
            return Ok(());
        }
        match self.start_headless_locked(st, package, process, uid) {
            Ok(false) => Ok(()),
            Ok(true) => {
                atl_diag::answered(AM, "service cold start (bringUpServiceLocked)");
                Ok(())
            }
            Err(e) => {
                let mut calls = Vec::new();
                st.services.start_failed(process, uid, &mut calls);
                self.services_out(st, calls);
                atl_diag::answered(AM, "service process start failed");
                Err(e)
            }
        }
    }

    /// bindServiceLocked under the state lock: lookup, permission, the rules, and the process start the rules ask for (headless, as a broadcast's cold start).
    /// An implicit Intent resolves first (lookup_service); the bound Intent stays the caller's.
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
        let looked = match self.lookup_service(caller_uid, intent) {
            Lookup::Found(l) => *l,
            Lookup::NotFound(why) => {
                if let Some(why) = why {
                    atl_diag::answered(AM, why);
                }
                return Bound::NotFound;
            }
            Lookup::Denied(_) => return Bound::Denied,
        };
        let host = running_host(st, &looked.process, looked.uid);
        // the bound Intent as the service gets it: the caller's, unchanged (ServiceRecord's IntentBindRecord.intent); an implicit one keeps no component: retrieveServiceLocked sets it only for BIND_EXTERNAL_SERVICE (ActiveServices.java:4956)
        let mut calls = Vec::new();
        let req = Bind {
            component: looked.component.clone(),
            uid: looked.uid,
            process: looked.process.clone(),
            info: looked.info,
            intent: intent.clone(),
            key: intent_key(intent, None),
            conn: ConnHandle(conn),
            client_pid,
            caller_uid,
            flags,
        };
        let need = st.services.bind(
            req,
            host,
            crate::token::new,
            self.services_now(),
            &mut calls,
        );
        self.services_out(st, calls);
        if let Err(e) =
            self.bring_up_locked(st, &looked.component.0, &looked.process, looked.uid, need)
        {
            log(format_args!(
                "Unable to launch app {} for service {}/{}: {e}",
                looked.process, looked.component.0, looked.component.1
            ));
        }
        Bound::Ok
    }

    /// startServiceLocked (AS :945) under the state lock: the lookup (`!` a permission), the background check (`?`, uid_state), then startServiceInnerLocked: the start goes to the
    /// services, and the process start the rules ask for. `caller_fg`: the caller has a visible Task (AS callerFg; a call with no caller app is foreground). `require_foreground`: startForegroundService.
    /// Returns the ComponentName the binder answers (ContextImpl reads it): the service's own, or `!` / `?` / `!!` (a failed process start). None: no such service.
    pub(crate) fn start_service_locked(
        &self,
        st: &mut State,
        caller_uid: u32,
        caller_fg: bool,
        intent: &Intent,
        require_foreground: bool,
    ) -> Option<ComponentName> {
        let looked = match self.lookup_service(caller_uid, intent) {
            Lookup::Found(l) => *l,
            Lookup::NotFound(why) => {
                if let Some(why) = why {
                    atl_diag::answered(AM, why);
                }
                return None;
            }
            Lookup::Denied(permission) => return Some(sentinel("!", permission)),
        };
        let started = st.services.start_requested(&looked.component);
        if uid_state::background_start_refused(
            &st.uids,
            looked.uid,
            looked.target_sdk,
            looked.persistent,
            started,
            require_foreground,
            self.now_ms(),
        ) {
            atl_diag::answered(
                AM,
                "startService: background start refused (ServiceStartNotAllowedException)",
            );
            return Some(sentinel(
                "?",
                format!("app is in background uid {}", looked.uid),
            ));
        }
        if require_foreground {
            if looked.target_sdk < SDK_O {
                atl_diag::answered(
                    AM,
                    "startForegroundService: targetSdk below 26, plain start (AS :1120-1125)",
                );
            }
            if st.uids.is_idle(looked.uid) {
                atl_diag::answered(
                    AM,
                    "startForegroundService: background restriction not modelled (AS :1036-1052)",
                );
            }
        }
        let running = st
            .processes
            .iter()
            .any(|r| r.process_name == looked.process && r.uid == looked.uid);
        if !caller_fg && !require_foreground && !running {
            atl_diag::answered(
                AM,
                "startService: background caller, process not running: AOSP may delay the start (mDelayedStartList), started now",
            );
        }
        let host = running_host(st, &looked.process, looked.uid);
        let mut calls = Vec::new();
        let req = Start {
            component: looked.component.clone(),
            uid: looked.uid,
            process: looked.process.clone(),
            info: looked.info,
            intent: intent.clone(),
            caller_uid,
            foreground: require_foreground && looked.target_sdk >= SDK_O,
        };
        let need = st.services.start(
            req,
            host,
            crate::token::new,
            self.services_now(),
            &mut calls,
        );
        self.services_out(st, calls);
        let name = ComponentName {
            package: looked.component.0.clone(),
            class: looked.component.1.clone(),
        };
        match self.bring_up_locked(st, &looked.component.0, &looked.process, looked.uid, need) {
            Ok(()) => Some(name),
            Err(e) => {
                log(format_args!(
                    "Unable to launch app {} for service {}/{}: {e}",
                    looked.process, looked.component.0, looked.component.1
                ));
                Some(sentinel("!!", e))
            }
        }
    }

    /// startService from a PendingIntent of kind SERVICE / FOREGROUND_SERVICE (PendingIntentRecord.sendInner :694, startServiceInPackage): the start as the creator `uid`, with no
    /// caller process (callerFg true). The system ignores the start's result (it logs and swallows the exceptions, :696-700), so this reports nothing back.
    pub(crate) fn start_service_as(
        &self,
        intent: &Intent,
        uid: u32,
        foreground: bool,
        grant: Option<crate::uid_state::TempGrant>,
    ) {
        let mut st = self.state();
        // the grant is the creator's, applied before the start reads it (PendingIntentRecord :604-613: tempAllowlistForPendingIntent precedes startServiceInPackage)
        if let Some(grant) = grant {
            self.grant_temp_locked(&mut st, uid, grant);
        }
        let _ = self.start_service_locked(&mut st, uid, true, intent, foreground);
        atl_diag::answered(AM, "PendingIntent service send: start result not reported");
    }

    /// stopServiceLocked under the state lock (Context.stopService, AS :1650): 1 the start was dropped, 0 no such service, -1 a refused permission. A service still needed by a bind keeps running.
    pub(crate) fn stop_service_locked(
        &self,
        st: &mut State,
        caller_uid: u32,
        intent: &Intent,
    ) -> i32 {
        let component = match self.lookup_service(caller_uid, intent) {
            Lookup::Found(l) => l.component,
            Lookup::NotFound(_) => {
                atl_diag::answered(AM, "stopService: no such service");
                return 0;
            }
            Lookup::Denied(_) => return -1,
        };
        let mut calls = Vec::new();
        let stopped = st.services.stop_service(&component, &mut calls);
        self.services_out(st, calls);
        // a released start may leave its process empty: its headless grace starts now (activity.rs touch_all_locked)
        self.touch_all_locked(st);
        i32::from(stopped)
    }

    /// stopServiceTokenLocked under the state lock (Service.stopSelf, AS :1781): the token's record must be named `class`; `start_id` stops only the most recent start
    /// (service_records.rs stop_self). False when refused, or no such service.
    pub(crate) fn stop_self_locked(
        &self,
        st: &mut State,
        class: Option<&ComponentName>,
        token: &SpIBinder,
        start_id: i32,
    ) -> bool {
        let Some(class) = class else {
            return false;
        };
        let named = st
            .services
            .component_of(token)
            .is_some_and(|c| c == (class.package.clone(), class.class.clone()));
        if !named {
            atl_diag::refused(AM, "stopSelf: no such service (name or token differ)");
            return false;
        }
        let mut calls = Vec::new();
        let stopped = st.services.stop_self(token, start_id, &mut calls);
        self.services_out(st, calls);
        if stopped {
            self.touch_all_locked(st);
        } else {
            atl_diag::refused(AM, "stopSelf: startId is not the last start");
        }
        stopped
    }

    /// serviceDoneExecutingLocked under the state lock (AS :6705): a START result goes to the services (service_records.rs done_executing_start: the IllegalArgumentException for an
    /// unknown result). STOP and ANON (the destroy and the bind callbacks) change nothing here: the record already went at bring-down, and no ANR waits on them.
    pub(crate) fn service_done_executing_locked(
        &self,
        st: &mut State,
        token: Option<&SpIBinder>,
        kind: i32,
        start_id: i32,
        res: i32,
    ) -> binder::Result<()> {
        if kind != SERVICE_DONE_EXECUTING_START {
            atl_diag::answered(AM, "serviceDoneExecuting: not a start (nothing to finish)");
            return Ok(());
        }
        let Some(token) = token else {
            atl_diag::answered(AM, "serviceDoneExecuting: no token (nothing to finish)");
            return Ok(());
        };
        st.services
            .done_executing_start(token, start_id, res)
            .map_err(|e| {
                atl_diag::refused(AM, "serviceDoneExecuting: unknown start result");
                super::illegal_argument(&format!("Unknown service start result: {}", e.0))
            })
    }

    /// unbindServiceLocked under the state lock.
    pub(crate) fn unbind_service_locked(
        &self,
        st: &mut State,
        conn: Strong<dyn IServiceConnection>,
    ) -> bool {
        let mut calls = Vec::new();
        let found = st.services.unbind(&ConnHandle(conn), &mut calls);
        self.services_out(st, calls);
        // a released binding may leave its process empty: its headless grace starts now (activity.rs touch_all_locked)
        self.touch_all_locked(st);
        found
    }

    /// killServicesLocked + the client half of cleanUpApplicationRecordLocked for a dead or killed process. `allow_restart` false: the process was ended with its window
    /// (`--window-close-stops-services`); AOSP restarts STICKY services only when it is true (killServicesLocked :7202, allowRestart). Each restart gets its am_handler timer at its time.
    pub(crate) fn services_died(&self, st: &mut State, pid: i32, allow_restart: bool) {
        let mut calls = Vec::new();
        let restarts =
            st.services
                .process_died(pid, allow_restart, self.services_now(), &mut calls);
        self.services_out(st, calls);
        for restart in restarts {
            atl_diag::answered(AM, "service restart scheduled (am_handler timer)");
            // the harness's not-sticky fence reads this line before the death line app_died logs after this call
            log(format_args!(
                "restart of {}/{} scheduled (am_handler timer)",
                restart.component.0, restart.component.1
            ));
            let at = u64::try_from(restart.at).unwrap_or(0);
            self.arm(Timer::Restart(restart.component), at);
        }
    }

    /// The restart timer of `component` fired (am_handler, at its time): performServiceRestartLocked. A restart taken meanwhile (a start, a bind, an attach) is nothing here.
    pub(crate) fn restart_service(&self, component: (String, String)) {
        let mut st = self.state();
        let Some((process, uid)) = st.services.process_of(&component) else {
            return;
        };
        let host = running_host(&st, &process, uid);
        let mut calls = Vec::new();
        let need = st
            .services
            .restart(&component, host, self.services_now(), &mut calls);
        self.services_out(&mut st, calls);
        if let Err(e) = self.bring_up_locked(&mut st, &component.0, &process, uid, need) {
            log(format_args!(
                "restart of {}/{} failed: {e}",
                component.0, component.1
            ));
        }
    }

    /// PROC_START_TIMEOUT_MSG (AMS handleProcessStartOrKillTimeoutLocked :4444, "failed to attach"), for the process started at `start_seq` if it is still unattached: the record
    /// leaves, the services waiting in it come down (processStartTimedOutLocked :6969, "Forcing bringing down service"), its Task and receivers go as at a death, and the pid is
    /// killed (killLocked "start timeout"). A process that dies before attach never attaches: this timeout is its only end (AMS has no binder death for it).
    pub(crate) fn process_start_timed_out(&self, start_seq: i64) {
        let mut st = self.state();
        let Some(i) = st
            .processes
            .iter()
            .position(|r| r.start_seq == start_seq && r.attached.is_none())
        else {
            return;
        };
        let rec = st.processes.remove(i);
        log(format_args!(
            "process {} (pid {}) failed to attach: start timeout at={}",
            rec.process_name,
            rec.pid,
            atl_diag::wall_stamp()
        ));
        let lost = self.unattached_ended_locked(&mut st, &rec);
        if let Some(task) = rec.task {
            self.task_gone_locked(&mut st, task);
        }
        self.touch_all_locked(&mut st);
        drop(st);
        self.deliver(Effects {
            pids: vec![rec.pid],
            lost,
        });
    }

    /// The end of an unattached process, its record already out of the table: every side is told from here, and only here. The services waiting for the process come down
    /// (processStartTimedOutLocked :6969); the broadcast side's cold start fails and its registrations go (onApplicationTimeoutLocked -> onApplicationCleanupLocked, BroadcastQueueImpl
    /// :709-745); the providers it was to publish go, their waiters returned (cleanupAppInLaunchingProvidersLocked :1893-1916, called at :4483). Called by the start timeout and by
    /// force-stop; the caller delivers the returned waiters and kills the pid.
    pub(crate) fn unattached_ended_locked(
        &self,
        st: &mut State,
        rec: &ProcessRecord,
    ) -> Vec<LostWaiter> {
        let mut calls = Vec::new();
        st.services
            .start_failed(&rec.process_name, rec.uid, &mut calls);
        self.services_out(st, calls);
        let key = ProcKey {
            name: rec.process_name.clone(),
            uid: rec.uid,
        };
        self.start_failed_locked(st, &key);
        self.broadcasts_died_locked(st, rec.pid);
        // unpublished records only: a published one of the same key belongs to a live process (start_launcher starts without the guard)
        let lost = Self::remove_providers_of_process(st, &rec.package, &rec.process_name, |r| {
            r.provider.is_none()
        });
        if !lost.is_empty() {
            atl_diag::answered(AM, "provider waiters released: process never attached");
        }
        lost
    }

    /// stopInBackgroundLocked (AS :1685), from a uid's settle sweep: its started services whose start mode is not NORMAL stop (a targetSdk 26+ app, now idle, not persistent).
    pub(crate) fn stop_in_background_locked(&self, st: &mut State, uid: u32) {
        let now = self.now_ms();
        let mut calls = Vec::new();
        let mut stopped = 0_u32;
        for component in st.services.started_of_uid(uid) {
            let Some(p) = self.packages.by_name(&component.0) else {
                continue;
            };
            let persistent = self.persistent_app(&component.0);
            if st
                .uids
                .start_mode(uid, p.app().target_sdk_version, persistent, now)
                == StartMode::Normal
            {
                continue;
            }
            log(format_args!(
                "Stopping service due to app idle: uid {uid} {}/{}",
                component.0, component.1
            ));
            if st.services.stop_service(&component, &mut calls) {
                stopped += 1;
            }
        }
        self.services_out(st, calls);
        if stopped > 0 {
            atl_diag::answered(AM, "service stopped: app idle (stopInBackgroundLocked)");
            self.touch_all_locked(st);
        }
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
        st.services
            .attach(pid, &process, uid, &thread, self.services_now(), &mut calls);
        self.services_out(st, calls);
    }

    /// The services' outbound effects under `st`: the calls go out in order, then the foreground changes the rules made are applied (foreground_effects_locked).
    fn services_out(&self, st: &mut State, calls: Vec<Call<Svc>>) {
        self.service_calls.post(calls);
        self.foreground_effects_locked(st);
    }

    /// The foreground changes the rules made under the lock. The fgRequired timers are armed for SERVICE_START_FOREGROUND_TIMEOUT_MS from now (AS :7731-7737, at the delivery).
    /// An ended foreground state drops its uid's FGS reason (uid_state; the settle sweep arms if the uid is now idle) and cancels its notification.
    fn foreground_effects_locked(&self, st: &mut State) {
        let fx = st.services.take_foreground();
        let now = self.now_ms();
        for component in fx.arms {
            self.arm(
                Timer::FgsStart(component),
                now + SERVICE_START_FOREGROUND_TIMEOUT_MS,
            );
        }
        for end in fx.ends {
            if end.was_foreground {
                atl_diag::answered(AM, "foreground service ended: uid FGS reason drops");
                let at = st.uids.on_fgs_stopped(end.uid, now);
                self.arm_settle(end.uid, at);
            }
            if let Some(id) = end.cancel_notice {
                self.notices.cancel(&end.component.0, id);
            }
        }
    }

    /// The ANR of a startForeground timeout, 10 s after its stop (AS :7640 -> ProcessErrorStateRecord :672-673): a headless process gets the silent background-ANR kill; a
    /// windowed one is only counted (the ANR dialog is not modelled). A process that ended first is counted and left alone.
    pub(crate) fn foreground_anr(&self, pid: i32) {
        let headless = self
            .state()
            .processes
            .iter()
            .find(|r| r.pid == pid)
            .map(|r| r.task.is_none());
        let Some(headless) = headless else {
            atl_diag::answered(AM, "fgs start timeout: process ended before the ANR");
            return;
        };
        if !headless {
            atl_diag::answered(AM, "fgs start timeout: ANR not modelled");
            return;
        }
        atl_diag::answered(
            AM,
            "fgs start timeout: headless process ended (silent bg ANR)",
        );
        self.kill(&[pid]);
    }

    /// The fgRequired timer of `component` (serviceForegroundTimeout, AS :7597-7628): a service still waiting for startForeground is stopped (stopServiceLocked, which clears its
    /// start). The ANR follows 10 s later (AS :7640, foreground_anr). A timer whose wait is over is discarded (AS :7610-7613).
    pub(crate) fn foreground_timeout(&self, component: (String, String)) {
        let mut st = self.state();
        if !st.services.foreground_expired(&component) {
            return;
        }
        let pid = st.services.pid_of(&component);
        log(format_args!(
            "service {}/{} did not call startForeground within {SERVICE_START_FOREGROUND_TIMEOUT_MS} ms: stopped",
            component.0, component.1
        ));
        if let Some(pid) = pid {
            self.arm(
                Timer::FgsAnr(pid),
                self.now_ms() + SERVICE_START_FOREGROUND_ANR_DELAY_MS,
            );
        }
        let mut calls = Vec::new();
        st.services.stop_service(&component, &mut calls);
        self.touch_all_locked(&mut st);
        self.services_out(&mut st, calls);
    }

    /// setServiceForegroundLocked (AS :1842) under the state lock, with setServiceForegroundInnerLocked's checks (AS :2165-2260, :2578-2613). `class` and `token` name the
    /// service (findServiceLocked: a mismatch is a silent no-op). An id makes it foreground and its uid's FGS reason starts; the notification comes back to post once the lock is
    /// released. Id 0 is stopForeground: `flags` REMOVE cancels the notification, DETACH forgets it. `requested` is the type the service passed (MANIFEST: its manifest's).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_service_foreground_locked(
        &self,
        st: &mut State,
        class: Option<&ComponentName>,
        token: Option<&SpIBinder>,
        id: i32,
        notification: Option<&Notification>,
        flags: i32,
        requested: i32,
    ) -> binder::Result<Option<Posting>> {
        let (Some(class), Some(token)) = (class, token) else {
            atl_diag::answered(AM, "setServiceForeground: no such service");
            return Ok(None);
        };
        let Some(component) = st
            .services
            .component_of(token)
            .filter(|c| c.0 == class.package && c.1 == class.class)
        else {
            atl_diag::answered(
                AM,
                "setServiceForeground: no such service (name or token differ)",
            );
            return Ok(None);
        };
        let Some(package) = self.packages.by_name(&component.0) else {
            return Ok(None);
        };
        let (uid, target_sdk) = (package.uid, package.app().target_sdk_version);
        if id == 0 {
            st.services.stop_foreground(
                token,
                flags & STOP_FOREGROUND_REMOVE != 0,
                flags & STOP_FOREGROUND_DETACH != 0,
            );
            self.foreground_effects_locked(st);
            return Ok(None);
        }
        let Some(notification) = notification else {
            atl_diag::refused(AM, "setServiceForeground: null notification");
            return Err(super::illegal_argument("null notification"));
        };
        // FOREGROUND_SERVICE is enforced from targetSdk P (AS :2197-2198), against the service's app
        if target_sdk >= SDK_P && !self.holds(uid, FOREGROUND_SERVICE_PERMISSION) {
            atl_diag::refused(
                AM,
                "setServiceForeground: FOREGROUND_SERVICE permission denied",
            );
            return Err(security(&format!(
                "Permission Denial: startForeground from uid {uid} requires {FOREGROUND_SERVICE_PERMISSION}"
            )));
        }
        // the requested type is the manifest's, or a subset of it (AS :2204-2229; debug.skip_fgs_manifest_type_check unset)
        let manifest = st
            .services
            .info_of(token)
            .map_or(TYPE_NONE, |i| i.foreground_service_type);
        let ty = if requested == TYPE_MANIFEST {
            manifest
        } else {
            requested
        };
        if ty & !manifest != 0 {
            atl_diag::refused(
                AM,
                "setServiceForeground: foregroundServiceType is not a subset of the manifest's",
            );
            return Err(super::illegal_argument(&format!(
                "foregroundServiceType 0x{ty:08X} is not a subset of foregroundServiceType attribute 0x{manifest:08X} in service element of manifest file"
            )));
        }
        let pid = st.services.host_of(token).unwrap_or(-1);
        // the type policy (ForegroundServiceTypePolicy: NONE is disabled from targetSdk 34; AS validateForegroundServiceType :2883-2890)
        if ty == TYPE_NONE && target_sdk >= SDK_UPSIDE_DOWN_CAKE {
            return Err(if requested == TYPE_MANIFEST {
                atl_diag::refused(
                    AM,
                    "setServiceForeground: MissingForegroundServiceTypeException",
                );
                super::illegal_state(&format!(
                    "Starting FGS without a type  callerApp=pid {pid} targetSDK={target_sdk}"
                ))
            } else {
                atl_diag::refused(
                    AM,
                    "setServiceForeground: InvalidForegroundServiceTypeException",
                );
                super::illegal_state(&format!(
                    "Starting FGS with type none callerApp=pid {pid} targetSDK={target_sdk} has been prohibited"
                ))
            });
        }
        if ty & TYPE_DATA_SYNC != 0 {
            atl_diag::answered(AM, "fgs type timeout not modelled: dataSync");
            if target_sdk >= SDK_UPSIDE_DOWN_CAKE
                && !self.holds(uid, FOREGROUND_SERVICE_DATA_SYNC_PERMISSION)
            {
                atl_diag::refused(
                    AM,
                    "setServiceForeground: FOREGROUND_SERVICE_DATA_SYNC permission denied",
                );
                return Err(security(&format!(
                    "Permission Denial: starting FGS with type dataSync requires {FOREGROUND_SERVICE_DATA_SYNC_PERMISSION}"
                )));
            }
        }
        if ty & !TYPE_DATA_SYNC != 0 {
            atl_diag::answered(AM, "fgs type permission not modelled: other types");
        }
        if st.uids.is_idle(uid) {
            atl_diag::answered(
                AM,
                "setServiceForeground: background restriction not modelled (AS :2283)",
            );
        }
        let Some(set) = st.services.set_foreground(token, Foreground { id, ty }) else {
            return Ok(None);
        };
        if set.first {
            st.uids.on_fgs_started(uid);
        }
        if let Some(old) = set.cancel {
            self.notices.cancel(&component.0, old);
        }
        atl_diag::answered(AM, "setServiceForeground: foreground (notification posted)");
        // setServiceForegroundInnerLocked :2626: the notification is the foreground one (fixNotification keeps the flag)
        let mut notification = notification.clone();
        notification.flags |= FLAG_FOREGROUND_SERVICE;
        Ok(Some(Posting {
            uid,
            package: component.0,
            id,
            notification,
        }))
    }

    /// getForegroundServiceTypeLocked (AS :1864-1876): the type of the foreground service `class` and `token` name; NONE when it is not foreground or not named.
    pub(crate) fn foreground_service_type_locked(
        &self,
        st: &State,
        class: Option<&ComponentName>,
        token: Option<&SpIBinder>,
    ) -> i32 {
        let (Some(class), Some(token)) = (class, token) else {
            return TYPE_NONE;
        };
        let named = st
            .services
            .component_of(token)
            .is_some_and(|c| c.0 == class.package && c.1 == class.class);
        if named {
            st.services.foreground_type(token)
        } else {
            TYPE_NONE
        }
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
