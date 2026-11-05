// SPDX-License-Identifier: MIT
//! ActivityTaskManager-lite (`activity_task`): the activity lifecycle and startActivity, the two things the app's Instrumentation and ActivityThread ask of ATM.
//! [`ActivityTasks`] owns the lifecycle machine (activity_stack) and is the only place a lifecycle decision becomes a transaction: every command the machine returns is sent here,
//! as one ClientTransaction per command, under the machine lock (the sends are oneway, so none blocks). The accepted starts are checked by activity_start.
//! activity.rs keeps the processes and forwards the client controller's acks here. Converter-gated: it speaks binder.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::app::IActivityClientController::IActivityClientController;
use atl_aidl::aidl::android::app::IActivityTaskManager::{
    BnActivityTaskManager, IActivityTaskManager,
};
use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_parcelables::android::app::ProfilerInfo;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::parts::{
    DestroyActivityItem, PauseActivityItem, StartActivityItem, StopActivityItem,
};
use binder::{BinderFeatures, FromIBinder, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::activity::{global_configuration, launch_item, log, resume_item, security};
use crate::activity_stack::{Command, Machine, Step};
use crate::activity_start::{self, Call, Refusal, Source, Target};
use crate::config_diff::Config;
use crate::counters;
use crate::manifest_flags;
use crate::packages::{ActivityDecl, Packages};
use crate::tasks::{TaskId, Tasks};
use crate::token;

const ATM: &str = "IActivityTaskManager";

/// What the machine keeps per activity (ActivityRecord, reduced): its app thread, the identity its starts are checked against, and the configuration state a change is delivered by.
#[derive(Clone)]
pub(crate) struct Owner {
    pub thread: Strong<dyn IApplicationThread>,
    pub uid: u32,
    pub package: String,
    /// the affinity of the activity's task: its taskAffinity, else the package name (see [`affinity_of`])
    pub affinity: String,
    /// the activity's class name (ActivityDecl.name); the services' log names the activity by it
    pub component: String,
    /// the merged configuration the app last got (ActivityRecord.mLastReportedConfiguration); a configuration change is diffed against it (activity.rs task_resized)
    pub reported: Config,
    /// ActivityInfo.getRealConfigChanged: the CONFIG_* bits the activity handles itself
    pub declared: u32,
    /// a relaunch was sent and activityRelaunched has not come (ActivityRecord.mPendingRelaunchCount, at most 1 here: Delivery::Wait)
    pub relaunching: bool,
}

/// What a Launch step is built from: the activity's package and declaration, and the LaunchActivityItem's ident.
#[derive(Clone)]
pub(crate) struct Launch {
    pub package: String,
    pub decl: ActivityDecl,
    pub ident: i32,
}

type Lifecycle = Machine<SpIBinder, Owner, Launch>;

/// The parts of a startActivity call the lifecycle needs besides the caller's identity.
pub(crate) struct StartArgs<'a> {
    /// The intent's explicit component (package, class); None for an implicit intent.
    pub component: Option<(&'a str, &'a str)>,
    pub request_code: i32,
    pub flags: i32,
    pub result_to: Option<&'a SpIBinder>,
}

/// ActivityRecord's task affinity: the manifest's taskAffinity, else the package name.
pub(crate) fn affinity_of(decl: &ActivityDecl, package: &str) -> String {
    decl.task_affinity
        .clone()
        .unwrap_or_else(|| package.to_owned())
}

pub struct ActivityTasks {
    packages: Arc<Packages>,
    tasks: Arc<Tasks<SpIBinder>>,
    /// the ActivityClientController every LaunchActivityItem names: ActivityClient routes its calls through it
    controller: SpIBinder,
    machine: Mutex<Lifecycle>,
    /// LaunchActivityItem.ident: a stand-in for System.identityHashCode(r), unique per launch
    idents: AtomicI32,
    /// the machine's settled state as last logged; written only under the machine lock (note_settle)
    settled: AtomicBool,
}

impl ActivityTasks {
    pub(crate) fn new(
        packages: Arc<Packages>,
        tasks: Arc<Tasks<SpIBinder>>,
        controller: SpIBinder,
    ) -> Self {
        ActivityTasks {
            packages,
            tasks,
            controller,
            machine: Mutex::new(Machine::new()),
            idents: AtomicI32::new(0),
            settled: AtomicBool::new(true),
        }
    }

    /// The controller binder the services hand out (getActivityClientController and each LaunchActivityItem).
    pub(crate) fn controller(&self) -> SpIBinder {
        self.controller.clone()
    }

    fn machine(&self) -> MutexGuard<'_, Lifecycle> {
        self.machine.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_ident(&self) -> i32 {
        self.idents.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Logs each change of the machine's settled state: `activity unsettled` once a launch, start or lifecycle step is sent and not acknowledged, `activity settled` once all is. run-apk reads the last one.
    /// Called under the machine lock, before the commands are sent, so the line precedes the acks it can cause.
    fn note_settle(&self, m: &Lifecycle) {
        let now = m.settled();
        if self.settled.swap(now, Ordering::Relaxed) != now {
            let state = if now { "settled" } else { "unsettled" };
            log(format_args!(
                "activity {state} at={}",
                counters::wall_stamp()
            ));
        }
    }

    /// The transaction item of one step of `token`'s record. None: a launch of a package that is no longer installed (counted, never reached).
    fn item(&self, token: &SpIBinder, step: Step<Launch>) -> Option<ClientTransactionItem> {
        let activity_token = Some(token.clone());
        Some(match step {
            Step::Launch(launch) => {
                let Some(p) = self.packages.by_name(&launch.package) else {
                    counters::count(ATM, "launch of a package not installed");
                    return None;
                };
                ClientTransactionItem::LaunchActivity(Box::new(launch_item(
                    &self.packages,
                    &self.tasks.display(),
                    &self.controller,
                    p,
                    &launch.decl,
                    token.clone(),
                    launch.ident,
                )))
            }
            Step::Start => {
                ClientTransactionItem::StartActivity(StartActivityItem { activity_token })
            }
            Step::Resume => ClientTransactionItem::ResumeActivity(resume_item(token.clone())),
            Step::Pause {
                finished,
                user_leaving,
            } => ClientTransactionItem::PauseActivity(PauseActivityItem {
                activity_token,
                finished,
                user_leaving,
                dont_report: false,
                auto_entering_pip: false,
            }),
            Step::Stop => ClientTransactionItem::StopActivity(StopActivityItem { activity_token }),
            Step::Destroy { finished } => {
                ClientTransactionItem::DestroyActivity(DestroyActivityItem {
                    activity_token,
                    finished,
                })
            }
        })
    }

    /// One transaction per command, in the order the machine decided. Called under the machine lock.
    fn send(&self, commands: Vec<Command<SpIBinder, Owner, Launch>>) {
        for command in commands {
            let items = command
                .steps
                .into_iter()
                .filter_map(|step| self.item(&command.token, step))
                .map(Some)
                .collect();
            let transaction = ClientTransaction { items };
            if let Err(e) = command
                .owner
                .thread
                .r#scheduleTransaction(Some(&transaction))
            {
                log(format_args!("scheduleTransaction (lifecycle): {e}"));
            }
        }
    }

    /// finishAttachApplication's launcher: a new Task (no launch yet names a caller's Task), its record pushed, its launch sent. Returns the record's token.
    pub(crate) fn launch_launcher(
        &self,
        thread: Strong<dyn IApplicationThread>,
        uid: u32,
        package: &str,
        decl: &ActivityDecl,
    ) -> SpIBinder {
        let token = token::new();
        let task = self.tasks.new_task(uid);
        self.tasks.bind_activity(token.clone(), task);
        let owner = Owner {
            thread,
            uid,
            package: package.to_owned(),
            affinity: affinity_of(decl, package),
            component: decl.name.clone(),
            reported: self.launch_config(),
            declared: self
                .packages
                .by_name(package)
                .map_or(0, |p| manifest_flags::real_config_changes(p, decl)),
            relaunching: false,
        };
        let launch = Launch {
            package: package.to_owned(),
            decl: decl.clone(),
            ident: self.next_ident(),
        };
        let mut m = self.machine();
        let commands = m.push(task, token.clone(), owner, launch);
        self.note_settle(&m);
        self.send(commands);
        token
    }

    /// startActivity: validate, then the new record goes on top of the caller's activity's Task and is sent its launch. Ok: the START code (0 started, else refused).
    /// A foreign resultTo (not one of this caller's records) is a SecurityException; a call with no resultTo record is refused as CANCELED.
    pub(crate) fn start(
        &self,
        caller: &Strong<dyn IApplicationThread>,
        calling_uid: u32,
        calling_package: &str,
        args: StartArgs<'_>,
    ) -> Result<i32, Status> {
        let StartArgs {
            component,
            request_code,
            flags,
            result_to,
        } = args;
        let mut m = self.machine();
        let source = result_to.and_then(|t| {
            m.owner(t)
                .filter(|o| o.thread.as_binder() == caller.as_binder())
                .map(|o| (t.clone(), o.clone()))
        });
        let source_view = source.as_ref().map(|(_, o)| Source {
            uid: o.uid,
            package: &o.package,
            affinity: &o.affinity,
        });
        let package = component.and_then(|(p, _)| self.packages.by_name(p));
        let decl = component.and_then(|(_, class)| {
            package.and_then(|p| p.manifest.activities.iter().find(|a| a.name == class))
        });
        let call = Call {
            calling_uid,
            calling_package,
            request_code,
            flags,
            component,
            source: source_view,
        };
        let target = Target {
            process: package.map_or("", |p| p.process_name()),
            decl,
        };
        let decl = match activity_start::check(&call, target) {
            Ok(decl) => decl,
            Err(Refusal::Code { code, .. }) => return Ok(code),
            Err(Refusal::Security(msg)) => return Err(security(msg)),
        };
        let Some((source_token, _)) = source else {
            return Ok(activity_start::START_CANCELED);
        };
        let Some(task) = m.task_of(&source_token) else {
            return Ok(activity_start::START_CANCELED);
        };
        let token = token::new();
        self.tasks.bind_activity(token.clone(), task);
        let owner = Owner {
            thread: caller.clone(),
            uid: calling_uid,
            package: calling_package.to_owned(),
            affinity: affinity_of(decl, calling_package),
            component: decl.name.clone(),
            reported: self.launch_config(),
            declared: package.map_or(0, |p| manifest_flags::real_config_changes(p, decl)),
            relaunching: false,
        };
        let launch = Launch {
            package: calling_package.to_owned(),
            decl: decl.clone(),
            ident: self.next_ident(),
        };
        let commands = m.push(task, token, owner, launch);
        self.note_settle(&m);
        self.send(commands);
        Ok(activity_start::START_SUCCESS)
    }

    /// ActivityClientController.activityResumed: the resume ack, logged as `activity resumed <class> at=<stamp>` before the settled change it may cause. False when no record has the token (nothing logged).
    pub(crate) fn resumed(&self, token: &SpIBinder) -> bool {
        let mut m = self.machine();
        let Some(component) = m.owner(token).map(|o| o.component.clone()) else {
            return false;
        };
        log(format_args!(
            "activity resumed {component} at={}",
            counters::wall_stamp()
        ));
        let commands = m.resumed(token);
        self.note_settle(&m);
        self.send(commands);
        true
    }

    /// activityPaused (synchronous): the pause ack. False when no record has the token.
    pub(crate) fn paused(&self, token: &SpIBinder) -> bool {
        let mut m = self.machine();
        if m.phase(token).is_none() {
            return false;
        }
        let commands = m.paused(token);
        self.note_settle(&m);
        self.send(commands);
        true
    }

    /// activityStopped: the stop ack.
    pub(crate) fn stopped(&self, token: &SpIBinder) -> bool {
        let mut m = self.machine();
        if m.phase(token).is_none() {
            return false;
        }
        let commands = m.stopped(token);
        self.note_settle(&m);
        self.send(commands);
        true
    }

    /// finishActivity: the record is finishing; false when the token names no record.
    pub(crate) fn finish(&self, token: &SpIBinder) -> bool {
        let mut m = self.machine();
        match m.finish(token) {
            Some(commands) => {
                self.note_settle(&m);
                self.send(commands);
                true
            }
            None => false,
        }
    }

    /// The items a relaunch of `token` ends with, decided by the machine from the record's phase (Resume only for a resumed record).
    pub(crate) fn relaunch_items(&self, token: &SpIBinder) -> Vec<Option<ClientTransactionItem>> {
        let mut m = self.machine();
        let steps = m.relaunch(token);
        self.note_settle(&m);
        steps
            .into_iter()
            .filter_map(|step| self.item(token, step))
            .map(Some)
            .collect()
    }

    /// The configuration a launch carries (launch_item: the global configuration, no override): what the record's `reported` starts from.
    fn launch_config(&self) -> Config {
        Config::from(&global_configuration(&self.tasks.display()))
    }

    /// The records of `task` with their owners, bottom -> top: what a configuration change of the Task is delivered to. A destroyed record is not on the stack, so not listed.
    pub(crate) fn records(&self, task: TaskId) -> Vec<(SpIBinder, Owner)> {
        self.machine()
            .records(task)
            .map(|(k, o)| (k.clone(), o.clone()))
            .collect()
    }

    /// Changes the owner of `token`'s record; false when no record has it.
    pub(crate) fn update_owner(&self, token: &SpIBinder, f: impl FnOnce(&mut Owner)) -> bool {
        self.machine().owner_mut(token).map(f).is_some()
    }

    /// Whether a record of `task` waits for its relaunch.
    pub(crate) fn task_relaunching(&self, task: TaskId) -> bool {
        self.machine().records(task).any(|(_, o)| o.relaunching)
    }

    /// The process of `thread` died: its records leave the stack (Machine::drop_owned).
    pub(crate) fn process_died(&self, thread: &SpIBinder) {
        let mut m = self.machine();
        m.drop_owned(|o| o.thread.as_binder() == *thread);
        self.note_settle(&m);
    }
}

/// IActivityTaskManager: startActivity and getActivityClientController. Every other method keeps the stub's unimplemented answer (counted).
pub struct ActivityTaskService {
    atm: Arc<ActivityTasks>,
}

impl ActivityTaskService {
    pub fn new(atm: Arc<ActivityTasks>) -> Self {
        ActivityTaskService { atm }
    }

    pub fn new_binder(atm: Arc<ActivityTasks>) -> SpIBinder {
        BnActivityTaskManager::new_binder(ActivityTaskService::new(atm), BinderFeatures::default())
            .as_binder()
    }
}

impl Interface for ActivityTaskService {}

impl IActivityTaskManager for ActivityTaskService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(ATM, method)
    }

    fn r#startActivity(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        calling_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        intent: Option<&Intent>,
        _resolved_type: Option<&str>,
        result_to: Option<&SpIBinder>,
        _result_who: Option<&str>,
        request_code: i32,
        flags: i32,
        _profiler_info: Option<&ProfilerInfo>,
        _options: Option<&Bundle>,
    ) -> binder::Result<i32> {
        let (Some(caller), Some(calling_package)) = (caller, calling_package) else {
            return Err(security(
                "startActivity: no caller thread or calling package",
            ));
        };
        let component = intent
            .and_then(|i| i.component.as_ref())
            .map(|c| (c.package.as_str(), c.class.as_str()));
        let args = StartArgs {
            component,
            request_code,
            flags,
            result_to,
        };
        self.atm.start(
            caller,
            ThreadState::get_calling_uid(),
            calling_package,
            args,
        )
    }

    fn r#getActivityClientController(
        &self,
    ) -> binder::Result<Option<Strong<dyn IActivityClientController>>> {
        let controller = <dyn IActivityClientController>::try_from(self.atm.controller())?;
        Ok(Some(controller))
    }
}
