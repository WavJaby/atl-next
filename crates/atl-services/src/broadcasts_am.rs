// SPDX-License-Identifier: MIT
//! The broadcast half of the activity service (child module of activity.rs): BroadcastController's send path and BroadcastQueueImpl's wiring. Every broadcast is one record of the
//! dispatcher (broadcast_dispatch.rs); this module builds its receiver list, runs the dispatch-time policy (BroadcastSkipPolicy: start mode, disallowBackgroundStart), performs the
//! dispatcher's actions (sends through broadcast_queue.rs, cold starts through start_headless_locked, timers through am_handler, resultTo) and feeds it the process and binder events.
//! The rules are broadcast.rs's. A refusal or an unmodelled branch counts `atl_diag::answered(AM, <AOSP message up to its ':'>)`.

use std::collections::VecDeque;

use super::*;

use crate::broadcast::BootAudience;
use crate::broadcast_dispatch::{
    self as bd, Action, BroadcastId, Component, Dispatcher, Facts, Finish, NewBroadcast, Outcome,
    Policy, ProcKey, Receiver, Target,
};
use crate::install;
use crate::uid_state::TempGrant;
use atl_parcelables::codecs::android::content::pm::ResolveInfo::Resolved;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_services_types::hw_timeouts::PENDING_COLD_START_ABANDON_BASE_MS;

/// What each receiver of a broadcast gets besides its result: the intent, the user, and whether it is a registration's initial sticky (BroadcastRecord.initialSticky).
#[derive(Clone)]
pub(super) struct Payload {
    pub(super) intent: Intent,
    pub(super) user: i32,
    pub(super) initial_sticky: bool,
    /// the broadcast's BroadcastOptions temp allowlist, granted to each receiver's uid as its delivery is scheduled (BroadcastQueueImpl :1178-1190); None for every other send
    pub(super) grant: Option<TempGrant>,
}

/// The activity service's dispatcher: every broadcast not yet retired and every process queue (broadcast_dispatch.rs).
pub(super) type Dispatch = Dispatcher<ReceiverHandle, Bundle, Payload>;
type DispatchAction = Action<ReceiverHandle, Bundle, Payload>;
type Recv = Receiver<ReceiverHandle>;
/// Activity.RESULT_OK (-1): the initial result of a send (ContextImpl.sendBroadcast).
const BROADCAST_RESULT_OK: i32 = -1;

/// The services' own resultTo continuations (AOSP's in-process IIntentReceiver.Stub, UserController :712-730 and :943-960): run by [`Inner::system_step`] on the broadcast's final result.
/// Each carries the boot send's audience (D2): the opted-in packages at atld start, or one package's first start, whose held launch BootDone releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SystemStep {
    /// LOCKED_BOOT_COMPLETED is done: BOOT_COMPLETED goes next.
    LockedBootDone(BootAudience),
    /// BOOT_COMPLETED is done (AOSP sets mBootCompleted here).
    BootDone(BootAudience),
}

/// The process queue of the services' own resultTo. The services are the process, so no app thread attaches; the queue exists so the dispatcher hands the result back in process.
fn system_proc() -> ProcKey {
    ProcKey {
        name: "@system".to_owned(),
        uid: SYSTEM_UID,
    }
}

/// android.permission.RECEIVE_BOOT_COMPLETED: the boot broadcasts' required permission (UserController :727, :964).
const RECEIVE_BOOT_COMPLETED: &str = "android.permission.RECEIVE_BOOT_COMPLETED";

/// This process's pid, the sender pid of the services' own sends.
fn own_pid() -> i32 {
    i32::try_from(std::process::id()).unwrap_or(i32::MAX)
}

/// One send as the binder side makes it (BroadcastController.broadcastIntentLocked after verifyBroadcastLocked): the sender's (pid, uid), its user, the permissions it required (None =
/// null, not an empty list), its result (code, data, extras), whether it is sticky and ordered, and the process a result goes back to (resultTo).
pub(super) struct SendSpec<'a> {
    pub(super) intent: &'a Intent,
    pub(super) resolved_type: Option<&'a str>,
    pub(super) sender: (i32, u32),
    pub(super) user: i32,
    pub(super) required: Option<&'a [String]>,
    pub(super) result: (i32, Option<String>, Option<Bundle>),
    pub(super) sticky: bool,
    pub(super) ordered: bool,
    /// the resultTo handle and the pid of the process that holds it
    pub(super) result_to: Option<(ReceiverHandle, i32)>,
    /// BroadcastOptions.maxManifestReceiverApiLevel (None = no limit): CONNECTIVITY_ACTION's M (connectivity.rs)
    pub(super) max_manifest_api: Option<i32>,
    /// the BroadcastOptions temp allowlist of the send (uid_state::grant_of); None = none
    pub(super) grant: Option<TempGrant>,
    /// the package the send is about and the table its receivers must see it in (receivers_locked, Packages::visible per receiver uid); None = every receiver
    pub(super) about: Option<(Arc<Packages>, String)>,
}

/// The dispatch-time skip (BroadcastSkipPolicy.shouldSkipMessage's start-mode part, :229-246): a manifest receiver whose uid is restricted (uid_state::start_mode DELAYED_RIGID) is
/// skipped unless disallowBackgroundStart clears it (explicit, INCLUDE_BACKGROUND or signature-only permissions, :347-354). Registered receivers are not restricted.
fn dispatch_skip(
    packages: &Packages,
    uids: &uid_state::Uids,
    now: u64,
    facts: &Facts,
    r: &Recv,
) -> Option<String> {
    let Target::Manifest(component) = &r.target else {
        return None;
    };
    let Some(p) = packages.by_name(&component.package) else {
        return Some(format!("Skipping delivery to {component}: package is gone"));
    };
    if let Some((scope, audience)) = &facts.boot {
        // the boot broadcasts' gate (broadcast::boot_refusal): D2's audience, and LOCKED's directBootAware
        let aware = p
            .receiver(&component.class)
            .is_some_and(|d| d.component.direct_boot_aware);
        let opted_in = packages.user_state().autostart(&component.package);
        if let Some(why) =
            broadcast::boot_refusal(*scope, aware, audience, &component.package, opted_in)
        {
            return Some(format!("{why}: {component}"));
        }
    }
    if facts
        .max_manifest_api
        .is_some_and(|max| !broadcast::manifest_target_in_range(p.app().target_sdk_version, 0, max))
    {
        return Some(format!(
            "Manifest receiver API level outside the options: {component}"
        ));
    }
    let persistent = p.app().flags & PERSISTENT_MASK == PERSISTENT_MASK;
    let mode = uids.start_mode(r.proc.uid, p.app().target_sdk_version, persistent, now);
    if mode != uid_state::StartMode::DelayedRigid {
        return None;
    }
    broadcast::disallow_background_start(facts.flags, facts.explicit, facts.signature_perms)
        .then(|| format!("Background execution not allowed: receiving to {component}"))
}

/// BroadcastController.java:1608-1643: registered receivers and manifest receivers merged by priority, a registered one first at equal priority (both lists sorted, stable).
fn merge_by_priority(mut registered: Vec<Recv>, mut manifest: Vec<Recv>) -> Vec<Recv> {
    registered.sort_by_key(|r| std::cmp::Reverse(r.priority));
    manifest.sort_by_key(|r| std::cmp::Reverse(r.priority));
    let mut out = Vec::with_capacity(registered.len() + manifest.len());
    let (mut r, mut m) = (
        registered.into_iter().peekable(),
        manifest.into_iter().peekable(),
    );
    loop {
        let take_registered = match (r.peek(), m.peek()) {
            (Some(a), Some(b)) => a.priority >= b.priority,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        let next = if take_registered { r.next() } else { m.next() };
        out.extend(next);
    }
    out
}

impl Inner {
    /// Runs `f` against the dispatcher with the dispatch-time policy over the caller's state (uids, packages, the clock).
    pub(super) fn dispatch<T>(
        &self,
        st: &mut State,
        f: impl FnOnce(&mut Dispatch, &mut Policy<'_, ReceiverHandle>) -> T,
    ) -> T {
        let now = self.now_ms();
        let packages = self.packages.snapshot();
        let State {
            dispatcher, uids, ..
        } = st;
        let uids: &uid_state::Uids = uids;
        let mut policy = |facts: &Facts, r: &Recv| dispatch_skip(&packages, uids, now, facts, r);
        f(dispatcher, &mut policy)
    }

    /// Performs the dispatcher's actions in order (a failed start feeds its own actions back in). Ends no process: a process ends only at its death, force-stop or start timeout.
    pub(super) fn perform(&self, st: &mut State, actions: Vec<DispatchAction>) {
        let mut work: VecDeque<DispatchAction> = actions.into();
        while let Some(action) = work.pop_front() {
            match action {
                Action::Send { pid, job } => {
                    let Some(thread) = Self::thread_of_pid(st, pid) else {
                        atl_diag::answered(AM, "broadcast dropped: its process is not attached");
                        continue;
                    };
                    // BroadcastQueueImpl :1178-1190: the receiver's uid gets the broadcast's temp allowlist as its delivery is scheduled, before the receiver runs
                    let uid = st.processes.iter().find(|r| r.pid == pid).map(|r| r.uid);
                    if let (Some(grant), Some(uid)) = (job.payload.grant, uid) {
                        self.grant_temp_locked(st, uid, grant);
                    }
                    if let Some(job) = self.wire_job(job) {
                        self.broadcast_queue.enqueue(thread, job);
                    }
                }
                Action::StartProcess { key, component } => {
                    match self.start_headless_locked(st, &component.package, &key.name, key.uid) {
                        Ok(true) => {
                            atl_diag::answered(
                                AM,
                                "broadcast cold start (scheduleReceiverColdLocked)",
                            );
                        }
                        Ok(false) => {}
                        Err(e) => {
                            // BroadcastQueueImpl:1037-1050: the start failed, the delivery fails
                            atl_diag::answered(
                                AM,
                                "broadcast dropped: receiver process start failed",
                            );
                            log(format_args!(
                                "Unable to launch app {} for broadcast {component}: {e}",
                                key.name
                            ));
                            let more = self.dispatch(st, |d, p| d.start_failed(&key, p));
                            work.extend(more);
                        }
                    }
                }
                Action::ResultTo {
                    handle: ReceiverHandle::System(step),
                    ..
                } => self.system_step(st, step),
                Action::ResultTo {
                    pid,
                    handle: ReceiverHandle::Binder(receiver),
                    result,
                    payload,
                } => {
                    if let Some(thread) = Self::thread_of_pid(st, pid) {
                        // scheduleResultTo (BroadcastQueueImpl:1263): a registered receiver, assumed delivered; the broadcast's own intent
                        self.broadcast_queue.enqueue(
                            thread,
                            broadcast_queue::Job::Registered {
                                receiver,
                                intent: payload.intent,
                                result_code: result.code,
                                data: result.data,
                                extras: result.extras,
                                ordered: false,
                                initial_sticky: false,
                                assume_delivered: true,
                                user: payload.user,
                            },
                        );
                    }
                }
                Action::ArmTimeout {
                    pid,
                    id,
                    index,
                    after_ms,
                } => {
                    let at = self.now_ms() + after_ms;
                    self.arm(Timer::Broadcast { pid, id, index }, at);
                }
                Action::ArmColdStart {
                    key,
                    token,
                    after_ms,
                } => {
                    let at = self.now_ms() + after_ms;
                    self.arm(Timer::ColdStart { key, token }, at);
                }
                Action::ColdStartAbandoned { key } => {
                    // the backstop: the slot is released and the delivery re-enqueued or failed. The process is not ended here; an unattached one ends at its start timeout
                    atl_diag::answered(AM, "cold start abandoned: slot released");
                    log(format_args!(
                        "cold start of {} abandoned: not attached within {} ms at={}",
                        key.name,
                        self.hw.scale(PENDING_COLD_START_ABANDON_BASE_MS),
                        atl_diag::wall_stamp()
                    ));
                }
                Action::Skip { id, index, reason } => {
                    atl_diag::answered(AM, reason.split(':').next().unwrap_or("skipped"));
                    log(format_args!("broadcast {id}/{index} skipped: {reason}"));
                }
                Action::Retired { .. } => {}
            }
        }
    }

    /// The delivery of a dispatcher job as the sender thread sends it (scheduleReceiver / scheduleRegisteredReceiver). A manifest receiver whose component is gone is dropped here
    /// (counted); its delivery then ends by timeout.
    fn wire_job(
        &self,
        job: bd::Job<ReceiverHandle, Bundle, Payload>,
    ) -> Option<broadcast_queue::Job> {
        let packages = self.packages.snapshot();
        let (result_code, data, extras) = (job.result.code, job.result.data, job.result.extras);
        match job.receiver {
            Target::Registered(ReceiverHandle::Binder(receiver)) => {
                Some(broadcast_queue::Job::Registered {
                    receiver,
                    intent: job.payload.intent,
                    result_code,
                    data,
                    extras,
                    ordered: job.ordered,
                    initial_sticky: job.payload.initial_sticky,
                    assume_delivered: job.assume_delivered,
                    user: job.payload.user,
                })
            }
            Target::Registered(ReceiverHandle::System(_)) => {
                // the dispatcher's targets are receivers; the in-process resultTo is never one
                atl_diag::answered(AM, "broadcast dropped: in-process handle as a receiver");
                None
            }
            Target::Manifest(component) => {
                let Some(p) = packages.by_name(&component.package) else {
                    atl_diag::answered(AM, "broadcast dropped: receiver package is gone");
                    return None;
                };
                let Some(decl) = p.receiver(&component.class) else {
                    atl_diag::answered(AM, "broadcast dropped: receiver component is gone");
                    return None;
                };
                // the stored receiver for STOCK_PM_FLAGS (ComputerEngine.queryIntentReceivers' ResolveInfo.activityInfo)
                let info = package::activity_info(p, &packages, decl, package::STOCK_PM_FLAGS);
                // the receiver's Intent carries its component (BroadcastRecord.getReceiverIntent: setComponent for a ResolveInfo)
                let mut intent = job.payload.intent;
                intent.component = Some(ComponentName {
                    package: p.name().to_owned(),
                    class: component.class,
                });
                Some(broadcast_queue::Job::Manifest {
                    intent,
                    info: Box::new(info),
                    result_code,
                    data,
                    extras,
                    ordered: job.ordered,
                    user: job.payload.user,
                })
            }
        }
    }

    /// The attached thread of process `pid` (the dispatcher's sends and resultTo go to it).
    fn thread_of_pid(st: &State, pid: i32) -> Option<Strong<dyn IApplicationThread>> {
        st.processes
            .iter()
            .find(|r| r.pid == pid)
            .and_then(|r| r.attached.as_ref().map(|a| a.thread.clone()))
    }

    /// The dispatcher's queue key of process `pid`: its process name and uid.
    fn process_key_of(st: &State, pid: i32) -> Option<ProcKey> {
        st.processes.iter().find(|r| r.pid == pid).map(|r| ProcKey {
            name: r.process_name.clone(),
            uid: r.uid,
        })
    }

    /// A manifest receiver of package `p` (its declaration `decl`): the permission rules (BroadcastSkipPolicy :133-146, :308-321), then the receiver of its process. The priority is the
    /// filter's for an implicit receiver, 0 for an explicit one.
    fn manifest_receiver(
        &self,
        sender_uid: u32,
        required: &[String],
        p: &PackageRecord,
        decl: &ActivityInfo,
        class: &str,
        priority: i32,
    ) -> Option<Recv> {
        let component = Component {
            package: p.name().to_owned(),
            class: class.to_owned(),
        };
        let target = broadcast::ManifestReceiver {
            component: &component.to_string(),
            permission: decl.permission.as_deref(),
            exported: decl.component.exported,
            uid: p.uid,
        };
        if let Some(why) = broadcast::skip_manifest(
            &target,
            sender_uid,
            required,
            |perm| self.holds(sender_uid, perm),
            |perm| self.holds(p.uid, perm),
        ) {
            atl_diag::answered(
                AM,
                "broadcast skipped: Permission Denial (manifest receiver)",
            );
            log(format_args!("{why}"));
            return None;
        }
        let process = decl
            .component
            .process_name
            .as_deref()
            .unwrap_or(p.process_name())
            .to_owned();
        Some(Receiver {
            target: Target::Manifest(component),
            priority,
            proc: ProcKey {
                name: process,
                uid: p.uid,
            },
        })
    }

    /// The manifest receiver of an explicit component (broadcastIntentLocked's collectReceiverComponents for a component, :1966-1982).
    fn explicit_receiver(
        &self,
        package: &str,
        class: &str,
        sender_uid: u32,
        required: &[String],
    ) -> Option<Recv> {
        let packages = self.packages.snapshot();
        let Some(p) = packages.by_name(package) else {
            atl_diag::answered(
                AM,
                "broadcast to a component of an unknown package: no receiver",
            );
            return None;
        };
        let Some(decl) = p
            .receiver(class)
            .filter(|r| packages.enabled(&r.component, 0))
        else {
            atl_diag::answered(
                AM,
                "broadcast to a component that is no enabled <receiver>: no receiver",
            );
            return None;
        };
        self.manifest_receiver(sender_uid, required, p, decl, class, 0)
    }

    /// The manifest receivers an implicit intent matches (ComputerEngine.queryIntentReceivers, the caller's visible packages), each by its filter's priority.
    fn implicit_receivers(
        &self,
        sender_uid: u32,
        intent: &Intent,
        resolved_type: Option<&str>,
        required: &[String],
    ) -> Vec<Recv> {
        let mut out = Vec::new();
        let packages = self.packages.snapshot();
        for info in package::query_intent_receivers(
            &packages,
            sender_uid,
            intent,
            resolved_type,
            package::STOCK_PM_FLAGS,
            USER_SYSTEM,
        ) {
            let Resolved::Activity(decl) = &info.info else {
                continue;
            };
            let (Some(package), Some(class)) = (
                decl.component.item.package_name.as_deref(),
                decl.component.item.name.as_deref(),
            ) else {
                continue;
            };
            let Some(p) = packages.by_name(package) else {
                continue;
            };
            out.extend(self.manifest_receiver(sender_uid, required, p, decl, class, info.priority));
        }
        out
    }

    /// The receivers of a send in delivery-list order: an explicit component's manifest receiver, else the implicit manifest receivers and the registered ones (not for
    /// FLAG_RECEIVER_REGISTERED_ONLY, which keeps only the registered), merged by priority. Registered receivers of a process that is not attached are not receivers here.
    /// With `about`, a receiver uid that may not see the package in that table is dropped, counted `broadcast skipped: package not visible`.
    pub(super) fn receivers_locked(
        &self,
        st: &State,
        spec: &SendSpec<'_>,
        intent: &Intent,
        key: &crate::broadcast::IntentKey,
    ) -> Vec<Recv> {
        let (sender_uid, required) = (spec.sender.1, spec.required.unwrap_or(&[]));
        let mut manifest = Vec::new();
        if intent.flags & broadcast::FLAG_RECEIVER_REGISTERED_ONLY == 0 {
            match &intent.component {
                Some(c) => manifest
                    .extend(self.explicit_receiver(&c.package, &c.class, sender_uid, required)),
                None => manifest.extend(self.implicit_receivers(
                    sender_uid,
                    intent,
                    spec.resolved_type,
                    required,
                )),
            }
        }
        // BroadcastController.java:1578-1603: a package's own added / restarted / cleared broadcast does not reach its manifest receivers
        let ssp = intent
            .data
            .as_deref()
            .and_then(|d| d.split_once(':'))
            .map(|(_, ssp)| ssp);
        manifest.retain(|r| match &r.target {
            Target::Manifest(c) => {
                let own = broadcast::skips_own_package(intent.action.as_deref(), ssp, &c.package);
                if own {
                    atl_diag::answered(AM, "broadcast skipped: package's own receiver");
                }
                !own
            }
            Target::Registered(_) => true,
        });
        let mut registered = Vec::new();
        if intent.component.is_none() {
            for (receiver, rpid, ruid, bf) in st.broadcasts.receivers(key, spec.user) {
                if let Some(why) = broadcast::skip_registered(
                    &bf,
                    sender_uid,
                    ruid,
                    required,
                    |p| self.holds(sender_uid, p),
                    |p| self.holds(ruid, p),
                ) {
                    atl_diag::answered(AM, why.split(':').next().unwrap_or("skipped"));
                    log(format_args!("{why}"));
                    continue;
                }
                let Some(proc) = Self::process_key_of(st, rpid) else {
                    atl_diag::answered(
                        AM,
                        "broadcast skipped: registered receiver of no attached process",
                    );
                    continue;
                };
                registered.push(Receiver {
                    target: Target::Registered(receiver),
                    priority: bf.priority,
                    proc,
                });
            }
        }
        let receivers = merge_by_priority(registered, manifest);
        // BroadcastHelper's visibility allow list (IPH :2447-2450): Packages::visible per receiver uid in the send's table
        let Some((table, package)) = &spec.about else {
            return receivers;
        };
        receivers
            .into_iter()
            .filter(|r| {
                let seen = table.visible(r.proc.uid, package).is_some();
                if !seen {
                    atl_diag::answered(AM, "broadcast skipped: package not visible");
                }
                seen
            })
            .collect()
    }

    /// The send path (BroadcastController.broadcastIntentLocked after verifyBroadcastLocked): a sticky is checked and stored (:1415-1495), then the receivers (receivers_locked) become
    /// one dispatcher broadcast. Returns BROADCAST_SUCCESS, or the sticky's refusal code.
    pub(super) fn send_broadcast(&self, spec: SendSpec<'_>) -> binder::Result<i32> {
        let mut st = self.state();
        self.send_broadcast_locked(&mut st, spec)
    }

    /// [`Self::send_broadcast`] under the state lock: the services' own continuations send from inside a delivery (perform), where the lock is already held.
    fn send_broadcast_locked(&self, st: &mut State, spec: SendSpec<'_>) -> binder::Result<i32> {
        let (pid, uid) = spec.sender;
        if spec.sticky {
            let holds = self.holds(uid, permission::BROADCAST_STICKY);
            let required_any = spec.required.is_some_and(|r| !r.is_empty());
            match broadcast::sticky_send(
                holds,
                pid,
                uid,
                required_any,
                spec.intent.component.is_some(),
            ) {
                Ok(()) => {}
                Err(StickyRefusal::Security(msg)) => {
                    log(format_args!("{msg}"));
                    return Err(security(&msg));
                }
                Err(StickyRefusal::CantHavePermission) => {
                    log(format_args!(
                        "Can't broadcast sticky intent and enforce permissions {:?}",
                        spec.required
                    ));
                    return Ok(broadcast::BROADCAST_STICKY_CANT_HAVE_PERMISSION);
                }
            }
        }
        let key = intent_key(spec.intent, spec.resolved_type);
        // the sysconfig allow list adds INCLUDE_BACKGROUND to its actions (BroadcastController.java:1101-1107)
        let mut intent = std::borrow::Cow::Borrowed(spec.intent);
        if broadcast::include_background_for(intent.action.as_deref())
            && intent.flags & broadcast::FLAG_RECEIVER_INCLUDE_BACKGROUND == 0
        {
            intent.to_mut().flags |= broadcast::FLAG_RECEIVER_INCLUDE_BACKGROUND;
        }
        if spec.sticky {
            st.broadcasts
                .put_sticky(spec.user, key.clone(), spec.intent.clone(), uid);
        }
        let receivers = self.receivers_locked(st, &spec, &intent, &key);
        let facts = Facts {
            flags: intent.flags,
            explicit: intent.component.is_some() || intent.package.is_some(),
            signature_perms: self.permissions.is_signature_only(spec.required),
            max_manifest_api: spec.max_manifest_api,
            boot: broadcast::boot_scope(intent.action.as_deref()).map(|s| {
                let audience = match &spec.result_to {
                    Some((ReceiverHandle::System(SystemStep::LockedBootDone(a)), _))
                    | Some((ReceiverHandle::System(SystemStep::BootDone(a)), _)) => a.clone(),
                    // a boot action from anyone but the services' own send: protected (only the system sends it), so the opted-in rule stands
                    _ => BootAudience::OptedIn,
                };
                (s, audience)
            }),
        };
        let result_to = match spec.result_to {
            // the services' own resultTo: its queue is the system's whatever pid the caller names (system_proc)
            Some((ReceiverHandle::System(step), _)) => {
                Some((ReceiverHandle::System(step), system_proc()))
            }
            Some((handle, rpid)) => Self::process_key_of(st, rpid).map(|key| (handle, key)),
            None => None,
        };
        let b = NewBroadcast {
            facts,
            receivers,
            ordered: spec.ordered,
            result: Outcome {
                code: spec.result.0,
                data: spec.result.1,
                extras: spec.result.2,
            },
            result_to,
            payload: Payload {
                intent: intent.into_owned(),
                user: spec.user,
                initial_sticky: false,
                grant: spec.grant,
            },
        };
        let (_, actions) = self.dispatch(st, |d, p| d.enqueue(b, p));
        self.perform(st, actions);
        Ok(BROADCAST_SUCCESS)
    }

    /// The services' own resultTo continuations (SystemStep), run on the final result of an ordered broadcast the services sent. LOCKED's result sends BOOT_COMPLETED.
    fn system_step(&self, st: &mut State, step: SystemStep) {
        match step {
            SystemStep::LockedBootDone(audience) => {
                log(format_args!(
                    "LOCKED_BOOT_COMPLETED finished ({audience:?}): sending BOOT_COMPLETED"
                ));
                self.send_boot(
                    st,
                    broadcast::ACTION_BOOT_COMPLETED,
                    broadcast::REASON_BOOT_COMPLETED,
                    SystemStep::BootDone(audience),
                );
            }
            SystemStep::BootDone(audience) => {
                log(format_args!("BOOT_COMPLETED finished ({audience:?})"));
                if let BootAudience::FirstStart(package) = audience {
                    self.release_held_launches_locked(st, &package);
                }
            }
        }
    }

    /// D2 clause 2 at a process's finishAttachApplication: when this is `package`'s first process start of the run and it is not opted in, LOCKED_BOOT_COMPLETED goes out for it alone (then
    /// BOOT_COMPLETED, system_step) and true is returned; the launch of process `hold` (a start_seq) is held until BOOT's final result (release_held_launches_locked). Held before the send: with
    /// no receivers both sends finish inside it.
    pub(super) fn first_start_boot_locked(
        &self,
        st: &mut State,
        package: &str,
        hold: Option<i64>,
    ) -> bool {
        let packages = self.packages.snapshot();
        let opted_in = packages.user_state().autostart(package);
        if !broadcast::first_start_boot(&mut st.booted, package, opted_in) {
            return false;
        }
        st.held_launches.extend(hold);
        atl_diag::sent(AM, "boot broadcasts at first process start");
        self.send_boot(
            st,
            broadcast::ACTION_LOCKED_BOOT_COMPLETED,
            broadcast::REASON_LOCKED_BOOT_COMPLETED,
            SystemStep::LockedBootDone(BootAudience::FirstStart(package.to_owned())),
        );
        true
    }

    /// `package`'s first-start BOOT_COMPLETED finished: each of its held launches is posted due now (Timer::HeldLaunch), so it runs off this lock.
    fn release_held_launches_locked(&self, st: &mut State, package: &str) {
        let seqs: Vec<i64> = st
            .processes
            .iter()
            .filter(|r| r.package == package && st.held_launches.contains(&r.start_seq))
            .map(|r| r.start_seq)
            .collect();
        let now = self.now_ms();
        for seq in seqs {
            st.held_launches.remove(&seq);
            atl_diag::answered(AM, "launch released after first-start boot broadcasts");
            self.arm(Timer::HeldLaunch(seq), now);
        }
    }

    /// One boot broadcast as the system (UserController :712-730 and :943-960): ordered, NO_ABORT | INCLUDE_BACKGROUND | OFFLOAD, EXTRA_USER_HANDLE 0, required RECEIVE_BOOT_COMPLETED,
    /// the boot temp allowlist (FOREGROUND_SERVICE_ALLOWED, 20 s) with `reason`. Its final result goes to `result_to` in process.
    fn send_boot(&self, st: &mut State, action: &str, reason: i32, result_to: SystemStep) {
        let intent = Intent {
            action: Some(action.to_owned()),
            flags: broadcast::FLAG_RECEIVER_NO_ABORT
                | broadcast::FLAG_RECEIVER_INCLUDE_BACKGROUND
                | broadcast::FLAG_RECEIVER_OFFLOAD,
            extras: Some(Bundle::from_entries([(
                broadcast::EXTRA_USER_HANDLE.to_owned(),
                Value::Int(USER_SYSTEM),
            )])),
            ..Intent::default()
        };
        let required = [RECEIVE_BOOT_COMPLETED.to_owned()];
        let spec = SendSpec {
            intent: &intent,
            resolved_type: None,
            sender: (own_pid(), SYSTEM_UID),
            user: USER_SYSTEM,
            required: Some(&required),
            result: (BROADCAST_RESULT_OK, None, None),
            sticky: false,
            ordered: true,
            result_to: Some((ReceiverHandle::System(result_to), own_pid())),
            max_manifest_api: None,
            about: None,
            grant: uid_state::grant_of(
                broadcast::BOOT_TIME_TEMP_ALLOWLIST_MS,
                uid_state::TEMP_TYPE_FGS_ALLOWED,
                reason,
            ),
        };
        if let Err(e) = self.send_broadcast_locked(st, spec) {
            log(format_args!("boot broadcast {action}: {e}"));
        }
    }

    /// The registration's initial sticky (registerReceiverWithFeatureTraced:582-604): an unordered broadcast of the stored sticky to this one receiver, flagged initial.
    pub(super) fn sticky_replay_locked(
        &self,
        st: &mut State,
        pid: i32,
        receiver: ReceiverHandle,
        priority: i32,
        intent: Intent,
    ) {
        let Some(proc) = Self::process_key_of(st, pid) else {
            atl_diag::answered(
                AM,
                "registerReceiver: sticky not delivered (no attached process)",
            );
            return;
        };
        let b = NewBroadcast {
            facts: Facts {
                flags: intent.flags,
                ..Facts::default()
            },
            receivers: vec![Receiver {
                target: Target::Registered(receiver),
                priority,
                proc,
            }],
            ordered: false,
            // the result code the replay has always carried (0); AOSP's replay BroadcastRecord code is not checked against source yet
            result: Outcome {
                code: 0,
                data: None,
                extras: None,
            },
            result_to: None,
            payload: Payload {
                intent,
                user: broadcast::USER_ALL,
                initial_sticky: true,
                grant: None,
            },
        };
        let (_, actions) = self.dispatch(st, |d, p| d.enqueue(b, p));
        self.perform(st, actions);
    }

    /// The delivery timer of (`pid`, `id`, `index`) fired (BroadcastQueueImpl.broadcastTimeoutLocked). A timer whose delivery ended does nothing. The ANR is not modelled: the
    /// delivery ends as TIMEOUT, a headless process gets AOSP's silent background-ANR kill (ProcessErrorStateRecord :672-673, plan P2), a windowed one is only counted.
    pub(super) fn broadcast_timeout(&self, pid: i32, id: BroadcastId, index: usize) {
        let headless = {
            let mut st = self.state();
            if !st.dispatcher.delivering(pid, id, index) {
                return;
            }
            atl_diag::answered(AM, "broadcast timeout: ANR not modelled");
            let headless = st
                .processes
                .iter()
                .find(|r| r.pid == pid)
                .is_some_and(|r| r.task.is_none());
            let actions = self.dispatch(&mut st, |d, p| d.timeout(pid, id, index, p));
            self.perform(&mut st, actions);
            headless
        };
        if headless {
            atl_diag::answered(
                AM,
                "broadcast timeout: headless process ended (silent bg ANR)",
            );
            self.kill(&[pid]);
        }
    }

    /// The cold-start backstop timer fired (its ColdStartAbandoned action, performed by [`Self::perform`]); a stale token does nothing.
    pub(super) fn cold_start_timeout(&self, key: &ProcKey, token: u64) {
        let mut st = self.state();
        let actions = self.dispatch(&mut st, |d, p| d.cold_start_timeout(key, token, p));
        self.perform(&mut st, actions);
    }

    /// finishReceiver (IActivityManager.aidl:197, oneway): `who` is the app thread's binder, which names the process (LoadedApk :1755, BroadcastReceiver :326). The result and abort go to
    /// the dispatcher; the process's hold ends with its delivery.
    pub(super) fn finish_receiver(
        &self,
        who: Option<&SpIBinder>,
        result: Outcome<Bundle>,
        abort: bool,
    ) {
        let mut st = self.state();
        let pid = who.and_then(|who| {
            st.processes
                .iter()
                .find(|r| {
                    r.attached
                        .as_ref()
                        .is_some_and(|a| a.thread.as_binder() == *who)
                })
                .map(|r| r.pid)
        });
        match pid {
            Some(pid) => {
                let actions =
                    self.dispatch(&mut st, |d, p| d.finish(pid, Finish { result, abort }, p));
                self.perform(&mut st, actions);
            }
            None => {
                atl_diag::answered(AM, "finishReceiver: no attached process of that binder");
            }
        }
        self.touch_all_locked(&mut st);
        drop(st);
    }

    /// onApplicationAttachedLocked (finishAttachApplicationInner): the process's queue resumes with its pid.
    pub(super) fn process_attached_locked(&self, st: &mut State, pid: i32) {
        let Some(key) = Self::process_key_of(st, pid) else {
            return;
        };
        let actions = self.dispatch(st, |d, p| d.process_attached(&key, pid, p));
        self.perform(st, actions);
    }

    /// BroadcastQueueImpl.onApplicationCleanupLocked (:721-756) and the ReceiverList cleanup of a dead process: its registrations go, its active delivery (cold-starting or in-flight) fails (releasing the cold slot), and waiting manifest deliveries wait for a new process.
    pub(super) fn broadcasts_died_locked(&self, st: &mut State, rec: &ProcessRecord) {
        st.broadcasts.process_died(rec.pid);
        let key = ProcKey {
            name: rec.process_name.clone(),
            uid: rec.uid,
        };
        let actions = self.dispatch(st, |d, p| d.process_died(&key, rec.pid, p));
        self.perform(st, actions);
    }

    /// cleanupDisabledPackageReceiversLocked (force-stop): the package's waiting manifest deliveries are skipped (a crash keeps them, as AOSP does).
    pub(super) fn drop_package_locked(&self, st: &mut State, package: &str) {
        let actions = self.dispatch(st, |d, p| d.drop_package(package, p));
        self.perform(st, actions);
    }

    /// A cold start of `key` ended before attach (its record was removed by a start timeout or a force-stop): its delivery fails and the slot is released.
    pub(super) fn start_failed_locked(&self, st: &mut State, key: &ProcKey) {
        let actions = self.dispatch(st, |d, p| d.start_failed(key, p));
        self.perform(st, actions);
    }
}

/// One package broadcast of a publish as the send path takes it: the intent, its audience (None: every receiver) and its temp allowlist.
pub(super) struct PackageSend {
    pub(super) package: String,
    pub(super) intent: Intent,
    pub(super) about: Option<(Arc<Packages>, String)>,
    pub(super) grant: Option<TempGrant>,
}

/// The package broadcasts of a publish, in send order. An update: PACKAGE_REMOVED of the old version (BroadcastHelper.sendPackageRemovedBroadcasts), then ADDED, REPLACED and
/// MY_PACKAGE_REPLACED (sendPostInstallBroadcasts :600-720). A new package: ADDED. Audiences as IPH :2447-2450: REMOVED and REPLACED the table before the publish, ADDED the one it published.
pub(super) fn package_sends(
    reports: &[install::Reported],
    audience: &install::Audience,
) -> Vec<PackageSend> {
    let before = |r: &install::Reported| Some((Arc::clone(&audience.before), r.package.clone()));
    let after = |r: &install::Reported| Some((Arc::clone(&audience.after), r.package.clone()));
    let mut sends = Vec::new();
    for r in reports {
        match r.change {
            install::Change::New => sends.push(data_send(
                broadcast::ACTION_PACKAGE_ADDED,
                r,
                Vec::new(),
                after(r),
            )),
            install::Change::Updated { .. } => {
                sends.push(removed_send(r, before(r)));
                let replacing = || vec![(broadcast::EXTRA_REPLACING, Value::Bool(true))];
                sends.push(data_send(
                    broadcast::ACTION_PACKAGE_ADDED,
                    r,
                    replacing(),
                    after(r),
                ));
                sends.push(data_send(
                    broadcast::ACTION_PACKAGE_REPLACED,
                    r,
                    replacing(),
                    before(r),
                ));
                sends.push(my_package_replaced(r));
            }
            install::Change::Unchanged => {}
        }
    }
    sends
}

/// PACKAGE_REMOVED of an update's replace (BroadcastHelper.sendPackageRemovedBroadcasts :1082-1095, from the replace call :593-595): killApp (DONT_KILL_APP false), not
/// removed by the system (USER_INITIATED true), REMOVED_FOR_ALL_USERS (the replace's delete takes the package out of mPackages: IPH :2484-2485), REPLACING. No temp allowlist.
fn removed_send(r: &install::Reported, about: Option<(Arc<Packages>, String)>) -> PackageSend {
    data_send(
        broadcast::ACTION_PACKAGE_REMOVED,
        r,
        vec![
            (broadcast::EXTRA_DATA_REMOVED, Value::Bool(false)),
            (broadcast::EXTRA_DONT_KILL_APP, Value::Bool(false)),
            (broadcast::EXTRA_REMOVED_FOR_ALL_USERS, Value::Bool(true)),
            (broadcast::EXTRA_REPLACING, Value::Bool(true)),
            (broadcast::EXTRA_SYSTEM_UPDATE_UNINSTALL, Value::Bool(false)),
            (broadcast::EXTRA_USER_INITIATED, Value::Bool(true)),
        ],
        about,
    )
}

/// One data broadcast (sendPackageBroadcast): data `package:<pkg>` and EXTRA_UID, then `extras`, to the receivers `about` admits.
fn data_send(
    action: &str,
    r: &install::Reported,
    extras: Vec<(&str, Value)>,
    about: Option<(Arc<Packages>, String)>,
) -> PackageSend {
    let mut entries = vec![(
        broadcast::EXTRA_UID.to_owned(),
        Value::Int(i32::try_from(r.uid).unwrap_or(i32::MAX)),
    )];
    entries.extend(extras.into_iter().map(|(k, v)| (k.to_owned(), v)));
    PackageSend {
        package: r.package.clone(),
        intent: Intent {
            action: Some(action.to_owned()),
            data: Some(format!("package:{}", r.package)),
            extras: Some(Bundle::from_entries(entries)),
            ..Intent::default()
        },
        about,
        grant: None,
    }
}

/// MY_PACKAGE_REPLACED to the package's own receivers alone (setPackage, no data, no extras) with the REASON_PACKAGE_REPLACED temp allowlist; no audience (the allow list is null, BroadcastHelper :714-720).
fn my_package_replaced(r: &install::Reported) -> PackageSend {
    PackageSend {
        package: r.package.clone(),
        intent: Intent {
            action: Some(broadcast::ACTION_MY_PACKAGE_REPLACED.to_owned()),
            package: Some(r.package.clone()),
            ..Intent::default()
        },
        about: None,
        grant: uid_state::grant_of(
            broadcast::BOOT_TIME_TEMP_ALLOWLIST_MS,
            uid_state::TEMP_TYPE_FGS_ALLOWED,
            broadcast::REASON_PACKAGE_REPLACED,
        ),
    }
}

impl ActivityManager {
    /// A broadcast the services send as the system uid to the registered receivers, of `only_uid` alone when given (see [`Self::broadcast_registered`]'s callers). Returns the receivers queued.
    pub fn broadcast_registered(&self, intent: &Intent, only_uid: Option<u32>) -> usize {
        let inner = &self.inner;
        let user = only_uid.map_or(USER_SYSTEM, |uid| {
            i32::try_from(uid / permission::PER_USER_RANGE).unwrap_or(USER_SYSTEM)
        });
        let key = intent_key(intent, None);
        let mut st = inner.state();
        let mut receivers = Vec::new();
        for (receiver, rpid, ruid, bf) in st.broadcasts.receivers(&key, user) {
            if only_uid.is_some_and(|uid| ruid != uid) {
                continue;
            }
            if let Some(why) = broadcast::skip_registered(
                &bf,
                SYSTEM_UID,
                ruid,
                &[],
                |p| inner.holds(SYSTEM_UID, p),
                |p| inner.holds(ruid, p),
            ) {
                atl_diag::answered(AM, why.split(':').next().unwrap_or("skipped"));
                continue;
            }
            let Some(proc) = Inner::process_key_of(&st, rpid) else {
                continue;
            };
            receivers.push(Receiver {
                target: Target::Registered(receiver),
                priority: bf.priority,
                proc,
            });
        }
        let queued = receivers.len();
        if queued == 0 {
            return 0;
        }
        let b = NewBroadcast {
            facts: Facts {
                flags: intent.flags,
                ..Facts::default()
            },
            receivers,
            ordered: false,
            result: Outcome {
                code: BROADCAST_RESULT_OK,
                data: None,
                extras: None,
            },
            result_to: None,
            payload: Payload {
                intent: intent.clone(),
                user,
                initial_sticky: false,
                grant: None,
            },
        };
        let (_, actions) = inner.dispatch(&mut st, |d, p| d.enqueue(b, p));
        inner.perform(&mut st, actions);
        drop(st);
        queued
    }

    /// IIntentSender.send of a BROADCAST record (PendingIntentRecord.sendInner): the request intent to `user_id` as the creator `uid`, through the dispatcher like any other send.
    /// `finished` = the sender's finishedReceiver: ordered, its final result to the calling process `caller_pid` (PendingIntentRecord :674-677). The sender pid is this process.
    pub(super) fn send_pending_broadcast(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        finished: Option<(&Strong<dyn IIntentReceiver>, i32)>,
        grant: Option<TempGrant>,
    ) -> binder::Result<()> {
        const RESULT_OK: i32 = -1;
        let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        self.inner
            .send_broadcast(SendSpec {
                intent,
                resolved_type: None,
                sender: (pid, uid),
                user: user_id,
                required: None,
                result: (RESULT_OK, None, None),
                sticky: false,
                ordered: finished.is_some(),
                result_to: finished.map(|(h, caller)| (ReceiverHandle::Binder(h.clone()), caller)),
                max_manifest_api: None,
                grant,
                about: None,
            })
            .map(|_| ())
    }

    /// The start-time package broadcasts of the install's reports, before the boot ones (`package_sends`): a new package gets PACKAGE_ADDED; an updated one PACKAGE_REMOVED of the old
    /// version first, then PACKAGE_ADDED (EXTRA_REPLACING), PACKAGE_REPLACED, and MY_PACKAGE_REPLACED to its own receivers alone with the REASON_PACKAGE_REPLACED temp allowlist.
    /// Each receiver must see the package in its send's audience (`receivers_locked`). Unchanged packages send nothing; a refused downgrade comes as Unchanged (install.rs install keeps the installed package).
    pub fn package_broadcasts(&self, reports: &[install::Reported], audience: &install::Audience) {
        for send in package_sends(reports, audience) {
            let spec = SendSpec {
                intent: &send.intent,
                resolved_type: None,
                sender: (own_pid(), SYSTEM_UID),
                user: USER_SYSTEM,
                required: None,
                result: (BROADCAST_RESULT_OK, None, None),
                sticky: false,
                ordered: false,
                result_to: None,
                max_manifest_api: None,
                grant: send.grant,
                about: send.about,
            };
            if let Err(e) = self.inner.send_broadcast(spec) {
                let action = send.intent.action.as_deref().unwrap_or_default();
                log(format_args!(
                    "package broadcast {action} {}: {e}",
                    send.package
                ));
            }
        }
    }

    /// The boot at services start (UserController's chain, :712-730 then :943-960): the system queue is attached so the in-process resultTo is delivered, then LOCKED_BOOT_COMPLETED
    /// goes out ordered; its final result sends BOOT_COMPLETED (system_step). Both reach only the opted-in packages (D2, dispatch_skip); the others get theirs at their first process start
    /// (first_start_boot_locked).
    pub fn boot_completed(&self) {
        let mut st = self.inner.state();
        let actions = self.inner.dispatch(&mut st, |d, p| {
            d.process_attached(&system_proc(), own_pid(), p)
        });
        self.inner.perform(&mut st, actions);
        self.inner.send_boot(
            &mut st,
            broadcast::ACTION_LOCKED_BOOT_COMPLETED,
            broadcast::REASON_LOCKED_BOOT_COMPLETED,
            SystemStep::LockedBootDone(BootAudience::OptedIn),
        );
    }

    /// A broadcast the services send as the system to every user (ActivityManager.broadcastStickyIntent(intent, USER_ALL) for `sticky`, Context.sendBroadcastAsUser(intent,
    /// UserHandle.ALL) otherwise): system_server's own call clears its calling identity, so the sender is SYSTEM_UID in this process; result code Activity.RESULT_OK (-1).
    pub fn broadcast_as_system(&self, intent: &Intent, sticky: bool) {
        self.broadcast_as_system_max_api(intent, sticky, None);
    }

    /// [`Self::broadcast_as_system`] with BroadcastOptions.maxManifestReceiverApiLevel `max_manifest_api` (None = no limit): CONNECTIVITY_ACTION's M.
    pub fn broadcast_as_system_max_api(
        &self,
        intent: &Intent,
        sticky: bool,
        max_manifest_api: Option<i32>,
    ) {
        const RESULT_OK: i32 = -1;
        let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        if let Err(e) = self.inner.send_broadcast(SendSpec {
            intent,
            resolved_type: None,
            sender: (pid, SYSTEM_UID),
            user: broadcast::USER_ALL,
            required: None,
            result: (RESULT_OK, None, None),
            sticky,
            ordered: false,
            result_to: None,
            max_manifest_api,
            grant: None,
            about: None,
        }) {
            log(format_args!("system broadcast {:?}: {e}", intent.action));
        }
    }

    /// broadcastIntentWithFeature's body for its binder caller `caller` = (pid, uid) whose attached process is `app` = (package, persistent) ([`Inner::caller_app`]): verifyBroadcastLocked's
    /// Intent checks (FLAG_RECEIVER_FROM_SHELL dropped unless root or shell, :1907-1917), broadcastIntentLocked's sender check (:1031-1093, broadcast::sender_check: a protected action
    /// from a non-system caller is a SecurityException), then [`Inner::send_broadcast`] as that caller. An app never reaches the system identity of
    /// [`Self::broadcast_as_system`]. `ordered` = serialized or a resultTo; `result_to` = the caller's resultTo handle.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn broadcast_from_caller(
        &self,
        caller: (i32, u32),
        app: (Option<String>, bool),
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
        ordered: bool,
        required: Option<&[String]>,
        result: (i32, Option<String>, Option<Bundle>),
        sticky: bool,
        user_id: i32,
        result_to: Option<ReceiverHandle>,
    ) -> binder::Result<i32> {
        let Some(intent) = intent else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("broadcastIntent: null Intent"),
            ));
        };
        if intent.has_file_descriptors() {
            return Err(illegal_argument("File descriptors passed in Intent"));
        }
        if intent.flags & FLAG_RECEIVER_BOOT_UPGRADE != 0 {
            return Err(illegal_argument(
                "Can't use FLAG_RECEIVER_BOOT_UPGRADE here",
            ));
        }
        let (pid, uid) = caller;
        let mut intent = std::borrow::Cow::Borrowed(intent);
        if intent.flags & broadcast::FLAG_RECEIVER_FROM_SHELL != 0
            && ![ROOT_UID, SHELL_UID].contains(&uid)
        {
            intent.to_mut().flags &= !broadcast::FLAG_RECEIVER_FROM_SHELL;
        }
        let (package, persistent) = app;
        let action = intent.action.as_deref();
        let packages = self.inner.packages.snapshot();
        match broadcast::sender_check(
            action,
            packages.is_protected_broadcast(action),
            pid,
            uid,
            package.as_deref(),
            persistent,
            intent.component.as_ref().map(|c| c.package.as_str()),
        ) {
            Ok(None) => {}
            Ok(Some(own)) => intent.to_mut().package = Some(own),
            Err(msg) => {
                log(format_args!("{msg}"));
                return Err(security(&msg));
            }
        }
        self.inner.send_broadcast(SendSpec {
            intent: &intent,
            resolved_type,
            sender: (pid, uid),
            user: incoming_user(user_id),
            required,
            result,
            sticky,
            ordered,
            result_to: result_to.map(|handle| (handle, pid)),
            max_manifest_api: None,
            grant: None,
            about: None,
        })
    }
}
