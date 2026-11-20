// SPDX-License-Identifier: MIT
//! What the services do for a startActivity call: ActivityStarter's decision as a pure function ([`decide`], no binder), so the tests build without the converter feature.
//! An implicit intent arrives here already resolved by the package manager (activity_task: ActivityTaskSupervisor.resolveIntent), so `component` is None only when nothing matched.
//! Accepted: an explicit component of the caller's own package, in the caller's process, into one existing Task: the source's, or with no source activity (resultTo null or naming no record:
//! NEW_TASK forced) the caller's Task of the activity's affinity. Inside that Task the decision follows ActivityStarter.startActivityInner's order: FORWARD_RESULT, the explicit NEW_TASK's
//! result cancel, the reusable task, complyActivityFlags (NEW_TASK|CLEAR_TASK, CLEAR_TOP / singleTask, REORDER_TO_FRONT, the root's class), deliverToCurrentTopIfNeeded, else a push ([`Plan`]).
//! A shell / home start (no caller app) of a package's launcher whose process is not running is [`check_shell`].
//! Every other call is refused: a SecurityException when the caller does not own the activity it names (fail-closed; counted `exception:SECURITY` by the reply hook, not here), otherwise the START code
//! AOSP returns for the case (ActivityManager.java:162 FIRST_START_FATAL = -100), counted by `atl_diag::refused` under its reason. Refusals are never silent: the app sees the code, the log sees the count.
//! Refused, not modelled: implicit intents of the shell path, a chooser tie (no ResolverActivity), other packages, singleInstance(PerTask), other processes, and every start that needs a second Task
//! (NEW_TASK into another affinity, NEW_TASK|MULTIPLE_TASK, NEW_DOCUMENT, LAUNCH_ADJACENT with NEW_TASK, no source and no Task of the affinity), task reset, bringing a running app's task to front.

use atl_services_types::permission::{SHELL_UID, SYSTEM_UID};

/// What a start's check reads of the activity it starts (the installed ActivityInfo's, crate::packages::declared): its class, launch mode, process, task affinity and noHistory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    /// ActivityInfo.launchMode
    pub launch_mode: i32,
    /// ComponentInfo.processName (the parse fills it: the component's, else the package's)
    pub process: Option<String>,
    /// ActivityInfo.taskAffinity; None = the package name
    pub task_affinity: Option<String>,
    /// ActivityInfo.FLAG_NO_HISTORY (manifest android:noHistory)
    pub no_history: bool,
}

/// ActivityManager.START_SUCCESS: the activity is started (the record is pushed and its launch sent).
pub const START_SUCCESS: i32 = 0;
/// ActivityManager.START_DELIVERED_TO_TOP: an existing instance got the intent (deliverToCurrentTopIfNeeded, or recycleTask in a Task already in front); a success code, Instrumentation does not throw.
pub const START_DELIVERED_TO_TOP: i32 = 3;
/// ActivityManager.START_CANCELED: Instrumentation.execStartActivity throws "Activity could not be started" (Instrumentation.java:2452).
pub const START_CANCELED: i32 = -96;
/// ActivityManager.START_FORWARD_AND_REQUEST_CONFLICT (FIRST_START_FATAL_ERROR_CODE + 7): FORWARD_RESULT with a request code (ActivityStarter.java:1126-1128).
pub const START_FORWARD_AND_REQUEST_CONFLICT: i32 = -93;
/// ActivityManager.START_CLASS_NOT_FOUND: the explicit component's class is not in the package.
pub const START_CLASS_NOT_FOUND: i32 = -92;
/// ActivityManager.START_INTENT_NOT_RESOLVED: no activity matches the implicit intent.
pub const START_INTENT_NOT_RESOLVED: i32 = -91;

/// Intent.FLAG_ACTIVITY_* bits the decision reads (Intent.java).
const NEW_TASK: i32 = 0x1000_0000;
const SINGLE_TOP: i32 = 0x2000_0000;
const NO_HISTORY: i32 = 0x4000_0000;
const MULTIPLE_TASK: i32 = 0x0800_0000;
const CLEAR_TOP: i32 = 0x0400_0000;
const FORWARD_RESULT: i32 = 0x0200_0000;
const PREVIOUS_IS_TOP: i32 = 0x0100_0000;
const RESET_TASK_IF_NEEDED: i32 = 0x0020_0000;
const NO_USER_ACTION: i32 = 0x0004_0000;
const REORDER_TO_FRONT: i32 = 0x0002_0000;
const CLEAR_TASK: i32 = 0x0000_8000;
const LAUNCH_ADJACENT: i32 = 0x0000_1000;
/// Bits the decision models (some only in their single-Task cases, checked in `decide`).
const MODELLED: i32 = NEW_TASK
    | SINGLE_TOP
    | NO_HISTORY
    | MULTIPLE_TASK
    | CLEAR_TOP
    | FORWARD_RESULT
    | PREVIOUS_IS_TOP
    | RESET_TASK_IF_NEEDED
    | NO_USER_ACTION
    | REORDER_TO_FRONT
    | CLEAR_TASK
    | LAUNCH_ADJACENT;
/// Bits with no effect on which record gets the intent, what finishes or the result, accepted and ignored: EXCLUDE_FROM_RECENTS 0x00800000, BROUGHT_TO_FRONT 0x00400000 and LAUNCHED_FROM_HISTORY
/// 0x00100000 (set by the system, read by no start decision; apps resend intents they received), NO_ANIMATION 0x00010000 (transition only, ActivityStarter.java:2831), TASK_ON_HOME 0x00004000 (Back
/// from the Task's root goes home: ATL has no home), RETAIN_IN_RECENTS 0x00002000 (recents only: ATL has no recents list), MATCH_EXTERNAL 0x00000800 (instant apps), and 0x1-0x100: URI grants,
/// FROM_BACKGROUND, DEBUG_LOG_RESOLUTION, stopped-package filters, DIRECT_BOOT_AUTO (not activity placement; a same-package start needs no grant).
const IGNORED: i32 = 0x0080_0000
    | 0x0040_0000
    | 0x0010_0000
    | 0x0001_0000
    | 0x0000_4000
    | 0x0000_2000
    | 0x0000_0800
    | 0x0000_01ff;
/// Every named FLAG_ACTIVITY_* bit, for the refusal log line ([`flag_names`]).
const NAMES: &[(i32, &str)] = &[
    (NO_HISTORY, "NO_HISTORY"),
    (SINGLE_TOP, "SINGLE_TOP"),
    (NEW_TASK, "NEW_TASK"),
    (MULTIPLE_TASK, "MULTIPLE_TASK"),
    (CLEAR_TOP, "CLEAR_TOP"),
    (FORWARD_RESULT, "FORWARD_RESULT"),
    (PREVIOUS_IS_TOP, "PREVIOUS_IS_TOP"),
    (0x0080_0000, "EXCLUDE_FROM_RECENTS"),
    (0x0040_0000, "BROUGHT_TO_FRONT"),
    (RESET_TASK_IF_NEEDED, "RESET_TASK_IF_NEEDED"),
    (0x0010_0000, "LAUNCHED_FROM_HISTORY"),
    (0x0008_0000, "NEW_DOCUMENT"),
    (NO_USER_ACTION, "NO_USER_ACTION"),
    (REORDER_TO_FRONT, "REORDER_TO_FRONT"),
    (0x0001_0000, "NO_ANIMATION"),
    (CLEAR_TASK, "CLEAR_TASK"),
    (0x0000_4000, "TASK_ON_HOME"),
    (0x0000_2000, "RETAIN_IN_RECENTS"),
    (LAUNCH_ADJACENT, "LAUNCH_ADJACENT"),
    (0x0000_0800, "MATCH_EXTERNAL"),
    (0x0000_0400, "REQUIRE_NON_BROWSER"),
    (0x0000_0200, "REQUIRE_DEFAULT"),
];
/// ActivityInfo.LAUNCH_MULTIPLE: `standard`, the launch mode a manifest without `android:launchMode` gets; LAUNCH_SINGLE_TOP / LAUNCH_SINGLE_TASK.
const LAUNCH_MODE_STANDARD: i32 = 0;
const LAUNCH_SINGLE_TOP: i32 = 1;
const LAUNCH_SINGLE_TASK: i32 = 2;
const INTERFACE: &str = "IActivityTaskManager";

/// `bits` as FLAG_ACTIVITY_* names joined by `|`; an unnamed bit as hex.
pub fn flag_names(bits: i32) -> String {
    let named: i32 = NAMES.iter().map(|(b, _)| b).fold(0, |a, b| a | b);
    let mut out: Vec<String> = NAMES
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| (*n).to_owned())
        .collect();
    if bits & !named != 0 {
        out.push(format!("0x{:x}", bits & !named));
    }
    out.join("|")
}

/// Why a start is refused. `Security` is thrown as a SecurityException; `Code` is returned as the startActivity result; `flags`: the intent flag bits the refusal is about (0: none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Security(&'static str),
    Code {
        code: i32,
        reason: &'static str,
        flags: i32,
    },
}

/// The activity resultTo names, as the services know its owner: the owner's uid and package, and the affinity its task has (its manifest's taskAffinity, else the package name).
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub uid: u32,
    pub package: &'a str,
    pub affinity: &'a str,
    /// ActivityRecord.finishing: no result is owed to a finishing caller (ActivityStarter.java:1112).
    pub finishing: bool,
    /// The source owes its own caller a result (ActivityRecord.resultTo): what FORWARD_RESULT moves to the new record.
    pub owes: bool,
}

/// One startActivity call, as the binder interface delivers it.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub calling_uid: u32,
    pub calling_package: &'a str,
    pub request_code: i32,
    pub flags: i32,
    /// The intent's explicit component: (package, class name). None for an implicit intent.
    pub component: Option<(&'a str, &'a str)>,
    /// resultTo's activity, when the caller's thread runs it. None: resultTo is null (a start from a non-activity Context) or names no record, e.g. a finished activity's token
    /// (ActivityRecord.isInAnyTask null, ActivityStarter.java:1106-1107): not an error, the start has no source and owes no result.
    pub source: Option<Source<'a>>,
}

impl Call<'_> {
    /// FLAG_ACTIVITY_FORWARD_RESULT with a source record: the source's result target moves to the new record (ActivityStarter.java:1123-1136).
    pub fn forwards(&self) -> bool {
        self.flags & FORWARD_RESULT != 0 && self.source.is_some()
    }

    /// The start has a result record (ActivityStarter.executeRequest, :1106-1136): forwarded, the source's own; else requestCode >= 0 and the source not finishing.
    pub fn result_requested(&self) -> bool {
        if self.forwards() {
            self.request_code < 0 && self.source.is_some_and(|s| s.owes)
        } else {
            self.request_code >= 0 && self.source.is_some_and(|s| !s.finishing)
        }
    }

    /// An explicit NEW_TASK in the intent cancels the owed result before the start is decided (sendNewTaskResultRequestIfNeeded, ActivityStarter.java:2718, 2846-2849).
    pub fn new_task_cancels_result(&self) -> bool {
        self.result_requested() && self.flags & NEW_TASK != 0
    }

    /// The result still owed when the start is decided: what the reusable-task search depends on (ActivityStarter.java:2978).
    pub fn result_at_reuse(&self) -> bool {
        self.result_requested() && !self.new_task_cancels_result()
    }
}

/// One record of the Task the start goes into, as the decision reads it, bottom -> top.
#[derive(Debug, Clone, Copy)]
pub struct Rec<'a> {
    /// Its activity class.
    pub class: &'a str,
    /// ActivityRecord.finishing
    pub finishing: bool,
    /// It is the start's source record.
    pub source: bool,
}

/// The package the component's class is looked up in, and the Task the start goes into.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The calling package's process (`<application android:process>`, else the package name).
    pub process: &'a str,
    pub decl: Option<&'a Declared>,
    /// The Task: the source's; with no source the caller's Task whose root has the activity's affinity (findTask). None with no source and no such Task.
    pub task: Option<&'a [Rec<'a>]>,
}

/// What an accepted start does to its Task: `finish` first, then `then`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Records the start finishes (indices into [`Target::task`]), in AOSP's order: clear-top top first (finishActivityAbove), clear-task bottom first (removeActivities).
    /// Each answers its own owed result RESULT_CANCELED.
    pub finish: Vec<usize>,
    pub then: Then,
    /// The new record is no-history (FLAG_ACTIVITY_NO_HISTORY or manifest noHistory).
    pub no_history: bool,
    /// false: FLAG_ACTIVITY_NO_USER_ACTION, the record the start covers pauses without onUserLeaveHint.
    pub user_leaving: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    /// A new record on top of the Task, owing the start's result.
    Push,
    /// The record `to` gets the intent (onNewIntent). `to_front`: moved to the top first (REORDER_TO_FRONT). `cancel`: the start's owed result is answered RESULT_CANCELED first
    /// (deliverToCurrentTopIfNeeded, :2469); false: recycleTask's reuse sends none, the result is never answered.
    Deliver {
        to: usize,
        to_front: bool,
        cancel: bool,
    },
}

impl Plan {
    /// The START code the call returns.
    pub fn code(&self) -> i32 {
        match self.then {
            Then::Push => START_SUCCESS,
            Then::Deliver { .. } => START_DELIVERED_TO_TOP,
        }
    }
}

fn refused(code: i32, reason: &'static str, flags: i32) -> Refusal {
    atl_diag::refused(INTERFACE, &format!("startActivity ({reason})"));
    Refusal::Code {
        code,
        reason,
        flags,
    }
}

/// Decides one start (ActivityStarter.executeRequest's checks, then startActivityInner's order). Ok: the declaration of the activity to start and what to do in the Task.
pub fn decide<'a>(call: &Call<'_>, target: Target<'a>) -> Result<(&'a Declared, Plan), Refusal> {
    if let Some(source) = call.source
        && (source.uid != call.calling_uid || source.package != call.calling_package)
    {
        return Err(Refusal::Security("resultTo is not the caller's activity"));
    }
    if call.forwards() && call.request_code >= 0 {
        return Err(refused(
            START_FORWARD_AND_REQUEST_CONFLICT,
            "FORWARD_RESULT with a request code",
            FORWARD_RESULT,
        ));
    }
    let Some((package, class)) = call.component else {
        return Err(refused(
            START_INTENT_NOT_RESOLVED,
            "implicit intent (no activity matches)",
            0,
        ));
    };
    if package != call.calling_package {
        return Err(refused(START_CANCELED, "another package's activity", 0));
    }
    let Some(decl) = target.decl.filter(|d| d.name == class) else {
        return Err(refused(
            START_CLASS_NOT_FOUND,
            "class not declared by the package",
            0,
        ));
    };
    if !matches!(
        decl.launch_mode,
        LAUNCH_MODE_STANDARD | LAUNCH_SINGLE_TOP | LAUNCH_SINGLE_TASK
    ) {
        return Err(refused(
            START_CANCELED,
            "launch mode singleInstance / singleInstancePerTask",
            0,
        ));
    }
    let single_task = decl.launch_mode == LAUNCH_SINGLE_TASK;
    // computeLaunchingTaskFlags (ActivityStarter.java:2910-2928): no source -> NEW_TASK is forced ("startActivity called from non-Activity context"); singleTask implies it. After the
    // explicit NEW_TASK's result cancel (:2718), so singleTask keeps its result.
    let flags = if single_task || call.source.is_none() {
        call.flags | NEW_TASK
    } else {
        call.flags
    };
    if decl.process.as_deref().is_some_and(|p| p != target.process) {
        return Err(refused(START_CANCELED, "activity in another process", 0));
    }
    let unmodelled = flags & !(MODELLED | IGNORED);
    if unmodelled != 0 {
        return Err(refused(
            START_CANCELED,
            "intent flags not modelled",
            unmodelled,
        ));
    }
    let new_task = flags & NEW_TASK != 0;
    // resolveReusableTask (:2972-2978): NEW_TASK without MULTIPLE_TASK, or singleTask, and no result still owed
    let reuse = (new_task && flags & MULTIPLE_TASK == 0 || single_task) && !call.result_at_reuse();
    if new_task && flags & MULTIPLE_TASK != 0 {
        return Err(refused(
            START_CANCELED,
            "NEW_TASK with MULTIPLE_TASK (a new task is not modelled)",
            MULTIPLE_TASK,
        ));
    }
    // computeLaunchingTaskFlags :2931-2939 drops LAUNCH_ADJACENT without NEW_TASK or a source; with both it asks for a task beside the source's
    if flags & LAUNCH_ADJACENT != 0 && new_task && call.source.is_some() {
        return Err(refused(
            START_CANCELED,
            "LAUNCH_ADJACENT with NEW_TASK (a task beside is not modelled)",
            LAUNCH_ADJACENT,
        ));
    }
    // complyActivityFlags :2493-2497 resets only a reused task
    if reuse && flags & RESET_TASK_IF_NEEDED != 0 {
        return Err(refused(
            START_CANCELED,
            "RESET_TASK_IF_NEEDED on a reused task (task reset is not modelled)",
            RESET_TASK_IF_NEEDED,
        ));
    }
    if reuse {
        let affinity = decl
            .task_affinity
            .as_deref()
            .unwrap_or(call.calling_package);
        match call.source {
            // findTask finds the Task of the activity's affinity; one of another affinity than the source's would be a second Task
            Some(source) if affinity != source.affinity => {
                return Err(refused(
                    START_CANCELED,
                    "NEW_TASK into another task affinity",
                    NEW_TASK,
                ));
            }
            Some(_) => {}
            None if target.task.is_none() => {
                return Err(refused(
                    START_CANCELED,
                    "no source and no task of the activity's affinity (a new task is not modelled)",
                    0,
                ));
            }
            None => {
                atl_diag::answered(
                    INTERFACE,
                    "startActivity with no source activity (into its affinity's task)",
                );
            }
        }
    }
    // not reused: computeTargetTask (:2169-2175) puts it into the source's Task (a result is owed, or no NEW_TASK)
    let Some(task) = target.task else {
        return Err(refused(START_CANCELED, "no task to start into", 0));
    };
    Ok((decl, comply(call, flags, decl, task)))
}

/// complyActivityFlags (ActivityStarter.java:2490-2611) on the Task, then deliverToCurrentTopIfNeeded (:2441-2484) when the start still adds a record.
fn comply(call: &Call<'_>, flags: i32, decl: &Declared, task: &[Rec<'_>]) -> Plan {
    let class = decl.name.as_str();
    let live = |i: &usize| !task[*i].finishing;
    // Task.findActivityInHistory: the newest non-finishing record of the class
    let newest = task.iter().rposition(|r| !r.finishing && r.class == class);
    let single_top = flags & SINGLE_TOP != 0 || decl.launch_mode == LAUNCH_SINGLE_TOP;
    let mut finish: Vec<usize> = Vec::new();
    let mut then = None;
    if flags & (NEW_TASK | CLEAR_TASK) == NEW_TASK | CLEAR_TASK {
        // performClearTaskForReuse -> removeActivities: every record, bottom first; the new record becomes the root
        finish = (0..task.len()).filter(live).collect();
    } else if flags & CLEAR_TOP != 0 || decl.launch_mode == LAUNCH_SINGLE_TASK {
        // Task.clearTopActivities: above r top first; a standard r without SINGLE_TOP finishes too and a new one is pushed, else r gets the intent
        if let Some(r) = newest {
            finish = (r + 1..task.len()).rev().filter(live).collect();
            if decl.launch_mode == LAUNCH_MODE_STANDARD && flags & SINGLE_TOP == 0 {
                finish.push(r);
            } else {
                then = Some(Then::Deliver {
                    to: r,
                    to_front: false,
                    cancel: false,
                });
            }
        }
    } else if flags & REORDER_TO_FRONT != 0 {
        then = newest.map(|r| Then::Deliver {
            to: r,
            to_front: true,
            cancel: false,
        });
    } else if task.first().is_some_and(|root| root.class == class) {
        // the Task's root class (realActivity): singleTop with the class on top and no result -> the intent to the top. A root intent equal to the start's would bring the Task to front
        // without a record (:2590-2596); the Task's base intent is not kept, so the start adds a record (plan S3)
        let top = task.iter().rposition(|r| !r.finishing);
        if let Some(t) =
            top.filter(|&t| single_top && task[t].class == class && !call.result_at_reuse())
        {
            then = Some(Then::Deliver {
                to: t,
                to_front: false,
                cancel: false,
            });
        }
    }
    let then = then.unwrap_or_else(|| {
        // topRunningNonDelayedActivityLocked(mNotTop): PREVIOUS_IS_TOP leaves the source out (:2793)
        let skip_source = flags & PREVIOUS_IS_TOP != 0;
        let top = (0..task.len()).rev().find(|i| {
            !task[*i].finishing && !finish.contains(i) && !(skip_source && task[*i].source)
        });
        match top {
            Some(t) if single_top && task[t].class == class => Then::Deliver {
                to: t,
                to_front: false,
                cancel: true,
            },
            _ => Then::Push,
        }
    });
    Plan {
        finish,
        then,
        no_history: flags & NO_HISTORY != 0 || decl.no_history,
        user_leaving: flags & NO_USER_ACTION == 0,
    }
}

/// A start with no caller app thread (ActivityTaskManagerService.startActivityAsUser with caller null: `am start`, a home screen). The uid must hold START_ANY_ACTIVITY, which only
/// the system and shell uids are given here (ActivityTaskSupervisor.checkStartAnyActivityPermission; fail-closed: any other uid is a SecurityException). Accepted: the explicit
/// launcher activity of an installed package whose process is not running, with NEW_TASK (ContextImpl.startActivity requires it outside an activity): its process is started and the
/// activity launched on attach (ActivityTaskManagerService.startProcessAsync -> realStartActivityLocked), in a new task. Refused: a running process (bringing its task to front is not
/// modelled), another class, flags other than NEW_TASK.
pub fn check_shell(
    calling_uid: u32,
    flags: i32,
    component: Option<(&str, &str)>,
    launcher: Option<&str>,
    running: bool,
) -> Result<(), Refusal> {
    if calling_uid != SYSTEM_UID && calling_uid != SHELL_UID {
        return Err(Refusal::Security("startActivity: no caller app thread"));
    }
    let Some((_, class)) = component else {
        return Err(refused(
            START_INTENT_NOT_RESOLVED,
            "implicit intent (intent filters are not modelled)",
            0,
        ));
    };
    let Some(launcher) = launcher else {
        return Err(refused(
            START_CLASS_NOT_FOUND,
            "package not installed or without launcher",
            0,
        ));
    };
    if class != launcher {
        return Err(refused(
            START_CANCELED,
            "shell start of a class other than the launcher",
            0,
        ));
    }
    if flags != NEW_TASK {
        return Err(refused(
            START_CANCELED,
            "shell start without NEW_TASK alone",
            flags & !NEW_TASK,
        ));
    }
    if running {
        return Err(refused(
            START_CANCELED,
            "shell start of a running process (task to front not modelled)",
            0,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = "org.example.app";
    const UID: u32 = 10_123;
    const MAIN: &str = "org.example.app.MainActivity";
    const SPLASH: &str = "org.example.app.SplashActivity";
    const OTHER_PACKAGE: &str = "org.example.other";

    /// A standard activity in the package's process, no affinity written (the parse fills both with the package's).
    fn decl(name: &str) -> Declared {
        Declared {
            name: name.to_owned(),
            launch_mode: LAUNCH_MODE_STANDARD,
            process: Some(PACKAGE.to_owned()),
            task_affinity: None,
            no_history: false,
        }
    }

    fn source() -> Source<'static> {
        Source {
            uid: UID,
            package: PACKAGE,
            affinity: PACKAGE,
            finishing: false,
            owes: false,
        }
    }

    /// A start from Splash of the package's own Main: the call every test below changes one field of.
    fn call<'a>() -> Call<'a> {
        Call {
            calling_uid: UID,
            calling_package: PACKAGE,
            request_code: -1,
            flags: 0,
            component: Some((PACKAGE, MAIN)),
            source: Some(source()),
        }
    }

    /// The Task [Splash (source, top)].
    const SPLASH_TASK: &[Rec<'static>] = &[Rec {
        class: SPLASH,
        finishing: false,
        source: true,
    }];

    fn target<'a>(main: &'a Declared, task: &'a [Rec<'a>]) -> Target<'a> {
        Target {
            process: PACKAGE,
            decl: Some(main),
            task: Some(task),
        }
    }

    fn code_of(r: Result<(&Declared, Plan), Refusal>) -> Refusal {
        match r {
            Err(refusal) => refusal,
            Ok((d, _)) => panic!("accepted {}, refusal expected", d.name),
        }
    }

    fn plan_of(r: Result<(&Declared, Plan), Refusal>) -> Plan {
        r.expect("accepted").1
    }

    fn push() -> Plan {
        Plan {
            finish: Vec::new(),
            then: Then::Push,
            no_history: false,
            user_leaving: true,
        }
    }

    fn deliver(to: usize, to_front: bool, cancel: bool) -> Then {
        Then::Deliver {
            to,
            to_front,
            cancel,
        }
    }

    fn canceled(reason: &'static str, flags: i32) -> Refusal {
        Refusal::Code {
            code: START_CANCELED,
            reason,
            flags,
        }
    }

    #[test]
    fn accepts_an_explicit_start_in_the_callers_task() {
        let main = decl(MAIN);
        let (accepted, plan) = decide(&call(), target(&main, SPLASH_TASK)).expect("accepted");
        assert_eq!(accepted.name, MAIN);
        assert_eq!(plan, push());
        assert_eq!(plan.code(), START_SUCCESS);
    }

    /// The shell start: system / shell uid only, the launcher only, NEW_TASK alone, a process not running.
    #[test]
    fn shell_starts_only_a_dead_packages_launcher() {
        let ok = |uid, flags, class, running| {
            check_shell(uid, flags, Some((PACKAGE, class)), Some(MAIN), running)
        };
        let canceled_code = |r: Result<(), Refusal>| {
            matches!(
                r,
                Err(Refusal::Code {
                    code: START_CANCELED,
                    ..
                })
            )
        };
        assert_eq!(ok(SHELL_UID, NEW_TASK, MAIN, false), Ok(()));
        assert_eq!(ok(SYSTEM_UID, NEW_TASK, MAIN, false), Ok(()));
        assert!(matches!(
            ok(UID, NEW_TASK, MAIN, false),
            Err(Refusal::Security(_))
        ));
        assert!(canceled_code(ok(SHELL_UID, NEW_TASK, MAIN, true)));
        assert!(canceled_code(ok(SHELL_UID, NEW_TASK, SPLASH, false)));
        assert!(canceled_code(ok(SHELL_UID, 0, MAIN, false)));
        assert!(matches!(
            check_shell(SHELL_UID, NEW_TASK, Some((PACKAGE, MAIN)), None, false),
            Err(Refusal::Code {
                code: START_CLASS_NOT_FOUND,
                ..
            })
        ));
    }

    #[test]
    fn accepts_new_task_into_the_callers_affinity() {
        let main = decl(MAIN);
        let c = Call {
            flags: NEW_TASK,
            ..call()
        };
        assert_eq!(plan_of(decide(&c, target(&main, SPLASH_TASK))), push());
    }

    #[test]
    fn accepts_a_declared_affinity_that_matches_the_source() {
        let mut main = decl(MAIN);
        main.task_affinity = Some("org.example.tasks".to_owned());
        let c = Call {
            flags: NEW_TASK,
            source: Some(Source {
                affinity: "org.example.tasks",
                ..source()
            }),
            ..call()
        };
        assert!(decide(&c, target(&main, SPLASH_TASK)).is_ok());
    }

    /// resultTo null or naming no record, with a request code and no NEW_TASK in the intent: ActivityStarter.executeRequest leaves sourceRecord and resultRecord null (:1104-1120, no error),
    /// computeLaunchingTaskFlags forces NEW_TASK (:2910-2918), findTask finds the caller's Task of the package affinity and a standard activity of another class is added on top
    /// (:2597-2602, recycleTask :2398-2402 START_SUCCESS): a push, no result owed.
    #[test]
    fn a_start_with_no_source_goes_into_its_affinitys_task() {
        let main = decl(MAIN);
        let c = Call {
            source: None,
            request_code: 7,
            ..call()
        };
        assert!(!c.result_requested());
        assert!(!c.new_task_cancels_result());
        assert_eq!(plan_of(decide(&c, target(&main, SPLASH_TASK))), push());
    }

    /// No source and no Task of the activity's affinity: AOSP starts a new Task, which is not modelled -> refused, counted.
    #[test]
    fn a_start_with_no_source_and_no_affinity_task_is_refused() {
        let main = decl(MAIN);
        let c = Call {
            source: None,
            ..call()
        };
        let none = Target {
            task: None,
            ..target(&main, SPLASH_TASK)
        };
        assert_eq!(
            code_of(decide(&c, none)),
            canceled(
                "no source and no task of the activity's affinity (a new task is not modelled)",
                0
            )
        );
    }

    /// The forced NEW_TASK makes MULTIPLE_TASK a new-task request: refused, the bit named.
    #[test]
    fn a_start_with_no_source_and_multiple_task_is_refused() {
        let main = decl(MAIN);
        let c = Call {
            source: None,
            flags: MULTIPLE_TASK,
            ..call()
        };
        assert_eq!(
            code_of(decide(&c, target(&main, SPLASH_TASK))),
            canceled(
                "NEW_TASK with MULTIPLE_TASK (a new task is not modelled)",
                MULTIPLE_TASK
            )
        );
    }

    #[test]
    fn a_source_of_another_uid_or_package_is_a_security_error() {
        let main = decl(MAIN);
        for s in [
            Source {
                uid: UID + 1,
                ..source()
            },
            Source {
                package: OTHER_PACKAGE,
                affinity: OTHER_PACKAGE,
                ..source()
            },
        ] {
            let c = Call {
                source: Some(s),
                ..call()
            };
            assert!(matches!(
                code_of(decide(&c, target(&main, SPLASH_TASK))),
                Refusal::Security(_)
            ));
        }
    }

    /// No result is owed to a plain start (request code -1), nor to a source that is finishing (ActivityStarter.java:1112-1119); one is for a request code.
    #[test]
    fn owes_a_result_for_a_request_code_from_a_live_source() {
        assert!(!call().result_requested());
        let asked = Call {
            request_code: 7,
            ..call()
        };
        assert!(asked.result_requested());
        let finishing = Call {
            source: Some(Source {
                finishing: true,
                ..source()
            }),
            ..asked
        };
        assert!(!finishing.result_requested());
        let main = decl(MAIN);
        assert_eq!(
            plan_of(decide(&finishing, target(&main, SPLASH_TASK))),
            push()
        );
    }

    /// FORWARD_RESULT: the result requested is the source's own (owes), never with a request code (-93 before anything else, ActivityStarter.java:1126-1128).
    #[test]
    fn forward_result_moves_the_sources_result_and_refuses_a_request_code() {
        let main = decl(MAIN);
        let forward = Call {
            flags: FORWARD_RESULT,
            source: Some(Source {
                owes: true,
                ..source()
            }),
            ..call()
        };
        assert!(forward.forwards());
        assert!(forward.result_requested());
        assert_eq!(
            plan_of(decide(&forward, target(&main, SPLASH_TASK))),
            push()
        );
        let owes_nothing = Call {
            source: Some(source()),
            ..forward
        };
        assert!(!owes_nothing.result_requested());
        let with_code = Call {
            request_code: 4,
            ..forward
        };
        assert_eq!(
            code_of(decide(&with_code, target(&main, SPLASH_TASK))),
            Refusal::Code {
                code: START_FORWARD_AND_REQUEST_CONFLICT,
                reason: "FORWARD_RESULT with a request code",
                flags: FORWARD_RESULT
            }
        );
        // with no source the flag forwards nothing (sourceRecord null, :1123)
        let no_source = Call {
            source: None,
            ..forward
        };
        assert!(!no_source.forwards());
    }

    /// An explicit NEW_TASK cancels the result before the reuse decision (ActivityStarter.java:2718): singleTask then reuses as with no result.
    #[test]
    fn an_explicit_new_task_cancels_the_result_before_the_reuse() {
        let c = Call {
            request_code: 7,
            flags: NEW_TASK,
            ..call()
        };
        assert!(c.new_task_cancels_result());
        assert!(!c.result_at_reuse());
    }

    #[test]
    fn refuses_an_implicit_intent_with_intent_not_resolved() {
        let main = decl(MAIN);
        let c = Call {
            component: None,
            ..call()
        };
        assert_eq!(
            code_of(decide(&c, target(&main, SPLASH_TASK))),
            Refusal::Code {
                code: START_INTENT_NOT_RESOLVED,
                reason: "implicit intent (no activity matches)",
                flags: 0
            }
        );
    }

    #[test]
    fn refuses_another_packages_activity() {
        let main = decl(MAIN);
        let c = Call {
            component: Some((OTHER_PACKAGE, MAIN)),
            ..call()
        };
        assert_eq!(
            code_of(decide(&c, target(&main, SPLASH_TASK))),
            canceled("another package's activity", 0)
        );
    }

    #[test]
    fn refuses_a_class_the_package_does_not_declare_with_class_not_found() {
        let splash = decl(SPLASH);
        let not_found = Refusal::Code {
            code: START_CLASS_NOT_FOUND,
            reason: "class not declared by the package",
            flags: 0,
        };
        assert_eq!(
            code_of(decide(&call(), target(&splash, SPLASH_TASK))),
            not_found
        );
        let none = Target {
            decl: None,
            ..target(&splash, SPLASH_TASK)
        };
        assert_eq!(code_of(decide(&call(), none)), not_found);
    }

    #[test]
    fn refuses_single_instance_and_another_process() {
        let mut main = decl(MAIN);
        main.launch_mode = 3; // ActivityInfo.LAUNCH_SINGLE_INSTANCE
        assert_eq!(
            code_of(decide(&call(), target(&main, SPLASH_TASK))),
            canceled("launch mode singleInstance / singleInstancePerTask", 0)
        );
        let mut remote = decl(MAIN);
        remote.process = Some(format!("{PACKAGE}:remote"));
        assert_eq!(
            code_of(decide(&call(), target(&remote, SPLASH_TASK))),
            canceled("activity in another process", 0)
        );
    }

    #[test]
    fn refuses_new_task_into_another_affinity() {
        let mut main = decl(MAIN);
        main.task_affinity = Some("org.example.tasks".to_owned());
        let c = Call {
            flags: NEW_TASK,
            ..call()
        };
        assert_eq!(
            code_of(decide(&c, target(&main, SPLASH_TASK))),
            canceled("NEW_TASK into another task affinity", NEW_TASK)
        );
        // without NEW_TASK the other affinity joins the source's Task (CTS newTask/test-3: [TaskAffinity1 Regular])
        assert_eq!(plan_of(decide(&call(), target(&main, SPLASH_TASK))), push());
    }

    /// Bits that need a second Task or are not modelled are refused with the bits named; ignored bits and the single-Task flags are accepted.
    #[test]
    fn refusals_name_the_flag_bits() {
        let main = decl(MAIN);
        let new_document = 0x0008_0000;
        let require_non_browser = 0x0000_0400;
        assert_eq!(
            code_of(decide(
                &Call {
                    flags: new_document | require_non_browser | CLEAR_TOP,
                    ..call()
                },
                target(&main, SPLASH_TASK)
            )),
            canceled(
                "intent flags not modelled",
                new_document | require_non_browser
            )
        );
        assert_eq!(
            flag_names(new_document | require_non_browser),
            "NEW_DOCUMENT|REQUIRE_NON_BROWSER"
        );
        assert_eq!(flag_names(CLEAR_TOP | NEW_TASK), "NEW_TASK|CLEAR_TOP");
        assert_eq!(
            code_of(decide(
                &Call {
                    flags: NEW_TASK | LAUNCH_ADJACENT,
                    ..call()
                },
                target(&main, SPLASH_TASK)
            )),
            canceled(
                "LAUNCH_ADJACENT with NEW_TASK (a task beside is not modelled)",
                LAUNCH_ADJACENT
            )
        );
        assert_eq!(
            code_of(decide(
                &Call {
                    flags: NEW_TASK | RESET_TASK_IF_NEEDED,
                    ..call()
                },
                target(&main, SPLASH_TASK)
            )),
            canceled(
                "RESET_TASK_IF_NEEDED on a reused task (task reset is not modelled)",
                RESET_TASK_IF_NEEDED
            )
        );
        // ignored: no placement effect; LAUNCH_ADJACENT / RESET_TASK_IF_NEEDED / MULTIPLE_TASK / CLEAR_TASK without NEW_TASK have none either (CTS clear-task_without_new-task,
        // newTask/test-multiple-task)
        for flags in [
            IGNORED,
            LAUNCH_ADJACENT | RESET_TASK_IF_NEEDED | MULTIPLE_TASK | CLEAR_TASK,
        ] {
            let c = Call { flags, ..call() };
            assert_eq!(
                plan_of(decide(&c, target(&main, SPLASH_TASK))),
                push(),
                "{flags:x}"
            );
        }
    }

    /// A Task view from CTS-style names, bottom -> top; the last record is the source.
    fn task_of<'a>(classes: &[&'a str]) -> Vec<Rec<'a>> {
        let n = classes.len();
        classes
            .iter()
            .enumerate()
            .map(|(i, c)| Rec {
                class: c,
                finishing: false,
                source: i + 1 == n,
            })
            .collect()
    }

    const R: &str = "Regular";
    const A1: &str = "TaskAffinity1";
    const A2: &str = "TaskAffinity2";
    const ST: &str = "SingleTop";
    const TASK: &str = "SingleTask";

    fn activity(name: &str, mode: i32) -> Declared {
        Declared {
            name: name.to_owned(),
            launch_mode: mode,
            ..decl(name)
        }
    }

    /// One CTS intent case in the single-Task scope (cts/tests/framework/base/windowmanager/intent_tests, plan S6), stacks bottom -> top: decide's plan applied to the stack must give
    /// the case's end stack. `kept` is how the end stack is made of the old records (Some(i)) and a pushed one (None), so a reused record and a new instance are told apart where the JSON
    /// alone cannot: AOSP's clearTopActivities / deliverToCurrentTopIfNeeded say which (comments per row).
    struct Case {
        name: &'static str,
        initial: &'static [&'static str],
        start: &'static str,
        mode: i32,
        flags: i32,
        request: i32,
        end: &'static [Option<usize>],
    }

    /// Applies a plan to the bottom -> top indices of a stack: finished records leave, a delivered record may move to the top, a push adds None.
    fn apply(n: usize, plan: &Plan) -> Vec<Option<usize>> {
        let mut stack: Vec<Option<usize>> = (0..n)
            .filter(|i| !plan.finish.contains(i))
            .map(Some)
            .collect();
        match plan.then {
            Then::Push => stack.push(None),
            Then::Deliver { to, to_front, .. } => {
                if to_front {
                    stack.retain(|r| *r != Some(to));
                    stack.push(Some(to));
                }
            }
        }
        stack
    }

    #[test]
    fn cts_intent_cases_in_one_task() {
        const STANDARD: i32 = LAUNCH_MODE_STANDARD;
        let cases = [
            // clearCases/test-1, forResult/test-5,6: [R] CLEAR_TOP R -> [R]: standard without SINGLE_TOP finishes R and starts a new one (clearTopActivities :1731-1736)
            Case {
                name: "clear-1",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: CLEAR_TOP,
                request: -1,
                end: &[None],
            },
            Case {
                name: "forResult-6",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: CLEAR_TOP,
                request: 1,
                end: &[None],
            },
            // clearCases/test-2: [R R] -> [R R]: the newest R is cleared and recreated
            Case {
                name: "clear-2",
                initial: &[R, R],
                start: R,
                mode: STANDARD,
                flags: CLEAR_TOP,
                request: -1,
                end: &[Some(0), None],
            },
            // clearCases/test-5: PREVIOUS_IS_TOP does not change a clear-top
            Case {
                name: "clear-5",
                initial: &[R, R],
                start: R,
                mode: STANDARD,
                flags: CLEAR_TOP | PREVIOUS_IS_TOP,
                request: -1,
                end: &[Some(0), None],
            },
            // clearCases/test-6: [R A1 R] CLEAR_TOP A1 -> [R A1]: the top R and A1 finish, a new A1 is pushed
            Case {
                name: "clear-6",
                initial: &[R, A1, R],
                start: A1,
                mode: STANDARD,
                flags: CLEAR_TOP,
                request: -1,
                end: &[Some(0), None],
            },
            // CLEAR_TOP|SINGLE_TOP keeps the instance: the intent to it after the records above finish
            Case {
                name: "clear-top-single-top",
                initial: &[R, A1, R],
                start: A1,
                mode: STANDARD,
                flags: CLEAR_TOP | SINGLE_TOP,
                request: -1,
                end: &[Some(0), Some(1)],
            },
            // clear-task_with_new-task: [R] NEW_TASK|CLEAR_TASK R -> [R], a new root
            Case {
                name: "clear-task-new-task",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: NEW_TASK | CLEAR_TASK,
                request: -1,
                end: &[None],
            },
            // clear-task_without_new-task: CLEAR_TASK alone is ignored -> [R R]
            Case {
                name: "clear-task-alone",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: CLEAR_TASK,
                request: -1,
                end: &[Some(0), None],
            },
            // forResult/test-1,2: [R] SINGLE_TOP R -> [R] (the top gets the intent)
            Case {
                name: "forResult-1",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: SINGLE_TOP,
                request: -1,
                end: &[Some(0)],
            },
            Case {
                name: "forResult-2",
                initial: &[R],
                start: R,
                mode: STANDARD,
                flags: SINGLE_TOP,
                request: 1,
                end: &[Some(0)],
            },
            // forResult/test-3,4: [ST] ST -> [ST]
            Case {
                name: "forResult-3",
                initial: &[ST],
                start: ST,
                mode: LAUNCH_SINGLE_TOP,
                flags: 0,
                request: -1,
                end: &[Some(0)],
            },
            Case {
                name: "forResult-4",
                initial: &[ST],
                start: ST,
                mode: LAUNCH_SINGLE_TOP,
                flags: 0,
                request: 1,
                end: &[Some(0)],
            },
            // forResult/test-8,9: [R A1] REORDER_TO_FRONT R -> [A1 R]
            Case {
                name: "forResult-8",
                initial: &[R, A1],
                start: R,
                mode: STANDARD,
                flags: REORDER_TO_FRONT,
                request: 1,
                end: &[Some(1), Some(0)],
            },
            Case {
                name: "forResult-9",
                initial: &[R, A1],
                start: R,
                mode: STANDARD,
                flags: REORDER_TO_FRONT,
                request: -1,
                end: &[Some(1), Some(0)],
            },
            // reorderToFront/reorder-to-front: [R A1 A2 R] REORDER_TO_FRONT A1 -> [R A2 R A1]
            Case {
                name: "reorder-to-front",
                initial: &[R, A1, A2, R],
                start: A1,
                mode: STANDARD,
                flags: REORDER_TO_FRONT,
                request: -1,
                end: &[Some(0), Some(2), Some(3), Some(1)],
            },
            // newTask/test-multiple-task: MULTIPLE_TASK alone -> [R ST]; newTask/test-6: NEW_TASK ST of the same affinity -> [R ST]
            Case {
                name: "multiple-task-alone",
                initial: &[R],
                start: ST,
                mode: LAUNCH_SINGLE_TOP,
                flags: MULTIPLE_TASK,
                request: -1,
                end: &[Some(0), None],
            },
            Case {
                name: "newTask-6",
                initial: &[R],
                start: ST,
                mode: LAUNCH_SINGLE_TOP,
                flags: NEW_TASK,
                request: -1,
                end: &[Some(0), None],
            },
            // singleTask in the Task: clear to it and the intent to it, also while a result is asked (complyActivityFlags runs on the source's Task, :2511-2532)
            Case {
                name: "single-task",
                initial: &[TASK, R],
                start: TASK,
                mode: LAUNCH_SINGLE_TASK,
                flags: 0,
                request: -1,
                end: &[Some(0)],
            },
            Case {
                name: "single-task-result",
                initial: &[TASK, R],
                start: TASK,
                mode: LAUNCH_SINGLE_TASK,
                flags: 0,
                request: 1,
                end: &[Some(0)],
            },
            // PREVIOUS_IS_TOP: the source is not top for singleTop -> a new instance above it (the root branch needs a root of the class: here the root is R)
            Case {
                name: "previous-is-top",
                initial: &[R, ST],
                start: ST,
                mode: LAUNCH_SINGLE_TOP,
                flags: PREVIOUS_IS_TOP,
                request: -1,
                end: &[Some(0), Some(1), None],
            },
        ];
        for case in &cases {
            let d = activity(case.start, case.mode);
            let task = task_of(case.initial);
            let c = Call {
                component: Some((PACKAGE, case.start)),
                flags: case.flags,
                request_code: case.request,
                ..call()
            };
            let plan = plan_of(decide(&c, target(&d, &task)));
            assert_eq!(
                apply(task.len(), &plan),
                case.end.to_vec(),
                "{}: {plan:?}",
                case.name
            );
        }
    }

    /// The finish order and the result handling of each reuse: clear-top above top first, clear-task bottom first; deliverToCurrentTopIfNeeded cancels the owed result, recycleTask's reuse does not.
    #[test]
    fn finish_order_and_result_cancel() {
        let r = activity(R, LAUNCH_MODE_STANDARD);
        let a1 = activity(A1, LAUNCH_MODE_STANDARD);
        let task = task_of(&[R, A1, R]);
        let clear_top = Call {
            component: Some((PACKAGE, A1)),
            flags: CLEAR_TOP,
            ..call()
        };
        assert_eq!(
            plan_of(decide(&clear_top, target(&a1, &task))).finish,
            vec![2, 1]
        );
        let clear_task = Call {
            component: Some((PACKAGE, R)),
            flags: NEW_TASK | CLEAR_TASK,
            ..call()
        };
        assert_eq!(
            plan_of(decide(&clear_task, target(&r, &task))).finish,
            vec![0, 1, 2]
        );
        let single_top = Call {
            component: Some((PACKAGE, R)),
            flags: SINGLE_TOP,
            request_code: 1,
            ..call()
        };
        assert_eq!(
            plan_of(decide(&single_top, target(&r, &task))).then,
            deliver(2, false, true)
        );
        let keep = Call {
            component: Some((PACKAGE, A1)),
            flags: CLEAR_TOP | SINGLE_TOP,
            request_code: 1,
            ..call()
        };
        let plan = plan_of(decide(&keep, target(&a1, &task)));
        assert_eq!(
            (plan.finish, plan.then),
            (vec![2], deliver(1, false, false))
        );
        assert_eq!(
            plan_of(decide(&keep, target(&a1, &task))).code(),
            START_DELIVERED_TO_TOP
        );
    }

    /// The new record's no-history (intent flag or manifest) and NO_USER_ACTION's userLeaving.
    #[test]
    fn no_history_and_no_user_action_reach_the_plan() {
        let mut main = decl(MAIN);
        let p = plan_of(decide(
            &Call {
                flags: NO_HISTORY | NO_USER_ACTION,
                ..call()
            },
            target(&main, SPLASH_TASK),
        ));
        assert!(p.no_history && !p.user_leaving);
        main.no_history = true;
        let p = plan_of(decide(&call(), target(&main, SPLASH_TASK)));
        assert!(p.no_history && p.user_leaving);
    }
}
