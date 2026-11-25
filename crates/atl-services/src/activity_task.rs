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
    ActivityManager, configuration_change_item, global_configuration, launch_item, log,
    process_configuration, resume_item, security, sent_task_override, task_configuration,
};
use crate::activity_stack::{Cancelled, Push};
use crate::activity_stack::{Command, Machine, ResultTo, Step};
use crate::activity_start::{self, Call, Rec, Refusal, Source, Target, Then};
use crate::config_diff::Config;
use crate::desktop_bounds::{ActivityInput, ActivityOrientation};
use crate::package::USER_SYSTEM;
use crate::packages::{PackageTable, Packages, Permissions, real_config_changes};
use crate::permission_request;
use crate::tasks::{TaskId, Tasks};
use crate::token;
use atl_parcelables::android::content::pm::{ActivityInfo, ConfigurationInfo};
use atl_parcelables::android::content::res::Configuration;
use atl_parcelables::codecs::android::content::pm::ResolveInfo::Resolved;
use atl_services_types::packages::SCREEN_ORIENTATION_UNSPECIFIED;
use atl_services_types::permission::{ROOT_UID, SYSTEM_UID};

const ATM: &str = "IActivityTaskManager";

/// Configuration.KEYBOARD_UNDEFINED / KEYBOARD_NOKEYS, NAVIGATION_DPAD / NAVIGATION_TRACKBALL (Configuration.java) and ConfigurationInfo.INPUT_FEATURE_HARD_KEYBOARD / FIVE_WAY_NAV (ConfigurationInfo.java).
const KEYBOARD_UNDEFINED: i32 = 0;
const KEYBOARD_NOKEYS: i32 = 1;
const NAVIGATION_DPAD: i32 = 2;
const NAVIGATION_TRACKBALL: i32 = 3;
const INPUT_FEATURE_HARD_KEYBOARD: i32 = 1;
const INPUT_FEATURE_FIVE_WAY_NAV: i32 = 2;

/// ActivityTaskManagerService.getDeviceConfigurationInfo (:1155): the input fields of the caller's global configuration, the input features they imply, and the GL ES version read at service start.
pub(crate) fn device_configuration_info(
    global: &Configuration,
    gl_es_version: i32,
) -> ConfigurationInfo {
    let mut features = 0;
    if global.navigation == NAVIGATION_DPAD || global.navigation == NAVIGATION_TRACKBALL {
        features |= INPUT_FEATURE_FIVE_WAY_NAV;
    }
    if global.keyboard != KEYBOARD_UNDEFINED && global.keyboard != KEYBOARD_NOKEYS {
        features |= INPUT_FEATURE_HARD_KEYBOARD;
    }
    ConfigurationInfo {
        req_touch_screen: global.touchscreen,
        req_keyboard_type: global.keyboard,
        req_navigation: global.navigation,
        req_input_features: features,
        req_gl_es_version: gl_es_version,
    }
}

/// What the machine keeps per activity (ActivityRecord, reduced): its app thread, the identity its starts are checked against, and the configuration state a change is delivered by.
#[derive(Clone)]
pub(crate) struct Owner {
    pub thread: Strong<dyn IApplicationThread>,
    /// the pid of the process the activity runs in (ActivityRecord.app): the process whose configuration follows it ([`Machine::process_top`])
    pub pid: i32,
    pub uid: u32,
    pub package: String,
    /// the affinity of the activity's task: its taskAffinity, else the package name (see [`affinity_of`])
    pub affinity: String,
    /// the activity's class name (ActivityInfo.name); the services' log names the activity by it
    pub component: String,
    /// the merged configuration the app last got (ActivityRecord.mLastReportedConfiguration); a configuration change is diffed against it (activity.rs task_resized)
    pub reported: Config,
    /// ActivityInfo.getRealConfigChanged: the CONFIG_* bits the activity handles itself
    pub declared: u32,
    /// a relaunch was sent and activityRelaunched has not come (ActivityRecord.mPendingRelaunchCount, at most 1 here: Delivery::Wait)
    pub relaunching: bool,
    /// started by the launcher with its MAIN / LAUNCHER intent (ActivityRecord LAUNCH_SOURCE_TYPE_HOME + isMainIntent): an unhandled Back on it as the Task's root moves the Task back, see [`ActivityTasks::back`]
    pub entry: bool,
    /// the label of the activity's last setTaskDescription (ActivityRecord.taskDescription.getLabel()); None until it sets one, and for a null label
    pub task_label: Option<String>,
    /// ActivityRecord.mOrientation: the manifest's screenOrientation, then each setRequestedOrientation; kept across a relaunch (same record)
    pub orientation: i32,
    /// the manifest's resize mode and aspect limits as the initial-bounds rule reads them (packages::bounds_input); read through [`Owner::bounds_input`], which takes the orientation from `orientation`
    pub bounds: ActivityInput,
}

impl Owner {
    /// What the bounds rules read of this activity now: its manifest limits with its requested orientation.
    pub(crate) fn bounds_input(&self) -> ActivityInput {
        ActivityInput {
            orientation: ActivityOrientation::from_screen_orientation(self.orientation),
            ..self.bounds
        }
    }
}

/// What a Task's window is titled from ([`ActivityTasks::task_title`]): the merged TaskDescription label, and the root activity whose label stands in when there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskTitle {
    /// the Task titled: its window is in the process the Task was launched in (activity.rs process_of_task)
    pub task: TaskId,
    pub label: Option<String>,
    pub package: String,
    pub class: String,
}

/// What an unhandled Back on an activity does (ActivityClientController.onBackPressed:1720-1747).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Back {
    /// requestFinish: the app finishes the activity (the default, :1744).
    Finish,
    /// moveActivityTaskToBack (:1736-1739): `task`, of the app with this uid, goes to the back, the activity stays.
    TaskToBack { uid: u32, task: TaskId },
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
    pub decl: ActivityInfo,
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
    /// resolvedType: the intent's MIME type as the caller resolved it (Intent.resolveTypeIfNeeded), matched against the filters' data types
    pub resolved_type: Option<&'a str>,
}

/// ActivityRecord's task affinity: the manifest's taskAffinity, else the package name.
pub(crate) fn affinity_of(decl: &ActivityInfo, package: &str) -> String {
    decl.task_affinity
        .clone()
        .unwrap_or_else(|| package.to_owned())
}

/// Answers the result `owed` as cancelled (RESULT_CANCELED, no data), as every cancelled start does (ActivityStarter.java:1222, 2470, 2846); nothing when none is owed.
fn cancel_owed(m: &mut Lifecycle, owed: Option<&ResultTo<SpIBinder>>) -> Vec<Cmd> {
    answer_owed(m, owed, RESULT_CANCELED, None)
}

/// The result a record the services finish answers (clear-top, clear-task, no-history: finishIfPossible): RESULT_CANCELED, no data.
impl Cancelled<SpIBinder> for ResultInfo {
    fn cancelled(to: &ResultTo<SpIBinder>) -> Self {
        ResultInfo {
            result_who: to.who.clone(),
            request_code: to.request_code,
            result_code: RESULT_CANCELED,
            data: None,
            caller_token: None,
        }
    }
}

/// Answers the result `owed` with (`code`, `data`) through the one deliver path (Machine::deliver: at once to a resumed caller, else with its next resume); nothing when none is owed.
fn answer_owed(
    m: &mut Lifecycle,
    owed: Option<&ResultTo<SpIBinder>>,
    code: i32,
    data: Option<Intent>,
) -> Vec<Cmd> {
    owed.map_or_else(Vec::new, |to| {
        m.deliver(
            to,
            ResultInfo {
                result_who: to.who.clone(),
                request_code: to.request_code,
                result_code: code,
                data,
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
    packages: Arc<PackageTable>,
    /// the callers' grants: a dismissed permission request returns their current state (permission_request)
    permissions: Arc<Permissions>,
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
        packages: Arc<PackageTable>,
        permissions: Arc<Permissions>,
        tasks: Arc<Tasks<SpIBinder>>,
        controller: SpIBinder,
    ) -> Self {
        ActivityTasks {
            packages,
            permissions,
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
                atl_diag::wall_stamp()
            ));
        }
    }

    /// The transaction item of one step of `token`'s record, which runs in the process `pid`. None: a launch of a package that is no longer installed (counted, never reached).
    fn item(
        &self,
        token: &SpIBinder,
        pid: i32,
        step: Step<Launch, ReferrerIntent, ResultInfo>,
    ) -> Option<ClientTransactionItem> {
        let packages = self.packages.snapshot();
        let activity_token = Some(token.clone());
        Some(match step {
            Step::Launch(launch) => {
                let Some(p) = packages.by_name(&launch.package) else {
                    atl_diag::answered(ATM, "launch of a package not installed");
                    return None;
                };
                // the token is bound to its Task before the machine sends its launch, and Tasks are never removed: no Task or size is a broken invariant, refused and counted
                let Some((task, size)) = self
                    .tasks
                    .task_of_token(Some(token))
                    .and_then(|task| Some((task, self.tasks.size(task)?)))
                else {
                    atl_diag::refused(ATM, "launch of an activity whose Task has no size");
                    return None;
                };
                let display = self.tasks.display();
                ClientTransactionItem::LaunchActivity(Box::new(launch_item(
                    &packages,
                    process_configuration(&*self.tasks, pid),
                    sent_task_override(&*self.tasks, &display, task, size),
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
    fn send(&self, m: &Lifecycle, commands: Vec<Cmd>) {
        for (pid, thread) in self.follow_process_tops(m) {
            // ActivityTaskSupervisor.java:838-846: a launch carries the process configuration (curConfig), the change is not sent apart
            let launched = commands.iter().any(|c| {
                c.owner.pid == pid && c.steps.iter().any(|s| matches!(s, Step::Launch(_)))
            });
            if launched {
                continue;
            }
            let transaction = ClientTransaction {
                items: vec![Some(configuration_change_item(process_configuration(
                    &*self.tasks,
                    pid,
                )))],
            };
            if let Err(e) = thread.r#scheduleTransaction(Some(&transaction)) {
                log(format_args!(
                    "scheduleTransaction (process configuration): {e}"
                ));
            }
        }
        for command in commands {
            log(format_args!(
                "activity send {} {} at={}",
                step_names(&command.steps),
                command.owner.component,
                atl_diag::wall_stamp()
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
                    atl_diag::wall_stamp()
                ));
            }
            let pid = command.owner.pid;
            let items = command
                .steps
                .into_iter()
                .filter_map(|step| self.item(&command.token, pid, step))
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

    /// WindowProcessController.updateActivityConfigurationListener after the machine decided: each process with a record follows its record launched last (Machine::process_top), recorded in
    /// Tasks (what process_configuration reads). Returns the processes whose followed Task changed, with the thread to tell them by.
    /// A process with no record in any Task is left as it is: it may be bound for its first launch (bind_configuration) and not pushed yet; one whose records all went keeps its last Task
    /// until it dies (activity.rs forgets it then) where AOSP returns it to the global configuration -- not observable while the services end a process whose Task emptied (kill_task_processes).
    fn follow_process_tops(&self, m: &Lifecycle) -> Vec<(i32, Strong<dyn IApplicationThread>)> {
        let mut seen: Vec<i32> = Vec::new();
        let mut changed = Vec::new();
        for owner in m.owners() {
            if seen.contains(&owner.pid) {
                continue;
            }
            seen.push(owner.pid);
            let Some(top) = m.process_top(|o| o.pid == owner.pid) else {
                continue;
            };
            if self.tasks.set_process_task(owner.pid, Some(top)) {
                log(format_args!(
                    "process {} configuration follows task {}",
                    owner.pid,
                    top.raw()
                ));
                changed.push((owner.pid, owner.thread.clone()));
            }
        }
        changed
    }

    /// bindApplication's configuration (ActivityTaskManagerService.preBindApplication:6792-6809): a process started for the launch of an activity into `task` follows that Task from the
    /// bind, so its Application starts with the configuration its activity will have; any other process (`task` None) gets the global one. Set either way: a reused pid starts clean.
    pub(crate) fn bind_configuration(&self, pid: i32, task: Option<TaskId>) -> Configuration {
        self.tasks.set_process_task(pid, task);
        process_configuration(&*self.tasks, pid)
    }

    /// finishAttachApplication's launcher: the process's Task (made at its start, `task`), its record pushed, its launch sent. Returns the record's token.
    pub(crate) fn launch_launcher(
        &self,
        thread: Strong<dyn IApplicationThread>,
        pid: i32,
        uid: u32,
        package: &str,
        decl: &ActivityInfo,
        task: TaskId,
    ) -> SpIBinder {
        let packages = self.packages.snapshot();
        let token = token::new();
        self.tasks.bind_token(token.clone(), task);
        let owner = Owner {
            thread,
            pid,
            uid,
            package: package.to_owned(),
            affinity: affinity_of(decl, package),
            component: decl.component.item.name.clone().unwrap_or_default(),
            reported: self.launch_config(task),
            declared: packages
                .by_name(package)
                .map_or(0, |_| real_config_changes(decl)),
            relaunching: false,
            entry: true,
            task_label: None,
            orientation: decl.screen_orientation,
            bounds: crate::packages::bounds_input(decl),
        };
        // the Task's size came from this root's orientation (Tasks::opening_size): the base the first top change is compared with
        self.tasks
            .replace_top_orientation(task, decl.screen_orientation);
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
                    class: decl.component.item.name.clone().unwrap_or_default(),
                }),
                ..Default::default()
            },
        };
        let mut m = self.machine();
        let push = Push {
            no_history: crate::packages::declared(decl).no_history,
            ..Push::default()
        };
        let commands = m.push_with(task, token.clone(), owner, launch, push);
        self.note_settle(&m);
        self.send(&m, commands);
        token
    }

    /// startActivity: validate, then the new record goes on top of the caller's activity's Task and is sent its launch. Ok: the START code (0 started, else refused).
    /// A foreign resultTo (not one of this caller's records) is a SecurityException; with no resultTo record the start goes into the caller's Task of the activity's affinity (activity_start::check).
    /// The new record runs in the caller's process (`calling_pid`).
    pub(crate) fn start(
        &self,
        caller: &Strong<dyn IApplicationThread>,
        calling_pid: i32,
        calling_uid: u32,
        calling_package: &str,
        args: StartArgs<'_>,
    ) -> Result<i32, Status> {
        let packages = self.packages.snapshot();
        let StartArgs {
            intent,
            reuse,
            results,
            request_code,
            result_to,
            result_who,
            resolved_type,
        } = args;
        // ActivityStarter.Request.resolveActivity -> ActivityTaskSupervisor.resolveIntent (MATCH_DEFAULT_ONLY, :768-769) -> the package manager's one resolver; resolveActivity stores the
        // found component back into the intent (:712-716), so the rest decides on an explicit start. No match (or a chooser tie, M76: no ResolverActivity) keeps the intent implicit:
        // activity_start refuses it with START_INTENT_NOT_RESOLVED -> ActivityNotFoundException.
        let resolved = intent
            .filter(|i| i.component.is_none())
            .and_then(|i| self.resolve(calling_uid, i, resolved_type));
        let intent = resolved.as_ref().or(intent);
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
            owes: m.owed(t).is_some(),
        });
        let package = component.and_then(|(p, _)| packages.by_name(p));
        let info = component.and_then(|(_, class)| package.and_then(|p| p.activity(class)));
        let declared = info.map(crate::packages::declared);
        let call = Call {
            calling_uid,
            calling_package,
            request_code,
            flags,
            component,
            source: source_view,
        };
        // the result the start owes its caller: the source's record, when a result was asked and the source is not finishing (Call::result_requested); with FORWARD_RESULT the source's own,
        // taken from it before the start is decided (ActivityStarter.java:1130-1136; a refused start then answers it cancelled, :1222); none with `results` off
        let mut owed = match (call.result_requested(), &source) {
            (true, Some((to, _))) if call.forwards() => m.take_owed(to).filter(|_| results),
            (true, Some((to, o))) if results => Some(ResultTo {
                token: to.clone(),
                who: result_who.map(str::to_owned),
                request_code,
                caller: (o.package.clone(), o.component.clone()),
            }),
            _ => None,
        };
        // runtime-permissions P1 (maintainer 2026-11-14): no permission controller installed -> the request is answered as dismissed; deleted when PermissionController is the controller (S4)
        if let Some(request) = intent.filter(|_| {
            permission_request::answered_as_dismissed(intent, packages.permission_controller())
        }) {
            let target_sdk = packages
                .by_name(calling_package)
                .map_or(0, |p| p.app().target_sdk_version);
            let data =
                permission_request::dismissed_result(request, calling_package, target_sdk, |p| {
                    self.permissions.check(calling_uid, Some(p))
                });
            atl_diag::answered(
                ATM,
                "startActivity REQUEST_PERMISSIONS (no permission controller: dismissed, P1)",
            );
            log(format_args!(
                "activity permission request from {calling_package} answered as dismissed (no permission controller) owed={} at={}",
                owed.is_some(),
                atl_diag::wall_stamp()
            ));
            let commands = answer_owed(
                &mut m,
                owed.as_ref(),
                permission_request::RESULT_OK,
                Some(data),
            );
            self.note_settle(&m);
            self.send(&m, commands);
            return Ok(activity_start::START_SUCCESS);
        }
        // the Task the start goes into: the source's; with no source record, the caller's Task whose root has the activity's affinity (ActivityStarter.getReusableIntentActivity -> findTask)
        let source_task = match &source {
            Some((t, _)) => m.task_of(t),
            None => info.map(|d| affinity_of(d, calling_package)).and_then(|a| {
                m.task_by_root(|o| o.thread.as_binder() == caller.as_binder() && o.affinity == a)
            }),
        };
        // the Task's records as the decision reads them, bottom -> top (tokens kept beside, in the same order)
        let records: Vec<(SpIBinder, String, bool)> = source_task
            .map(|task| {
                m.records(task)
                    .map(|(k, o)| {
                        (
                            k.clone(),
                            o.component.clone(),
                            m.is_finishing(k) == Some(true),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let view: Vec<Rec<'_>> = records
            .iter()
            .map(|(k, class, finishing)| Rec {
                class,
                finishing: *finishing,
                source: source.as_ref().is_some_and(|(t, _)| t == k),
            })
            .collect();
        let target = Target {
            process: package.map_or("", |p| p.process_name()),
            decl: declared.as_ref(),
            task: source_task.map(|_| view.as_slice()),
        };
        let (decl, plan) = match activity_start::decide(&call, target) {
            Ok((_, plan)) => (
                info.expect("decide accepts only a class the package declares"),
                plan,
            ),
            Err(Refusal::Code {
                code,
                reason,
                flags: bits,
            }) => {
                log(format_args!(
                    "activity start refused code={code} ({reason}{}): component={component:?} flags=0x{flags:x} result_to={} request_code={request_code} caller={calling_package} at={}",
                    if bits == 0 {
                        String::new()
                    } else {
                        format!(": {}", activity_start::flag_names(bits))
                    },
                    match (result_to, &source) {
                        (None, _) => "null",
                        (Some(_), None) => "no record",
                        (Some(_), Some(_)) => "record",
                    },
                    atl_diag::wall_stamp()
                ));
                // a refused start answers the result it owes as cancelled (ActivityStarter.java:1220-1222)
                let cancelled = cancel_owed(&mut m, owed.as_ref());
                self.note_settle(&m);
                self.send(&m, cancelled);
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
            self.send(&m, commands);
            return Ok(activity_start::START_CANCELED);
        };
        if let (Then::Deliver { .. }, false) = (plan.then, reuse) {
            atl_diag::refused(ATM, "startActivity (reuse refused: --no-new-intent)");
            commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
            self.note_settle(&m);
            self.send(&m, commands);
            return Ok(activity_start::START_CANCELED);
        }
        let token_at = |i: usize| records[i].0.clone();
        if !plan.finish.is_empty() {
            log(format_args!(
                "activity start finishes {} for {component:?} flags=0x{flags:x} at={}",
                plan.finish
                    .iter()
                    .map(|&i| records[i].1.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                atl_diag::wall_stamp()
            ));
        }
        let finished: Vec<SpIBinder> = plan.finish.iter().map(|&i| token_at(i)).collect();
        commands.extend(m.finish_for_start(&finished));
        if let Then::Deliver {
            to,
            to_front,
            cancel,
        } = plan.then
        {
            // deliverNewIntent: deliverToCurrentTopIfNeeded cancels the owed result first (:2469); recycleTask's reuse (clear-top, singleTask, reorder) sends none, the result is
            // never answered. START_DELIVERED_TO_TOP: a success code the app does not throw for.
            let instance = token_at(to);
            let referrer = ReferrerIntent {
                intent: intent.clone(),
                referrer: Some(calling_package.to_owned()),
                caller_token: None,
            };
            if cancel {
                commands.extend(cancel_owed(&mut m, owed.take().as_ref()));
            }
            if to_front {
                m.move_to_top(&instance);
            }
            commands.extend(
                m.new_intent(&instance, referrer, plan.user_leaving)
                    .unwrap_or_default(),
            );
            self.note_settle(&m);
            self.send(&m, commands);
            return Ok(plan.code());
        }
        let token = token::new();
        self.tasks.bind_token(token.clone(), task);
        let owner = Owner {
            thread: caller.clone(),
            pid: calling_pid,
            uid: calling_uid,
            package: calling_package.to_owned(),
            affinity: affinity_of(decl, calling_package),
            component: decl.component.item.name.clone().unwrap_or_default(),
            reported: self.launch_config(task),
            declared: package.map_or(0, |_| real_config_changes(decl)),
            relaunching: false,
            entry: false,
            task_label: None,
            orientation: decl.screen_orientation,
            bounds: crate::packages::bounds_input(decl),
        };
        let launch = Launch {
            package: calling_package.to_owned(),
            decl: decl.clone(),
            ident: self.next_ident(),
            intent: intent.clone(),
        };
        let push = Push {
            owed,
            no_history: plan.no_history,
            user_leaving: plan.user_leaving,
        };
        commands.extend(m.push_with(task, token, owner, launch, push));
        self.note_settle(&m);
        self.send(&m, commands);
        Ok(plan.code())
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
                    task,
                }
            } else {
                Back::Finish
            },
        )
    }

    /// `intent` made explicit to the activity the package manager resolves it to for `caller` (visibility, MATCH_DEFAULT_ONLY, priority); None: no activity, or a chooser tie.
    fn resolve(&self, caller: u32, intent: &Intent, resolved_type: Option<&str>) -> Option<Intent> {
        let packages = self.packages.snapshot();
        let found = crate::package::resolve_activity(
            &packages,
            caller,
            intent,
            resolved_type,
            crate::package::MATCH_DEFAULT_ONLY,
            USER_SYSTEM,
        )?;
        let Resolved::Activity(a) = found.info else {
            return None;
        };
        let item = a.component.item;
        let component = ComponentName {
            package: item.package_name?,
            class: item.name?,
        };
        log(format_args!(
            "activity implicit start resolved to {}/{}",
            component.package, component.class
        ));
        Some(Intent {
            component: Some(component),
            ..intent.clone()
        })
    }

    /// ActivityClientController.getCallingRecord:707-710: the (package, class) of the record `token` owes its result to; None when it was not started for a result, already finished, or unknown.
    pub(crate) fn calling(&self, token: &SpIBinder) -> Option<(String, String)> {
        self.machine().owed(token).map(|o| o.caller.clone())
    }

    /// ActivityClientController.activityResumed: the resume ack, logged as `activity resumed <class> at=<stamp>` before the settled change it may cause. False when no record has the token (nothing logged).
    pub(crate) fn resumed(&self, token: &SpIBinder) -> bool {
        let mut m = self.machine();
        let Some(component) = m.owner(token).map(|o| o.component.clone()) else {
            return false;
        };
        log(format_args!(
            "activity resumed {component} at={}",
            atl_diag::wall_stamp()
        ));
        let commands = m.resumed(token);
        self.note_settle(&m);
        self.send(&m, commands);
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
            atl_diag::wall_stamp()
        ));
        let commands = m.paused(token);
        self.note_settle(&m);
        self.send(&m, commands);
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
            atl_diag::wall_stamp()
        ));
        let commands = m.stopped(token);
        self.note_settle(&m);
        self.send(&m, commands);
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
            atl_diag::wall_stamp()
        ));
        let commands = m.set_visible(task, visible);
        self.note_settle(&m);
        self.send(&m, commands);
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
            atl_diag::wall_stamp()
        ));
        match m.finish(token, result) {
            Some(commands) => {
                self.note_settle(&m);
                self.send(&m, commands);
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
        // a relaunch ends with Resume at most (Machine::relaunch): no Launch, so the pid is never read
        let pid = m.owner(token).map_or(0, |o| o.pid);
        steps
            .into_iter()
            .filter_map(|step| self.item(token, pid, step))
            .map(Some)
            .collect()
    }

    /// The configuration a launch into `task` carries (launch_item: global + the Task's override; ActivityTaskSupervisor.java:1002 setLastReportedConfiguration(procConfig, overrideConfig)):
    /// what the record's `reported` starts from. A Task without a size gets no launch (item refuses it): the global one then.
    fn launch_config(&self, task: TaskId) -> Config {
        match self.tasks.size(task) {
            Some(size) => Config::from(&task_configuration(
                &self.tasks.display(),
                self.tasks.locale(),
                size,
            )),
            None => Config::from(&self.global_configuration()),
        }
    }

    /// ActivityTaskManagerService.getGlobalConfiguration (a caller's own is process_configuration).
    fn global_configuration(&self) -> Configuration {
        global_configuration(&self.tasks.display(), self.tasks.locale())
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

    /// setTaskDescription of `token` with `label`, then the title of its Task; None when no record has the token. Task.updateTaskDescription (Task.java:1919-1932,
    /// setTaskDescriptionFromActivityAboveRoot): forAllActivities walks top -> bottom and stops at the root; the first non-null label wins. Task overlays are not modelled.
    pub(crate) fn set_task_label(
        &self,
        token: &SpIBinder,
        label: Option<String>,
    ) -> Option<TaskTitle> {
        let mut m = self.machine();
        m.owner_mut(token)?.task_label = label;
        let task = m.task_of(token)?;
        let records: Vec<_> = m.records(task).collect();
        let (_, root) = records.first()?;
        Some(TaskTitle {
            task,
            label: records.iter().rev().find_map(|(_, o)| o.task_label.clone()),
            package: root.package.clone(),
            class: root.component.clone(),
        })
    }

    /// setRequestedOrientation of `token`'s record (ActivityRecord.setRequestedOrientation -> setOrientation): the request becomes its mOrientation; when the record is its Task's top it is also the
    /// Task's top orientation ([`Tasks::replace_top_orientation`]). Returns the record's Task and bounds input for the reshape check AOSP runs on every request
    /// (notifyActivityRequestedOrientationChanged -> DesktopActivityOrientationChangeHandler); None when no record has the token.
    pub(crate) fn set_requested_orientation(
        &self,
        token: &SpIBinder,
        orientation: i32,
    ) -> Option<(TaskId, ActivityInput)> {
        let mut m = self.machine();
        let owner = m.owner_mut(token)?;
        owner.orientation = orientation;
        let input = owner.bounds_input();
        let task = m.task_of(token)?;
        if m.records(task).last().is_some_and(|(top, _)| top == token) {
            self.tasks.replace_top_orientation(task, orientation);
        }
        Some((task, input))
    }

    /// ActivityClientController.getRequestedOrientation:843-850: the record's mOrientation (no app-compat override is modelled), SCREEN_ORIENTATION_UNSPECIFIED for no record.
    pub(crate) fn requested_orientation(&self, token: &SpIBinder) -> i32 {
        self.machine()
            .owner(token)
            .map_or(SCREEN_ORIENTATION_UNSPECIFIED, |o| o.orientation)
    }

    /// DesktopActivityOrientationChangeHandler's task-info branch (`:72-81`): a new top activity whose orientation differs from the previous top's is checked as a request. When `token`'s record
    /// is its Task's top and its orientation differs from the Task's top orientation at the last check, returns the Task and the record's bounds input; the check's value becomes the record's.
    /// Called at each resume (the top changes by a push or by a finish, and the new top resumes either way).
    pub(crate) fn top_orientation_changed(
        &self,
        token: &SpIBinder,
    ) -> Option<(TaskId, ActivityInput)> {
        let m = self.machine();
        let task = m.task_of(token)?;
        let (top, owner) = m.records(task).last()?;
        if top != token {
            return None;
        }
        let before = self.tasks.replace_top_orientation(task, owner.orientation);
        (before.is_some_and(|b| b != owner.orientation)).then(|| (task, owner.bounds_input()))
    }

    /// Whether a record of `task` waits for its relaunch.
    pub(crate) fn task_relaunching(&self, task: TaskId) -> bool {
        self.machine().records(task).any(|(_, o)| o.relaunching)
    }

    /// ActivityTaskSupervisor.removeTask's lifecycle half: every record of `task` finishes (Machine::remove_task). True when the shown, unstopped top must report its destroy before the
    /// Task's processes end (cleanUpRemovedTask's wait); false: they may end at once. Logged as `activity remove task <id>` before the commands it causes.
    pub(crate) fn remove_task(&self, task: TaskId) -> bool {
        let mut m = self.machine();
        let wait = m.top_visible(task);
        log(format_args!(
            "activity remove task {} wait={wait} at={}",
            task.raw(),
            atl_diag::wall_stamp()
        ));
        let commands = m.remove_task(task);
        self.note_settle(&m);
        self.send(&m, commands);
        wait
    }

    /// activityDestroyed (Machine::destroyed): the record's Task and whether it has nothing left, so its processes may end; None for no destroying record.
    pub(crate) fn destroyed(&self, token: &SpIBinder) -> Option<(TaskId, bool)> {
        self.machine().destroyed(token)
    }

    /// A record of the process of `thread` is in a Task other than `task` (Machine::holds_other_task).
    pub(crate) fn holds_other_task(&self, task: TaskId, thread: &SpIBinder) -> bool {
        self.machine()
            .holds_other_task(task, |o| o.thread.as_binder() == *thread)
    }

    /// A record of the process of `thread` is live in some Task, not finishing (Machine::holds_live): a window or an activity the user may see (uid_state::Holdings.task).
    pub(crate) fn hosts_activity(&self, thread: &SpIBinder) -> bool {
        self.machine()
            .holds_live(|o| o.thread.as_binder() == *thread)
    }

    /// The process of `thread` died: its records leave the stack (Machine::drop_owned). Some when that left no record anywhere: true when it held none that was not finishing
    /// (Machine::holds_live), the app ended itself or its Task was removed; false when it died with a live activity.
    pub(crate) fn process_died(&self, thread: &SpIBinder) -> Option<bool> {
        let mut m = self.machine();
        let of = |o: &Owner| o.thread.as_binder() == *thread;
        let finished = !m.holds_live(of);
        m.drop_owned(of);
        self.note_settle(&m);
        m.is_empty().then_some(finished)
    }
}

/// IActivityTaskManager: startActivity, getActivityClientController and getDeviceConfigurationInfo. Every other method keeps the stub's unimplemented answer (counted).
pub struct ActivityTaskService {
    atm: Arc<ActivityTasks>,
    /// the processes: a start with no caller app starts one (ActivityManager::start_without_caller)
    am: ActivityManager,
    /// StartArgs::reuse
    reuse: bool,
    /// StartArgs::results
    results: bool,
    /// ActivityTaskManagerService.GL_ES_VERSION: `ro.opengles.version` at service start, 0 (GL_ES_VERSION_UNDEFINED) when unset
    gl_es_version: i32,
}

impl ActivityTaskService {
    pub fn new(am: ActivityManager, reuse: bool, results: bool, gl_es_version: i32) -> Self {
        ActivityTaskService {
            atm: am.activity_tasks(),
            am,
            reuse,
            results,
            gl_es_version,
        }
    }

    /// `reuse` false: the test-only `--no-new-intent` (StartArgs::reuse); `results` false: the test-only `--no-activity-result` (StartArgs::results).
    pub fn new_binder(
        am: ActivityManager,
        reuse: bool,
        results: bool,
        gl_es_version: i32,
    ) -> SpIBinder {
        BnActivityTaskManager::new_binder(
            ActivityTaskService::new(am, reuse, results, gl_es_version),
            BinderFeatures::default(),
        )
        .as_binder()
    }
}

impl Interface for ActivityTaskService {}

/// ActivityTaskManagerService.assertPackageMatchesCallingUid (:2360, first step of startActivityAsUser :1278): `package` must be the calling uid's (isSameApp :2349: uid 0 and SYSTEM_UID
/// exactly pass), else SecurityException "Permission Denial: package=<p> does not belong to uid=<uid>".
fn assert_package_matches_calling_uid(
    packages: &Packages,
    uid: u32,
    package: &str,
) -> Result<(), Status> {
    if uid == ROOT_UID || uid == SYSTEM_UID || packages.is_same_app(package, uid) {
        return Ok(());
    }
    Err(security(&format!(
        "Permission Denial: package={package} does not belong to uid={uid}"
    )))
}

impl IActivityTaskManager for ActivityTaskService {
    fn r#startActivity(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        calling_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
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
        assert_package_matches_calling_uid(
            &self.atm.packages.snapshot(),
            calling_uid,
            calling_package,
        )?;
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
            resolved_type,
        };
        self.atm.start(
            caller,
            ThreadState::get_calling_pid(),
            calling_uid,
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

    fn r#getDeviceConfigurationInfo(&self) -> binder::Result<Option<ConfigurationInfo>> {
        // getGlobalConfigurationForCallingPid: the caller's process configuration
        Ok(Some(device_configuration_info(
            &process_configuration(&*self.atm.tasks, ThreadState::get_calling_pid()),
            self.gl_es_version,
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use atl_aidl::aidl::android::app::IApplicationThread::BnApplicationThread;
    use atl_parcelables::codecs::android::os::Bundle::Value;
    use atl_services_types::display_model::DisplayModel;

    use super::*;
    use crate::tasks::TaskSize;

    const PACKAGE: &str = "com.example.player";
    const LAUNCHER: &str = "com.example.player.Main";
    const VIDEO: &str = "com.example.player.Video";
    const UID: u32 = 10_123;
    /// the app process's pid: any value, the tests' app runs one process
    const PID: i32 = 4242;
    /// the request code of a startActivityForResult
    const REQUEST_CODE: i32 = 1131;

    /// Keeps every transaction scheduled on it, in order.
    struct Thread {
        scheduled: Arc<Mutex<Vec<ClientTransaction>>>,
    }

    impl Interface for Thread {}

    impl IApplicationThread for Thread {
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
    fn decl(name: &str) -> ActivityInfo {
        crate::packages::fixture::activity(PACKAGE, name)
    }

    /// One installed package: its launcher and one video activity.
    fn fixture_atm() -> ActivityTasks {
        let mut record = crate::packages::fixture::record(PACKAGE);
        record.uid = UID;
        record.info.activities = Some(vec![Some(decl(LAUNCHER)), Some(decl(VIDEO))]);
        let packages = Arc::new(PackageTable::new(Packages::new(
            vec![record],
            BTreeMap::new(),
        )));
        ActivityTasks::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages.snapshot(), &[])),
            Arc::new(Tasks::new(DisplayModel::default(), None, None).expect("the default display")),
            token::new(),
        )
    }

    /// R7-9: ActivityTaskManagerService.java:2349-2368: the calling package must be the calling uid's; uid 0 and SYSTEM_UID (1000) exactly pass; another app's uid, or SYSTEM_UID in user 1
    /// (101000: isSameApp compares the uid, not its app id), gets SecurityException with AOSP's message.
    #[test]
    fn calling_package_must_be_the_callers() {
        let atm = fixture_atm();
        let other = UID + 1;
        assert!(assert_package_matches_calling_uid(&atm.packages.snapshot(), UID, PACKAGE).is_ok());
        for uid in [0, 1000] {
            assert!(
                assert_package_matches_calling_uid(&atm.packages.snapshot(), uid, PACKAGE).is_ok()
            );
        }
        for uid in [other, 101_000] {
            let e = assert_package_matches_calling_uid(&atm.packages.snapshot(), uid, PACKAGE)
                .unwrap_err();
            assert_eq!(e.exception_code(), binder::ExceptionCode::SECURITY);
            let want = format!("Permission Denial: package={PACKAGE} does not belong to uid={uid}");
            assert!(e.get_description().contains(&want), "{e:?}");
        }
    }

    /// `am start` (atl-am): the shell uid names com.android.shell, which a device has installed under sharedUserId android.uid.shell, so isSameApp passes as there; an app naming it does not.
    #[test]
    fn shell_start_passes_as_the_shell_package() {
        const SHELL: &str = "com.android.shell";
        let atm = fixture_atm();
        let shell_uid = atl_services_types::permission::SHELL_UID;
        assert!(
            assert_package_matches_calling_uid(&atm.packages.snapshot(), shell_uid, SHELL).is_ok()
        );
        assert!(
            assert_package_matches_calling_uid(&atm.packages.snapshot(), shell_uid, PACKAGE)
                .is_err()
        );
        assert!(assert_package_matches_calling_uid(&atm.packages.snapshot(), UID, SHELL).is_err());
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
        atm.launch_launcher(thread.clone(), PID, UID, PACKAGE, &decl(LAUNCHER), task)
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

    /// ActivityTaskSupervisor.java:1001: the launch carries the activity's merged override = its freeform Task's. TaskFragment.computeConfigResourceOverrides: a freeform Task's bounds and
    /// appBounds are the Task's own (here from the origin); maxBounds is inherited from DisplayContent.computeScreenConfiguration:2488 (the display; ActivityRecord sandboxes it only for
    /// size-compat); WINDOWING_MODE_FREEFORM = 5 (WindowConfiguration.java:113). Values: a 562x900 Task on a 1920x1200 display.
    #[test]
    fn launch_carries_the_task_override() {
        let mut record = crate::packages::fixture::record(PACKAGE);
        record.uid = UID;
        record.info.activities = Some(vec![Some(decl(LAUNCHER))]);
        let packages = Arc::new(PackageTable::new(Packages::new(
            vec![record],
            BTreeMap::new(),
        )));
        let display = DisplayModel {
            width: 1920,
            height: 1200,
            ..DisplayModel::default()
        };
        let atm = ActivityTasks::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages.snapshot(), &[])),
            Arc::new(Tasks::new(display, None, None).expect("the test display")),
            token::new(),
        );
        let (thread, scheduled) = app_thread();
        let task = atm.tasks.new_task(
            UID,
            PACKAGE,
            TaskSize {
                width: 562,
                height: 900,
            },
        );
        atm.launch_launcher(thread, PID, UID, PACKAGE, &decl(LAUNCHER), task);
        let over = scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .find_map(|item| match item {
                ClientTransactionItem::LaunchActivity(launch) => launch.override_config.clone(),
                _ => None,
            })
            .expect("a LaunchActivityItem with an override");
        let rect = |right, bottom| atl_parcelables::parts::Rect {
            left: 0,
            top: 0,
            right,
            bottom,
        };
        let w = over.window_configuration;
        assert_eq!(
            (w.bounds, w.app_bounds, w.max_bounds, w.windowing_mode),
            (rect(562, 900), Some(rect(562, 900)), rect(1920, 1200), 5)
        );
    }

    /// preBindApplication + prepareConfigurationForLaunchingActivity: a process started for a launch binds with its Task's configuration and its launch's curConfig is the same; a process
    /// started for no launch binds with the display's. Values: 320 dpi = density 2, so a 562x900 Task is 281x450 dp portrait (1), the 1920x1200 display 960x600 dp landscape (2).
    #[test]
    fn bind_and_launch_carry_the_process_configuration_of_its_task() {
        let mut record = crate::packages::fixture::record(PACKAGE);
        record.uid = UID;
        record.info.activities = Some(vec![Some(decl(LAUNCHER))]);
        let packages = Arc::new(PackageTable::new(Packages::new(
            vec![record],
            BTreeMap::new(),
        )));
        let display = DisplayModel {
            width: 1920,
            height: 1200,
            dpi: 320,
            ..DisplayModel::default()
        };
        let atm = ActivityTasks::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages.snapshot(), &[])),
            Arc::new(Tasks::new(display, None, None).expect("the test display")),
            token::new(),
        );
        let task = atm.tasks.new_task(
            UID,
            PACKAGE,
            TaskSize {
                width: 562,
                height: 900,
            },
        );
        let dp = |c: &Configuration| (c.screen_width_dp, c.screen_height_dp, c.orientation);
        assert_eq!(dp(&atm.bind_configuration(PID + 1, None)), (960, 600, 2));
        let bound = atm.bind_configuration(PID, Some(task));
        assert_eq!(dp(&bound), (281, 450, 1));
        let (thread, scheduled) = app_thread();
        atm.launch_launcher(thread, PID, UID, PACKAGE, &decl(LAUNCHER), task);
        let cur = scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .find_map(|item| match item {
                ClientTransactionItem::LaunchActivity(launch) => launch.cur_config.clone(),
                _ => None,
            })
            .expect("a LaunchActivityItem with a curConfig");
        assert_eq!(dp(&cur), (281, 450, 1));
        assert_eq!(atm.tasks.process_task(PID), Some(task));
        // the launch carried it: no ConfigurationChangeItem of its own (ActivityTaskSupervisor.java:838-846)
        let changes = scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .filter(|item| matches!(item, ClientTransactionItem::ConfigurationChange(_)))
            .count();
        assert_eq!(changes, 0);
    }

    /// The start an app's startActivity carries (ZVideoPlayer: setClass + putExtra, no action, no flags) is the intent the fresh launch of its activity delivers: the caller's fields, extras included.
    #[test]
    fn fresh_start_delivers_the_caller_intent() {
        let atm = fixture_atm();
        let (thread, scheduled) = app_thread();
        let source = attach_launcher(&atm, &thread);
        let pairs = [("path", "clip.mp4"), ("mute", "true")];
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
                PID,
                UID,
                PACKAGE,
                StartArgs {
                    intent: Some(&caller_intent),
                    reuse: true,
                    results: true,
                    request_code: REQUEST_CODE,
                    result_to: Some(&source),
                    result_who: None,
                    resolved_type: None,
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

    /// Activity.requestPermissions' who (Activity.java:807 REQUEST_PERMISSIONS_WHO_PREFIX) and PackageManager.buildRequestPermissionsIntent's start for POST_NOTIFICATIONS.
    const PERMISSIONS_WHO: &str = "@android:requestPermissions:";
    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";

    /// The REQUEST_PERMISSIONS start from the resumed launcher; the results the app thread got for it (request code, result code, data).
    fn request_permissions(atm: &ActivityTasks) -> (i32, Vec<(i32, i32, Option<Intent>)>) {
        let (thread, scheduled) = app_thread();
        let source = attach_launcher(atm, &thread);
        atm.resumed(&source);
        let request = Intent {
            action: Some(permission_request::ACTION_REQUEST_PERMISSIONS.into()),
            extras: Some(Bundle::from_entries([(
                permission_request::EXTRA_NAMES.to_owned(),
                Value::StringArray(Some(vec![Some(POST_NOTIFICATIONS.into())])),
            )])),
            ..Default::default()
        };
        let status = atm
            .start(
                &thread,
                PID,
                UID,
                PACKAGE,
                StartArgs {
                    intent: Some(&request),
                    reuse: true,
                    results: true,
                    request_code: REQUEST_CODE,
                    result_to: Some(&source),
                    result_who: Some(PERMISSIONS_WHO),
                    resolved_type: None,
                },
            )
            .expect("start");
        let results = scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .filter_map(|item| match item {
                ClientTransactionItem::ActivityResult(r) => Some(r.result_infos.clone()),
                _ => None,
            })
            .flatten()
            .flatten()
            .flatten()
            .inspect(|r| assert_eq!(r.result_who.as_deref(), Some(PERMISSIONS_WHO)))
            .map(|r| (r.request_code, r.result_code, r.data))
            .collect();
        (status, results)
    }

    /// P1 fires with no permission controller installed: the start succeeds (the dialog would have started) and the owed result is GrantPermissionsActivity's dismissal: RESULT_OK (-1) with
    /// the requested name and its current state, PERMISSION_DENIED (-1) for a runtime permission never granted.
    #[test]
    fn permission_request_without_a_controller_is_answered_as_dismissed() {
        let atm = fixture_atm();
        let (status, results) = request_permissions(&atm);
        assert_eq!(status, activity_start::START_SUCCESS);
        let [(code, result, Some(data))] = results.as_slice() else {
            panic!("one result with data, got {results:?}");
        };
        assert_eq!((*code, *result), (REQUEST_CODE, -1));
        let extras: BTreeMap<String, Value> = data
            .extras
            .as_ref()
            .expect("extras")
            .entries()
            .expect("entries")
            .into_iter()
            .collect();
        assert_eq!(
            extras.get(permission_request::EXTRA_NAMES),
            Some(&Value::StringArray(Some(vec![Some(
                POST_NOTIFICATIONS.into()
            )])))
        );
        assert_eq!(
            extras.get(permission_request::EXTRA_RESULTS),
            Some(&Value::IntArray(Some(vec![-1])))
        );
    }

    /// With a resolvable controller (a privileged system package resolving MANAGE_PERMISSIONS) P1 stays out of the way: the start takes the normal path, which refuses the implicit intent
    /// (START_INTENT_NOT_RESOLVED, -91) and cancels the owed result (RESULT_CANCELED 0, no data) until runtime-permissions S5 resolves it.
    #[test]
    fn permission_request_with_a_controller_takes_the_normal_start() {
        let controller = "com.android.permissioncontroller";
        let class = "com.android.permissioncontroller.permission.ui.ManagePermissionsActivity";
        let mut c = crate::packages::fixture::record(controller);
        c.uid = 10_200;
        c.info.activities = Some(vec![Some(crate::packages::fixture::activity(
            controller, class,
        ))]);
        c.intents.push(crate::installed::ComponentIntents {
            kind: atl_services_types::installer_output::Kind::ActivityIntents,
            name: class.to_owned(),
            intents: vec![crate::packages::fixture::filter(
                &["android.intent.action.MANAGE_PERMISSIONS"],
                &["android.intent.category.DEFAULT"],
            )],
        });
        if let Some(a) = c.info.application_info.as_mut() {
            // FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED (ApplicationInfo.java:189, :535)
            a.flags = 1;
            a.private_flags = 1 << 3;
        }
        let mut app = crate::packages::fixture::record(PACKAGE);
        app.uid = UID;
        app.info.activities = Some(vec![Some(decl(LAUNCHER))]);
        let packages = Arc::new(PackageTable::new(Packages::new(
            vec![app, c],
            BTreeMap::new(),
        )));
        assert_eq!(
            packages.snapshot().permission_controller(),
            Some(controller)
        );
        let atm = ActivityTasks::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages.snapshot(), &[])),
            Arc::new(Tasks::new(DisplayModel::default(), None, None).expect("the default display")),
            token::new(),
        );
        let (status, results) = request_permissions(&atm);
        assert_eq!(status, activity_start::START_INTENT_NOT_RESOLVED);
        assert!(
            matches!(results.as_slice(), [(REQUEST_CODE, 0, None)]),
            "{results:?}"
        );
    }

    /// getDeviceConfigurationInfo over the global configuration: input fields undefined (0) until the platform layer supplies input -> no features; the GL ES version passes through
    /// (0x30001 = ES 3.1). Then AOSP's derivation on a configuration with a QWERTY keyboard (2) and a d-pad (2): both features; NOKEYS (1) and NONAV (1) give none (the break).
    #[test]
    fn device_configuration_info_follows_the_global_configuration() {
        let atm = fixture_atm();
        let info = device_configuration_info(&atm.global_configuration(), 0x30001);
        assert_eq!(
            info,
            ConfigurationInfo {
                req_gl_es_version: 0x30001,
                ..ConfigurationInfo::default()
            }
        );
        let keys = Configuration {
            touchscreen: 3,
            keyboard: 2,
            navigation: NAVIGATION_DPAD,
            ..Configuration::default()
        };
        let info = device_configuration_info(&keys, 0x20000);
        assert_eq!(
            (
                info.req_touch_screen,
                info.req_keyboard_type,
                info.req_navigation
            ),
            (3, 2, NAVIGATION_DPAD)
        );
        assert_eq!(info.req_input_features, 0b11);
        assert_eq!(info.req_gl_es_version, 0x20000);
        let none = Configuration {
            keyboard: KEYBOARD_NOKEYS,
            navigation: 1,
            ..Configuration::default()
        };
        assert_eq!(device_configuration_info(&none, 0).req_input_features, 0);
    }

    const ACTION_VIEW: &str = "android.intent.action.VIEW";
    const CATEGORY_DEFAULT: &str = "android.intent.category.DEFAULT";
    const OTHER: &str = "com.example.browser";

    /// `class` of `package` with one filter: ACTION_VIEW, CATEGORY_DEFAULT (what startActivity's MATCH_DEFAULT_ONLY requires).
    fn viewer(class: &str) -> crate::installed::ComponentIntents {
        crate::installed::ComponentIntents {
            kind: atl_services_types::installer_output::Kind::ActivityIntents,
            name: class.to_owned(),
            intents: vec![crate::packages::fixture::filter(
                &[ACTION_VIEW],
                &[CATEGORY_DEFAULT],
            )],
        }
    }

    /// The player (targetSdk `target_sdk`) with VIDEO filtering ACTION_VIEW, and with `other` a second package whose one activity filters it too.
    fn filtered_atm(other: bool, target_sdk: i32) -> ActivityTasks {
        let mut record = crate::packages::fixture::record(PACKAGE);
        record.uid = UID;
        if let Some(app) = record.info.application_info.as_mut() {
            app.target_sdk_version = target_sdk;
        }
        record.info.activities = Some(vec![Some(decl(LAUNCHER)), Some(decl(VIDEO))]);
        record.intents = vec![viewer(VIDEO)];
        let mut records = vec![record];
        if other {
            let class = format!("{OTHER}.View");
            let mut o = crate::packages::fixture::record(OTHER);
            o.uid = UID + 1;
            o.info.activities = Some(vec![Some(crate::packages::fixture::activity(
                OTHER, &class,
            ))]);
            o.intents = vec![viewer(&class)];
            records.push(o);
        }
        let packages = Arc::new(PackageTable::new(Packages::new(records, BTreeMap::new())));
        ActivityTasks::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages.snapshot(), &[])),
            Arc::new(Tasks::new(DisplayModel::default(), None, None).expect("the default display")),
            token::new(),
        )
    }

    /// startActivity(ForResult) of `intent` from `source`.
    fn start_of(
        atm: &ActivityTasks,
        thread: &Strong<dyn IApplicationThread>,
        source: &SpIBinder,
        intent: &Intent,
        request_code: i32,
    ) -> i32 {
        atm.start(
            thread,
            PID,
            UID,
            PACKAGE,
            StartArgs {
                intent: Some(intent),
                reuse: true,
                results: true,
                request_code,
                result_to: Some(source),
                result_who: None,
                resolved_type: None,
            },
        )
        .expect("start")
    }

    fn view() -> Intent {
        Intent {
            action: Some(ACTION_VIEW.into()),
            ..Default::default()
        }
    }

    /// The token of the record the LaunchActivityItem for `class` launched.
    fn token_of(scheduled: &Mutex<Vec<ClientTransaction>>, class: &str) -> SpIBinder {
        scheduled
            .lock()
            .expect("transactions lock")
            .iter()
            .flat_map(|t| t.items.iter().flatten())
            .find_map(|item| match item {
                ClientTransactionItem::LaunchActivity(l)
                    if l.intent
                        .as_ref()
                        .and_then(|i| i.component.as_ref())
                        .is_some_and(|c| c.class == class) =>
                {
                    l.activity_token.clone()
                }
                _ => None,
            })
            .expect("a LaunchActivityItem for the class")
    }

    /// An implicit intent one activity of the caller's own package filters starts that activity (ActivityTaskSupervisor.resolveIntent): the launch carries the intent made explicit
    /// (resolveActivity stores the component, :712-716) and keeps its action.
    #[test]
    fn an_implicit_intent_of_the_own_package_starts_the_resolved_activity() {
        let atm = filtered_atm(false, SDK_U);
        let (thread, scheduled) = app_thread();
        let source = attach_launcher(&atm, &thread);
        assert_eq!(
            start_of(&atm, &thread, &source, &view(), -1),
            activity_start::START_SUCCESS
        );
        let launched = intent_of(&scheduled, VIDEO);
        assert_eq!(launched.action.as_deref(), Some(ACTION_VIEW));
        assert_eq!(
            launched.component.map(|c| c.package),
            Some(PACKAGE.to_owned())
        );
    }

    /// No activity matches (Doodle Jump's `market://` VIEW, an `https://` VIEW with no browser): START_INTENT_NOT_RESOLVED -> ActivityNotFoundException in the app, as AOSP.
    #[test]
    fn an_implicit_intent_nothing_matches_is_not_resolved() {
        let atm = filtered_atm(false, SDK_U);
        let (thread, _) = app_thread();
        let source = attach_launcher(&atm, &thread);
        for data in ["market://details?id=x", "https://example.com/"] {
            let market = Intent {
                data: Some(data.into()),
                ..view()
            };
            assert_eq!(
                start_of(&atm, &thread, &source, &market, -1),
                activity_start::START_INTENT_NOT_RESOLVED,
                "{data}"
            );
        }
    }

    /// Another package filtering the same action. Caller targetSdk >= R without `<queries>`: the other package is invisible (AppsFilter, M76), so the own activity is the only match and
    /// starts; pinned to the other package, nothing matches. Caller targetSdk < R: both are visible and tie -> null (M76: no ResolverActivity) -> not resolved; pinned to the other
    /// package, it resolves and the existing cross-package rule refuses it (START_CANCELED, "another package's activity").
    #[test]
    fn an_implicit_intent_of_another_package_keeps_the_cross_package_refusal() {
        let to_other = Intent {
            package: Some(OTHER.into()),
            ..view()
        };
        for (sdk, open, pinned) in [
            (
                SDK_U,
                activity_start::START_SUCCESS,
                activity_start::START_INTENT_NOT_RESOLVED,
            ),
            (
                SDK_Q,
                activity_start::START_INTENT_NOT_RESOLVED,
                activity_start::START_CANCELED,
            ),
        ] {
            let atm = filtered_atm(true, sdk);
            let (thread, _) = app_thread();
            let source = attach_launcher(&atm, &thread);
            assert_eq!(
                start_of(&atm, &thread, &source, &to_other, -1),
                pinned,
                "sdk {sdk} pinned"
            );
            assert_eq!(
                start_of(&atm, &thread, &source, &view(), -1),
                open,
                "sdk {sdk}"
            );
        }
    }

    /// Build.VERSION_CODES.UPSIDE_DOWN_CAKE (34, the fixture's) and Q (29, below R = 30 where package visibility filtering starts).
    const SDK_U: i32 = 34;
    const SDK_Q: i32 = 29;

    /// getCallingActivity / getCallingPackage (ActivityClientController.getCallingRecord): the caller's component for a start for result, null for a plain start, null once the child finished.
    #[test]
    fn the_calling_activity_is_the_result_caller() {
        let atm = fixture_atm();
        let (thread, scheduled) = app_thread();
        let source = attach_launcher(&atm, &thread);
        let video = Intent {
            component: Some(ComponentName {
                package: PACKAGE.to_owned(),
                class: VIDEO.to_owned(),
            }),
            ..Default::default()
        };
        assert_eq!(
            start_of(&atm, &thread, &source, &video, REQUEST_CODE),
            activity_start::START_SUCCESS
        );
        let child = token_of(&scheduled, VIDEO);
        assert_eq!(
            atm.calling(&child),
            Some((PACKAGE.to_owned(), LAUNCHER.to_owned()))
        );
        assert_eq!(
            atm.calling(&source),
            None,
            "the launcher was not started for a result"
        );
        atm.finish(&child, RESULT_CANCELED, None);
        assert_eq!(atm.calling(&child), None, "the child finished");
    }

    /// A resultTo naming no record (an ad SDK's start from a token that is not an activity of this run) with a request code: ActivityStarter.executeRequest leaves sourceRecord and
    /// resultRecord null (:1104-1120), NEW_TASK is forced (:2885-2891) and findTask puts the activity on the caller's Task of the package affinity (:2972-3000, :2595-2602): START_SUCCESS,
    /// launched in the launcher's Task, owing no result (getCallingActivity null). Before this, ATL refused it START_CANCELED ("resultTo names no activity").
    #[test]
    fn a_start_with_a_stale_result_to_goes_into_the_callers_task() {
        let atm = fixture_atm();
        let (thread, scheduled) = app_thread();
        let launcher = attach_launcher(&atm, &thread);
        let video = Intent {
            component: Some(ComponentName {
                package: PACKAGE.to_owned(),
                class: VIDEO.to_owned(),
            }),
            ..Default::default()
        };
        let stale = token::new();
        assert_eq!(
            start_of(&atm, &thread, &stale, &video, REQUEST_CODE),
            activity_start::START_SUCCESS
        );
        let child = token_of(&scheduled, VIDEO);
        assert_eq!(atm.calling(&child), None, "no result is owed");
        let m = atm.machine();
        assert_eq!(
            m.task_of(&child),
            m.task_of(&launcher),
            "the launcher's Task"
        );
    }
}
