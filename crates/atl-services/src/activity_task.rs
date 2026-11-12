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
use atl_parcelables::android::content::{ComponentName, Intent};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::parts::{
    ActivityResultItem, DestroyActivityItem, NewIntentItem, PauseActivityItem, ReferrerIntent,
    ResultInfo, StartActivityItem, StopActivityItem,
};
use binder::{BinderFeatures, FromIBinder, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::activity::{
    ActivityManager, global_configuration, launch_item, log, resume_item, security,
};
use crate::activity_stack::{Command, Machine, ResultTo, Step};
use crate::activity_start::{self, Call, Refusal, Source, Start, Target};
use crate::config_diff::Config;
use crate::tasks::{TaskId, Tasks};
use crate::token;
use atl_services_types::counters;
use atl_services_types::manifest_flags;
use atl_services_types::packages::{ActivityDecl, Packages};

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
    /// started by the launcher with its MAIN / LAUNCHER intent (ActivityRecord LAUNCH_SOURCE_TYPE_HOME + isMainIntent): an unhandled Back on it as the Task's root moves the Task back, see [`ActivityTasks::back`]
    pub entry: bool,
}

/// What an unhandled Back on an activity does (ActivityClientController.onBackPressed:1720-1747).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Back {
    /// requestFinish: the app finishes the activity (the default, :1744).
    Finish,
    /// moveActivityTaskToBack (:1736-1739): the Task of the app with this uid goes to the back, the activity stays.
    TaskToBack { uid: u32 },
}

/// Intent.ACTION_MAIN / CATEGORY_LAUNCHER: how the launcher starts an app.
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
/// Intent.FLAG_ACTIVITY_NEW_TASK, set on a launcher start.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;

/// What a Launch step is built from: the activity's package and declaration, the LaunchActivityItem's ident, and the intent it delivers.
#[derive(Clone)]
pub(crate) struct Launch {
    pub package: String,
    pub decl: ActivityDecl,
    pub ident: i32,
    /// the start's intent as sent (ActivityRecord.intent, ActivityTaskSupervisor.java:1021); a launcher start's MAIN / LAUNCHER intent
    pub intent: Intent,
}

/// The intent a reused record gets in onNewIntent (ActivityRecord.deliverNewIntentLocked: the start's intent, referrer = the calling package, no caller token here).
type Lifecycle = Machine<SpIBinder, Owner, Launch, ReferrerIntent, ResultInfo>;
/// One command of the machine, sent as one transaction.
type Cmd = Command<SpIBinder, Owner, Launch, ReferrerIntent, ResultInfo>;
/// Activity.RESULT_CANCELED: the result of a start that does not happen.
const RESULT_CANCELED: i32 = 0;

/// The parts of a startActivity call the lifecycle needs besides the caller's identity.
pub(crate) struct StartArgs<'a> {
    pub intent: Option<&'a Intent>,
    /// false: a start that would reuse an instance is refused instead (`--no-new-intent`, the deliberate break of the start-finish stage's new-intent sub-gate)
    pub reuse: bool,
    /// false: the start owes no result, so none is delivered or cancelled (`--no-activity-result`, the deliberate break of the app gate's result sub-gate); the start is decided as with one
    pub results: bool,
    pub request_code: i32,
    pub result_to: Option<&'a SpIBinder>,
    /// resultWho: the name the caller gave the result (startActivity's resultWho)
    pub result_who: Option<&'a str>,
}

/// ActivityRecord's task affinity: the manifest's taskAffinity, else the package name.
pub(crate) fn affinity_of(decl: &ActivityDecl, package: &str) -> String {
    decl.task_affinity
        .clone()
        .unwrap_or_else(|| package.to_owned())
}

/// Answers the result `owed` as cancelled (RESULT_CANCELED, no data), as every cancelled start does (ActivityStarter.java:1222, 2470, 2846); nothing when none is owed.
fn cancel_owed(m: &mut Lifecycle, owed: Option<&ResultTo<SpIBinder>>) -> Vec<Cmd> {
    owed.map_or_else(Vec::new, |to| {
        m.deliver(
            to,
            ResultInfo {
                result_who: to.who.clone(),
                request_code: to.request_code,
                result_code: RESULT_CANCELED,
                data: None,
                caller_token: None,
            },
        )
    })
}

/// Step names of one command for the log: `Pause(finished=true,user_leaving=false),Stop`.
fn step_names<L, I, R>(steps: &[Step<L, I, R>]) -> String {
    steps
        .iter()
        .map(|s| match s {
            Step::Launch(_) => "Launch".to_owned(),
            Step::NewIntent { resume, .. } => format!("NewIntent(resume={resume})"),
            Step::Result(_) => "Result".to_owned(),
            Step::Start => "Start".to_owned(),
            Step::Resume => "Resume".to_owned(),
            Step::Pause {
                finished,
                user_leaving,
            } => format!("Pause(finished={finished},user_leaving={user_leaving})"),
            Step::Stop => "Stop".to_owned(),
            Step::Destroy { finished } => format!("Destroy(finished={finished})"),
        })
        .collect::<Vec<_>>()
        .join(",")
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
    fn item(
        &self,
        token: &SpIBinder,
        step: Step<Launch, ReferrerIntent, ResultInfo>,
    ) -> Option<ClientTransactionItem> {
        let activity_token = Some(token.clone());
        Some(match step {
            Step::Launch(launch) => {
                let Some(p) = self.packages.by_name(&launch.package) else {
                    counters::answered(ATM, "launch of a package not installed");
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
                    launch.intent,
                )))
            }
            Step::Result(results) => ClientTransactionItem::ActivityResult(ActivityResultItem {
                activity_token,
                result_infos: Some(results.into_iter().map(Some).collect()),
            }),
            Step::Start => {
                ClientTransactionItem::StartActivity(StartActivityItem { activity_token })
            }
            Step::NewIntent { intents, resume } => {
                ClientTransactionItem::NewIntent(NewIntentItem {
                    activity_token,
                    resume,
                    intents: Some(intents.into_iter().map(Some).collect()),
                })
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
    fn send(&self, commands: Vec<Cmd>) {
        for command in commands {
            log(format_args!(
                "activity send {} {} at={}",
                step_names(&command.steps),
                command.owner.component,
                counters::wall_stamp()
            ));
            if command
                .steps
                .iter()
                .any(|s| matches!(s, Step::NewIntent { .. }))
            {
                // the line the onNewIntent gate counts
                log(format_args!(
                    "activity new intent to {} at={}",
                    command.owner.component,
                    counters::wall_stamp()
                ));
            }
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

    /// finishAttachApplication's launcher: the process's Task (made at its start, `task`), its record pushed, its launch sent. Returns the record's token.
    pub(crate) fn launch_launcher(
        &self,
        thread: Strong<dyn IApplicationThread>,
        uid: u32,
        package: &str,
        decl: &ActivityDecl,
        task: TaskId,
    ) -> SpIBinder {
        let token = token::new();
        self.tasks.bind_token(token.clone(), task);
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
            entry: true,
        };
        let launch = Launch {
            package: package.to_owned(),
            decl: decl.clone(),
            ident: self.next_ident(),
            intent: Intent {
                action: Some(ACTION_MAIN.into()),
                categories: vec![Some(CATEGORY_LAUNCHER.into())],
                flags: FLAG_ACTIVITY_NEW_TASK,
                component: Some(ComponentName {
                    package: package.to_owned(),
                    class: decl.name.clone(),
                }),
                ..Default::default()
            },
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
            intent,
            reuse,
            results,
            request_code,
            result_to,
            result_who,
        } = args;
        let component = intent
            .and_then(|i| i.component.as_ref())
            .map(|c| (c.package.as_str(), c.class.as_str()));
        // ActivityStarter.mLaunchFlags start from intent.getFlags() (ActivityStarter.java computeLaunchingTaskFlags); the binder `flags` argument is startFlags (debug / profiling), unused here
        let flags = intent.map_or(0, |i| i.flags);
        let mut m = self.machine();
        let source = result_to.and_then(|t| {
            m.owner(t)
                .filter(|o| o.thread.as_binder() == caller.as_binder())
                .map(|o| (t.clone(), o.clone()))
        });
        let source_view = source.as_ref().map(|(t, o)| Source {
            uid: o.uid,
            package: &o.package,
            affinity: &o.affinity,
            finishing: m.is_finishing(t) == Some(true),
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
        // the result the start owes its caller: the source's record, when a result was asked and the source is not finishing (Call::result_requested); none with `results` off
        let mut owed = match (call.result_requested(), &source) {
            (true, Some((to, _))) if results => Some(ResultTo {
                token: to.clone(),
                who: result_who.map(str::to_owned),
                request_code,
            }),
            _ => None,
        };
        let source_task = source.as_ref().and_then(|(t, _)| m.task_of(t));
        let class = component.map(|(_, class)| class);
        let instance = source_task
            .zip(class)
            .and_then(|(task, class)| m.find_in_task(task, |o| o.component == class).cloned());
        let top_is_class = source_task
            .zip(class)
            .is_some_and(|(task, class)| m.top(task).is_some_and(|(_, o)| o.component == class));
        let target = Target {
            process: package.map_or("", |p| p.process_name()),
            decl,
            in_task: instance.is_some(),
            top_is_class,
        };
        let (decl, start) = match activity_start::check(&call, target) {
            Ok(accepted) => accepted,
            Err(Refusal::Code { code, .. }) => {
                // a refused start answers the result it owes as cancelled (ActivityStarter.java:1220-1222)
                let cancelled = cancel_owed(&mut m, owed.as_ref());
                self.note_settle(&m);
                self.send(cancelled);
                return Ok(code);
            }
            Err(Refusal::Security(msg)) => return Err(security(msg)),
        };
        let mut commands: Vec<Cmd> = Vec::new();
        // an explicit NEW_TASK cancels the owed result before the start is decided (sendNewTaskResultRequestIfNeeded, ActivityStarter.java:2718, 2846-2849)
        if call.new_task_cancels_result() {
            commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
        }
        let (Some(task), Some(intent)) = (source_task, intent) else {
            commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
            self.note_settle(&m);
            self.send(commands);
            return Ok(activity_start::START_CANCELED);
        };
        if let (Start::Reuse { .. }, false) = (start, reuse) {
            counters::refused(ATM, "startActivity (reuse refused: --no-new-intent)");
            commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
            self.note_settle(&m);
            self.send(commands);
            return Ok(activity_start::START_CANCELED);
        }
        if let (Start::Reuse { clear_top }, Some(instance)) = (start, instance) {
            // ActivityStarter.deliverToCurrentTopIfNeeded: the owed result is cancelled first (:2469), then the intent goes to the instance (deliverNewIntent);
            // START_DELIVERED_TO_TOP (3) for the top, START_TASK_TO_FRONT (2) after a clear-top; both are success codes the app does not throw for
            let referrer = ReferrerIntent {
                intent: intent.clone(),
                referrer: Some(calling_package.to_owned()),
                caller_token: None,
            };
            commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
            commands.extend(
                m.new_intent(&instance, referrer, clear_top)
                    .unwrap_or_default(),
            );
            self.note_settle(&m);
            self.send(commands);
            return Ok(if clear_top {
                activity_start::START_TASK_TO_FRONT
            } else {
                activity_start::START_DELIVERED_TO_TOP
            });
        }
        let token = token::new();
        self.tasks.bind_token(token.clone(), task);
        let owner = Owner {
            thread: caller.clone(),
            uid: calling_uid,
            package: calling_package.to_owned(),
            affinity: affinity_of(decl, calling_package),
            component: decl.name.clone(),
            reported: self.launch_config(),
            declared: package.map_or(0, |p| manifest_flags::real_config_changes(p, decl)),
            relaunching: false,
            entry: false,
        };
        let launch = Launch {
            package: calling_package.to_owned(),
            decl: decl.clone(),
            ident: self.next_ident(),
            intent: intent.clone(),
        };
        commands.extend(m.push_owed(task, token, owner, launch, owed));
        self.note_settle(&m);
        self.send(commands);
        Ok(activity_start::START_SUCCESS)
    }

    /// ActivityClientController.onBackPressed's choice for the record with `token`; None when no record has it (isInRootTaskLocked null: nothing happens). A lookup only.
    /// shouldMoveTaskToBack (:1750-1767): the record is its Task's root (the bottom record), the root is the launcher's entry, and the record is the Task's top. isRelativeTaskRootActivity is
    /// false here (no TaskFragments, so no record sits on a finishing root), and no task organizer intercepts (:1728-1735).
    pub(crate) fn back(&self, token: &SpIBinder) -> Option<Back> {
        let m = self.machine();
        let task = m.task_of(token)?;
        let records: Vec<_> = m.records(task).collect();
        let (root, root_owner) = records.first()?;
        let top = records.last().map(|(t, _)| *t);
        Some(
            if *root == token && root_owner.entry && top == Some(token) {
                Back::TaskToBack {
                    uid: root_owner.uid,
                }
            } else {
                Back::Finish
            },
        )
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
        let component = m
            .owner(token)
            .map(|o| o.component.clone())
            .unwrap_or_default();
        log(format_args!(
            "activity paused {component} at={}",
            counters::wall_stamp()
        ));
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
        let component = m
            .owner(token)
            .map(|o| o.component.clone())
            .unwrap_or_default();
        log(format_args!(
            "activity stopped {component} at={}",
            counters::wall_stamp()
        ));
        let commands = m.stopped(token);
        self.note_settle(&m);
        self.send(commands);
        true
    }

    /// The window of `task` is shown or hidden (Machine::set_visible). Logged as `activity task <id> visible=<bool>` before the commands it causes.
    /// No caller yet: the composer's visibility report reaches it in window lifecycle S2.
    #[expect(dead_code, reason = "S2 sends the composer's visibility report here")]
    pub(crate) fn visibility(&self, task: TaskId, visible: bool) {
        let mut m = self.machine();
        log(format_args!(
            "activity task {} visible={visible} at={}",
            task.raw(),
            counters::wall_stamp()
        ));
        let commands = m.set_visible(task, visible);
        self.note_settle(&m);
        self.send(commands);
    }

    /// finishActivity: the record is finishing and its result (`code`, `data`) goes to its caller (ActivityRecord.finishActivityResults); false when the token names no record.
    /// The result's callerToken stays null: AOSP's per-result caller Binder (content-URI identity) is not modelled.
    pub(crate) fn finish(&self, token: &SpIBinder, code: i32, data: Option<Intent>) -> bool {
        let mut m = self.machine();
        let result = |to: &ResultTo<SpIBinder>| ResultInfo {
            result_who: to.who.clone(),
            request_code: to.request_code,
            result_code: code,
            data,
            caller_token: None,
        };
        let component = m
            .owner(token)
            .map(|o| o.component.clone())
            .unwrap_or_default();
        log(format_args!(
            "activity finish {component} code={code} at={}",
            counters::wall_stamp()
        ));
        match m.finish(token, result) {
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

    /// The records of `task` with their owners, bottom -> top, that a configuration change of the Task is delivered to: not finishing (ActivityRecord.java:8441-8445 ensureActivityConfiguration
    /// skips a finishing activity; relaunching one re-ran a splash's onCreate, which started its next activity again). A destroyed record is not on the stack, so not listed.
    pub(crate) fn config_records(&self, task: TaskId) -> Vec<(SpIBinder, Owner)> {
        let m = self.machine();
        m.records(task)
            .filter(|(k, _)| m.is_finishing(k) == Some(false))
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
    /// the processes: a start with no caller app starts one (ActivityManager::start_without_caller)
    am: ActivityManager,
    /// StartArgs::reuse
    reuse: bool,
    /// StartArgs::results
    results: bool,
}

impl ActivityTaskService {
    pub fn new(am: ActivityManager, reuse: bool, results: bool) -> Self {
        ActivityTaskService {
            atm: am.activity_tasks(),
            am,
            reuse,
            results,
        }
    }

    /// `reuse` false: the test-only `--no-new-intent` (StartArgs::reuse); `results` false: the test-only `--no-activity-result` (StartArgs::results).
    pub fn new_binder(am: ActivityManager, reuse: bool, results: bool) -> SpIBinder {
        BnActivityTaskManager::new_binder(
            ActivityTaskService::new(am, reuse, results),
            BinderFeatures::default(),
        )
        .as_binder()
    }
}

impl Interface for ActivityTaskService {}

impl IActivityTaskManager for ActivityTaskService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        atl_services_wire::unimplemented(ATM, method)
    }

    fn r#startActivity(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        calling_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        intent: Option<&Intent>,
        _resolved_type: Option<&str>,
        result_to: Option<&SpIBinder>,
        result_who: Option<&str>,
        request_code: i32,
        _start_flags: i32,
        _profiler_info: Option<&ProfilerInfo>,
        _options: Option<&Bundle>,
    ) -> binder::Result<i32> {
        let calling_uid = ThreadState::get_calling_uid();
        let Some(calling_package) = calling_package else {
            return Err(security("startActivity: no calling package"));
        };
        let Some(caller) = caller else {
            let component = intent
                .and_then(|i| i.component.as_ref())
                .map(|c| (c.package.as_str(), c.class.as_str()));
            return self.am.start_without_caller(
                calling_uid,
                intent.map_or(0, |i| i.flags),
                component,
            );
        };
        let args = StartArgs {
            intent,
            reuse: self.reuse,
            results: self.results,
            request_code,
            result_to,
            result_who,
        };
        self.atm.start(caller, calling_uid, calling_package, args)
    }

    fn r#getActivityClientController(
        &self,
    ) -> binder::Result<Option<Strong<dyn IActivityClientController>>> {
        let controller = <dyn IActivityClientController>::try_from(self.atm.controller())?;
        Ok(Some(controller))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use atl_aidl::aidl::android::app::IApplicationThread::BnApplicationThread;
    use atl_parcelables::codecs::android::os::Bundle::Value;
    use atl_services_types::display_model::DisplayModel;
    use atl_services_types::packages::{Manifest, PackageRecord, SCREEN_ORIENTATION_UNSPECIFIED};
    use binder::StatusCode;

    use super::*;
    use crate::tasks::TaskSize;

    const PACKAGE: &str = "com.example.player";
    const LAUNCHER: &str = "com.example.player.Main";
    const VIDEO: &str = "com.example.player.Video";
    const UID: u32 = 10_123;
    /// the request code of a startActivityForResult
    const REQUEST_CODE: i32 = 1131;

    /// Keeps every transaction scheduled on it, in order.
    struct Thread {
        scheduled: Arc<Mutex<Vec<ClientTransaction>>>,
    }

    impl Interface for Thread {}

    impl IApplicationThread for Thread {
        fn r#unimplemented(&self, _method: &'static str) -> Status {
            StatusCode::UNKNOWN_TRANSACTION.into()
        }

        fn r#scheduleTransaction(
            &self,
            transaction: Option<&ClientTransaction>,
        ) -> binder::Result<()> {
            let transaction = transaction.cloned().unwrap_or_default();
            self.scheduled
                .lock()
                .expect("transactions lock")
                .push(transaction);
            Ok(())
        }
    }

    /// The app thread binder, and the list the services' transactions for it are kept in.
    fn app_thread() -> (
        Strong<dyn IApplicationThread>,
        Arc<Mutex<Vec<ClientTransaction>>>,
    ) {
        let scheduled = Arc::new(Mutex::new(Vec::new()));
        let thread = BnApplicationThread::new_binder(
            Thread {
                scheduled: Arc::clone(&scheduled),
            },
            BinderFeatures::default(),
        );
        (thread, scheduled)
    }

    /// `name` as the manifest declares it: standard launch mode (LAUNCH_MULTIPLE = 0), no attribute beyond its name.
    fn decl(name: &str) -> ActivityDecl {
        ActivityDecl {
            name: name.to_owned(),
            launcher: name == LAUNCHER,
            theme: 0,
            config_changes: 0,
            booleans: BTreeMap::new(),
            launch_mode: 0,
            task_affinity: None,
            process: None,
            meta_data: vec![],
            screen_orientation: SCREEN_ORIENTATION_UNSPECIFIED,
            resizeable: true,
            min_aspect_ratio: None,
            max_aspect_ratio: None,
        }
    }

    /// One installed package: its launcher and one video activity.
    fn fixture_atm() -> ActivityTasks {
        let manifest = Manifest {
            name: PACKAGE.to_owned(),
            version_code: 1,
            version_name: None,
            min_sdk: 21,
            target_sdk: 34,
            process_name: None,
            application_theme: 0,
            application_class: None,
            app_component_factory: None,
            application_meta_data: vec![],
            application_booleans: BTreeMap::new(),
            activities: vec![decl(LAUNCHER), decl(VIDEO)],
            receivers: vec![],
            providers: vec![],
            requested_permissions: vec![],
            permissions: vec![],
            libraries: vec![],
            instrumentation: vec![],
        };
        let record = PackageRecord {
            manifest,
            uid: UID,
            source_dir: String::new(),
            primary_cpu_abi: None,
            native_library_dir: String::new(),
            data_dir: String::new(),
            device_protected_data_dir: String::new(),
            granted_permissions: vec![],
            gl_driver: Default::default(),
        };
        ActivityTasks::new(
            Arc::new(Packages::new(vec![record], BTreeMap::new())),
            Arc::new(Tasks::new(DisplayModel::default())),
            token::new(),
        )
    }

    /// The launcher record, pushed and launched as finishAttachApplication does it; returns its token.
    fn attach_launcher(atm: &ActivityTasks, thread: &Strong<dyn IApplicationThread>) -> SpIBinder {
        let task = atm.tasks.new_task(
            UID,
            PACKAGE,
            TaskSize {
                width: 480,
                height: 800,
            },
        );
        atm.launch_launcher(thread.clone(), UID, PACKAGE, &decl(LAUNCHER), task)
    }

    /// The intent of the LaunchActivityItem scheduled for `class`, as the app thread received it.
    fn intent_of(scheduled: &Mutex<Vec<ClientTransaction>>, class: &str) -> Intent {
        scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .filter_map(|item| match item {
                ClientTransactionItem::LaunchActivity(launch) => launch.intent.clone(),
                _ => None,
            })
            .find(|i| i.component.as_ref().is_some_and(|c| c.class == class))
            .expect("a LaunchActivityItem for the class")
    }

    /// The start an app's startActivity carries (ZVideoPlayer: setClass + putExtra, no action, no flags) is the intent the fresh launch of its activity delivers: the caller's fields, extras included.
    #[test]
    fn fresh_start_delivers_the_caller_intent() {
        let atm = fixture_atm();
        let (thread, scheduled) = app_thread();
        let source = attach_launcher(&atm, &thread);
        let pairs = [("path", "/sdcard/video.mp4"), ("mute", "true")];
        let caller_intent = Intent {
            component: Some(ComponentName {
                package: PACKAGE.to_owned(),
                class: VIDEO.to_owned(),
            }),
            extras: Some(Bundle::from_strings(&pairs)),
            ..Default::default()
        };
        let status = atm
            .start(
                &thread,
                UID,
                PACKAGE,
                StartArgs {
                    intent: Some(&caller_intent),
                    reuse: true,
                    results: true,
                    request_code: REQUEST_CODE,
                    result_to: Some(&source),
                    result_who: None,
                },
            )
            .expect("start");
        assert_eq!(status, activity_start::START_SUCCESS);

        let launched = intent_of(&scheduled, VIDEO);
        // expected: the caller's own fields, read from the input (AOSP ActivityRecord keeps r.intent as the start's intent)
        assert_eq!(launched.action, caller_intent.action);
        assert_eq!(launched.categories, caller_intent.categories);
        assert_eq!(launched.flags, caller_intent.flags);
        assert_eq!(launched.component, caller_intent.component);
        // a Bundle is a map: the entries compare by key, not in the order the codec returns them
        let expected: BTreeMap<String, Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String(Some((*v).to_owned()))))
            .collect();
        let extras: BTreeMap<String, Value> = launched
            .extras
            .as_ref()
            .map_or_else(Vec::new, |b| b.entries().expect("extras"))
            .into_iter()
            .collect();
        assert_eq!(extras, expected);
    }

    /// The launcher start carries AOSP's launcher intent: ACTION_MAIN, CATEGORY_LAUNCHER, FLAG_ACTIVITY_NEW_TASK (0x10000000), the launcher's component, no extras.
    #[test]
    fn launcher_start_is_main_launcher_in_a_new_task() {
        let atm = fixture_atm();
        let (thread, scheduled) = app_thread();
        attach_launcher(&atm, &thread);

        let launched = intent_of(&scheduled, LAUNCHER);
        assert_eq!(
            launched.action.as_deref(),
            Some("android.intent.action.MAIN")
        );
        assert_eq!(
            launched.categories,
            vec![Some("android.intent.category.LAUNCHER".to_owned())]
        );
        assert_eq!(launched.flags, 0x1000_0000);
        assert_eq!(
            launched.component,
            Some(ComponentName {
                package: PACKAGE.to_owned(),
                class: LAUNCHER.to_owned(),
            })
        );
        assert!(launched.extras.is_none());
    }
}
