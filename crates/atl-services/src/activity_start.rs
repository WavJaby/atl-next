// SPDX-License-Identifier: MIT
//! Whether the services start an activity for a startActivity call: the accepted subset of ActivityStarter, pure (no binder), so the tests build without the converter feature.
//! Accepted: an explicit component of the caller's own package, started from one of the caller's activities, in the caller's task and process: standard (a new
//! instance), singleTop (onNewIntent to the top when it is that class, else new), singleTask (onNewIntent to its instance in the task after clearing what is above, else new; a new instance while a result is owed).
//! A start for a result (startActivityForResult) is accepted too: the result it owes its caller is answered by the services (see [`Call::result_requested`]).
//! And a start with no caller app (shell / home) of a package's launcher whose process is not running ([`check_shell`]).
//! Every other call is refused: a SecurityException when the caller does not own the activity it names (fail-closed), otherwise the START code AOSP returns for the case
//! (ActivityManager.java:162 FIRST_START_FATAL = -100), counted by `counters::refused` under its reason. Refusals are never silent: the app sees the code, the log sees the count.
//! Refused, not modelled: implicit intents (intent filters), other packages, singleInstance, other processes, intent flags except NEW_TASK, task affinity
//! across tasks (NEW_TASK into another affinity), more than one task per package, bringing a running app's task to front.

use crate::counters;
use crate::packages::ActivityDecl;
use crate::permission::{SHELL_UID, SYSTEM_UID};

/// ActivityManager.START_SUCCESS: the activity is started (the record is pushed and its launch sent).
pub const START_SUCCESS: i32 = 0;
/// ActivityManager.START_TASK_TO_FRONT / START_DELIVERED_TO_TOP: a reused instance got the intent (after a clear-top / as the top); success codes, Instrumentation does not throw.
pub const START_TASK_TO_FRONT: i32 = 2;
pub const START_DELIVERED_TO_TOP: i32 = 3;
/// ActivityManager.START_CANCELED: Instrumentation.execStartActivity throws "Activity could not be started" (Instrumentation.java:2452).
pub const START_CANCELED: i32 = -96;
/// ActivityManager.START_CLASS_NOT_FOUND: the explicit component's class is not in the package.
pub const START_CLASS_NOT_FOUND: i32 = -92;
/// ActivityManager.START_INTENT_NOT_RESOLVED: no activity matches the implicit intent.
pub const START_INTENT_NOT_RESOLVED: i32 = -91;

/// Intent.FLAG_ACTIVITY_NEW_TASK.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
/// ActivityInfo.LAUNCH_MULTIPLE: `standard`, the launch mode a manifest without `android:launchMode` gets; LAUNCH_SINGLE_TOP / LAUNCH_SINGLE_TASK.
const LAUNCH_MODE_STANDARD: i32 = 0;
const LAUNCH_SINGLE_TOP: i32 = 1;
const LAUNCH_SINGLE_TASK: i32 = 2;
const INTERFACE: &str = "IActivityTaskManager";

/// Why a start is refused. `Security` is thrown as a SecurityException; `Code` is returned as the startActivity result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Security(&'static str),
    Code { code: i32, reason: &'static str },
}

/// The activity resultTo names, as the services know its owner: the owner's uid and package, and the affinity its task has (its manifest's taskAffinity, else the package name).
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    pub uid: u32,
    pub package: &'a str,
    pub affinity: &'a str,
    /// ActivityRecord.finishing: no result is owed to a finishing caller (ActivityStarter.java:1112).
    pub finishing: bool,
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
    /// resultTo's activity, when the caller's thread runs it. None: resultTo names no activity.
    pub source: Option<Source<'a>>,
}

impl Call<'_> {
    /// A result is owed to the source: requestCode >= 0 and the source not finishing (ActivityStarter.executeRequest, ActivityStarter.java:1106-1119: resultRecord).
    pub fn result_requested(&self) -> bool {
        self.request_code >= 0 && self.source.is_some_and(|s| !s.finishing)
    }

    /// An explicit NEW_TASK in the intent cancels the owed result before the start is decided (sendNewTaskResultRequestIfNeeded, ActivityStarter.java:2718, 2846-2849).
    pub fn new_task_cancels_result(&self) -> bool {
        self.result_requested() && self.flags & FLAG_ACTIVITY_NEW_TASK != 0
    }

    /// The result still owed when the start is decided: what a singleTask reuse depends on (ActivityStarter.java:2978).
    pub fn result_at_reuse(&self) -> bool {
        self.result_requested() && !self.new_task_cancels_result()
    }
}

/// The package the component's class is looked up in: its process (`<application android:process>`, else the package name) and the class declaration, if the package has one.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub process: &'a str,
    pub decl: Option<&'a ActivityDecl>,
    /// An activity of the component's class, not finishing, is in the source's task: what a singleTask start reuses.
    pub in_task: bool,
    /// The source task's top non-finishing activity is of the component's class: what a singleTop start reuses.
    pub top_is_class: bool,
}

fn refused(code: i32, reason: &'static str) -> Refusal {
    counters::refused(INTERFACE, &format!("startActivity ({reason})"));
    Refusal::Code { code, reason }
}

/// Validates one start. Ok: the declaration of the activity to start; the caller starts it in the source's task.
pub fn check<'a>(
    call: &Call<'_>,
    target: Target<'a>,
) -> Result<(&'a ActivityDecl, Start), Refusal> {
    let Some(source) = call.source else {
        return Err(refused(START_CANCELED, "resultTo names no activity"));
    };
    if source.uid != call.calling_uid || source.package != call.calling_package {
        counters::refused(
            INTERFACE,
            "startActivity (resultTo is not the caller's activity)",
        );
        return Err(Refusal::Security("resultTo is not the caller's activity"));
    }
    let Some((package, class)) = call.component else {
        return Err(refused(
            START_INTENT_NOT_RESOLVED,
            "implicit intent (intent filters are not modelled)",
        ));
    };
    if package != call.calling_package {
        return Err(refused(START_CANCELED, "another package's activity"));
    }
    let Some(decl) = target.decl.filter(|d| d.name == class) else {
        return Err(refused(
            START_CLASS_NOT_FOUND,
            "class not declared by the package",
        ));
    };
    // reuse (ActivityStarter.startActivityInner): singleTop reuses the task's top when it is this class (deliverToCurrentTopIfNeeded, ActivityStarter.java:2441-2448), with a result owed or not
    // (its cancel goes first, :2469). singleTask reuses the class's instance in the task of its affinity, finishing what is above it (complyActivityFlags -> performClearTop, :2510-2533;
    // the affinity is checked below as for NEW_TASK, which singleTask implies), only with no result owed: getReusableIntentActivity looks for an instance only while resultTo is null
    // (:2978), so a result gives a new instance on the source's task (computeTargetTask, :2175). Either reuse gives the instance onNewIntent (deliverNewIntent). No instance: a new one on top.
    let start = match decl.launch_mode {
        LAUNCH_MODE_STANDARD => Start::New,
        LAUNCH_SINGLE_TOP if target.top_is_class => Start::Reuse { clear_top: false },
        LAUNCH_SINGLE_TOP => Start::New,
        LAUNCH_SINGLE_TASK if target.in_task && !call.result_at_reuse() => {
            Start::Reuse { clear_top: true }
        }
        LAUNCH_SINGLE_TASK => Start::New,
        _ => {
            return Err(refused(
                START_CANCELED,
                "launch mode singleInstance / singleInstancePerTask",
            ));
        }
    };
    let flags = if decl.launch_mode == LAUNCH_SINGLE_TASK {
        call.flags | FLAG_ACTIVITY_NEW_TASK
    } else {
        call.flags
    };
    if decl.process.as_deref().is_some_and(|p| p != target.process) {
        return Err(refused(START_CANCELED, "activity in another process"));
    }
    if flags & !FLAG_ACTIVITY_NEW_TASK != 0 {
        return Err(refused(START_CANCELED, "intent flags other than NEW_TASK"));
    }
    if flags & FLAG_ACTIVITY_NEW_TASK != 0 {
        let affinity = decl
            .task_affinity
            .as_deref()
            .unwrap_or(call.calling_package);
        if affinity != source.affinity {
            return Err(refused(
                START_CANCELED,
                "NEW_TASK into another task affinity",
            ));
        }
    }
    Ok((decl, start))
}

/// What an accepted start does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// A new record on top of the source's task.
    New,
    /// The instance of the class already in the task gets onNewIntent; `clear_top`: what is above it finishes first.
    Reuse { clear_top: bool },
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
        counters::refused(
            INTERFACE,
            "startActivity (no caller app, uid without START_ANY_ACTIVITY)",
        );
        return Err(Refusal::Security("startActivity: no caller app thread"));
    }
    let Some((_, class)) = component else {
        return Err(refused(
            START_INTENT_NOT_RESOLVED,
            "implicit intent (intent filters are not modelled)",
        ));
    };
    let Some(launcher) = launcher else {
        return Err(refused(
            START_CLASS_NOT_FOUND,
            "package not installed or without launcher",
        ));
    };
    if class != launcher {
        return Err(refused(
            START_CANCELED,
            "shell start of a class other than the launcher",
        ));
    }
    if flags != FLAG_ACTIVITY_NEW_TASK {
        return Err(refused(
            START_CANCELED,
            "shell start without NEW_TASK alone",
        ));
    }
    if running {
        return Err(refused(
            START_CANCELED,
            "shell start of a running process (task to front not modelled)",
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

    fn decl(name: &str) -> ActivityDecl {
        ActivityDecl {
            name: name.to_owned(),
            launcher: false,
            theme: 0,
            config_changes: 0,
            booleans: Default::default(),
            launch_mode: LAUNCH_MODE_STANDARD,
            task_affinity: None,
            process: None,
            meta_data: vec![],
            screen_orientation: crate::packages::SCREEN_ORIENTATION_UNSPECIFIED,
            resizeable: true,
            min_aspect_ratio: None,
            max_aspect_ratio: None,
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
            source: Some(Source {
                uid: UID,
                package: PACKAGE,
                affinity: PACKAGE,
                finishing: false,
            }),
        }
    }

    fn target(main: &ActivityDecl) -> Target<'_> {
        Target {
            process: PACKAGE,
            decl: Some(main),
            in_task: false,
            top_is_class: false,
        }
    }

    fn code_of(r: Result<(&ActivityDecl, Start), Refusal>) -> Refusal {
        match r {
            Err(refusal) => refusal,
            Ok((d, _)) => panic!("accepted {}, refusal expected", d.name),
        }
    }

    #[test]
    fn accepts_an_explicit_start_in_the_callers_task() {
        let main = decl(MAIN);
        let (accepted, start) = check(&call(), target(&main)).expect("accepted");
        assert_eq!(accepted.name, MAIN);
        assert_eq!(start, Start::New);
    }

    /// The shell start: system / shell uid only, the launcher only, NEW_TASK alone, a process not running.
    #[test]
    fn shell_starts_only_a_dead_packages_launcher() {
        let ok = |uid, flags, class, running| {
            check_shell(uid, flags, Some((PACKAGE, class)), Some(MAIN), running)
        };
        assert_eq!(ok(SHELL_UID, FLAG_ACTIVITY_NEW_TASK, MAIN, false), Ok(()));
        assert_eq!(ok(SYSTEM_UID, FLAG_ACTIVITY_NEW_TASK, MAIN, false), Ok(()));
        assert!(matches!(
            ok(UID, FLAG_ACTIVITY_NEW_TASK, MAIN, false),
            Err(Refusal::Security(_))
        ));
        assert!(matches!(
            ok(SHELL_UID, FLAG_ACTIVITY_NEW_TASK, MAIN, true),
            Err(Refusal::Code {
                code: START_CANCELED,
                ..
            })
        ));
        assert!(matches!(
            ok(SHELL_UID, FLAG_ACTIVITY_NEW_TASK, SPLASH, false),
            Err(Refusal::Code {
                code: START_CANCELED,
                ..
            })
        ));
        assert!(matches!(
            ok(SHELL_UID, 0, MAIN, false),
            Err(Refusal::Code {
                code: START_CANCELED,
                ..
            })
        ));
        assert!(matches!(
            check_shell(
                SHELL_UID,
                FLAG_ACTIVITY_NEW_TASK,
                Some((PACKAGE, MAIN)),
                None,
                false
            ),
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
            flags: FLAG_ACTIVITY_NEW_TASK,
            ..call()
        };
        assert!(check(&c, target(&main)).is_ok());
    }

    #[test]
    fn accepts_a_declared_affinity_that_matches_the_source() {
        let mut main = decl(MAIN);
        main.task_affinity = Some("org.example.tasks".to_owned());
        let c = Call {
            flags: FLAG_ACTIVITY_NEW_TASK,
            source: Some(Source {
                uid: UID,
                package: PACKAGE,
                affinity: "org.example.tasks",
                finishing: false,
            }),
            ..call()
        };
        assert!(check(&c, target(&main)).is_ok());
    }

    #[test]
    fn refuses_a_call_with_no_source_activity() {
        let main = decl(MAIN);
        let c = Call {
            source: None,
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "resultTo names no activity"
            }
        );
    }

    #[test]
    fn a_source_of_another_uid_is_a_security_error() {
        let main = decl(MAIN);
        let c = Call {
            source: Some(Source {
                uid: UID + 1,
                package: PACKAGE,
                affinity: PACKAGE,
                finishing: false,
            }),
            ..call()
        };
        assert!(matches!(
            code_of(check(&c, target(&main))),
            Refusal::Security(_)
        ));
    }

    #[test]
    fn a_source_of_another_package_is_a_security_error() {
        let main = decl(MAIN);
        let c = Call {
            source: Some(Source {
                uid: UID,
                package: OTHER_PACKAGE,
                affinity: OTHER_PACKAGE,
                finishing: false,
            }),
            ..call()
        };
        assert!(matches!(
            code_of(check(&c, target(&main))),
            Refusal::Security(_)
        ));
    }

    /// A start for a result is accepted: the result is owed to the caller (Call::result_requested) and answered by the services.
    #[test]
    fn accepts_a_start_for_result_and_owes_the_result() {
        let main = decl(MAIN);
        let c = Call {
            request_code: 7,
            ..call()
        };
        assert!(c.result_requested());
        assert_eq!(start_of(check(&c, target(&main))), Start::New);
    }

    /// No result is owed to a plain start (request code -1), nor to a source that is finishing (ActivityStarter.java:1112-1119).
    #[test]
    fn owes_no_result_without_a_request_code_or_to_a_finishing_source() {
        let main = decl(MAIN);
        assert!(!call().result_requested());
        let finishing = Call {
            request_code: 7,
            source: call().source.map(|s| Source {
                finishing: true,
                ..s
            }),
            ..call()
        };
        assert!(!finishing.result_requested());
        assert_eq!(start_of(check(&finishing, target(&main))), Start::New);
    }

    /// singleTop reuses for a result too; the services send the cancel before the intent (ActivityStarter.java:2469-2474).
    #[test]
    fn single_top_reuses_for_a_result_too() {
        let mut main = decl(MAIN);
        main.launch_mode = LAUNCH_SINGLE_TOP;
        let c = Call {
            request_code: 7,
            ..call()
        };
        let on_top = Target {
            in_task: true,
            top_is_class: true,
            ..target(&main)
        };
        assert_eq!(
            start_of(check(&c, on_top)),
            Start::Reuse { clear_top: false }
        );
    }

    /// singleTask is not reused while a result is owed: the new instance goes on the source's task (ActivityStarter.java:2978, 2175).
    #[test]
    fn single_task_with_a_result_starts_a_new_instance() {
        let mut main = decl(MAIN);
        main.launch_mode = LAUNCH_SINGLE_TASK;
        let in_task = Target {
            in_task: true,
            ..target(&main)
        };
        assert_eq!(
            start_of(check(&call(), in_task)),
            Start::Reuse { clear_top: true }
        );
        let c = Call {
            request_code: 7,
            ..call()
        };
        assert_eq!(start_of(check(&c, in_task)), Start::New);
    }

    /// An explicit NEW_TASK cancels the result before the reuse decision (ActivityStarter.java:2718): singleTask reuses as it does with no result.
    #[test]
    fn an_explicit_new_task_cancels_the_result_before_the_reuse() {
        let mut main = decl(MAIN);
        main.launch_mode = LAUNCH_SINGLE_TASK;
        let in_task = Target {
            in_task: true,
            ..target(&main)
        };
        let c = Call {
            request_code: 7,
            flags: FLAG_ACTIVITY_NEW_TASK,
            ..call()
        };
        assert!(c.new_task_cancels_result());
        assert!(!c.result_at_reuse());
        assert_eq!(
            start_of(check(&c, in_task)),
            Start::Reuse { clear_top: true }
        );
    }

    /// A refused start keeps its result owed: the caller answers it as cancelled (ActivityStarter.java:1220-1222).
    #[test]
    fn a_refused_start_for_result_keeps_the_result_owed() {
        let main = decl(MAIN);
        let c = Call {
            request_code: 7,
            component: Some((OTHER_PACKAGE, MAIN)),
            ..call()
        };
        assert!(c.result_requested());
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "another package's activity"
            }
        );
    }

    #[test]
    fn refuses_an_implicit_intent_with_intent_not_resolved() {
        let main = decl(MAIN);
        let c = Call {
            component: None,
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_INTENT_NOT_RESOLVED,
                reason: "implicit intent (intent filters are not modelled)"
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
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "another package's activity"
            }
        );
    }

    #[test]
    fn refuses_a_class_the_package_does_not_declare_with_class_not_found() {
        let splash = decl(SPLASH);
        let c = Call {
            component: Some((PACKAGE, MAIN)),
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&splash))),
            Refusal::Code {
                code: START_CLASS_NOT_FOUND,
                reason: "class not declared by the package"
            }
        );
        let none = Target {
            process: PACKAGE,
            decl: None,
            in_task: false,
            top_is_class: false,
        };
        assert_eq!(
            code_of(check(&c, none)),
            Refusal::Code {
                code: START_CLASS_NOT_FOUND,
                reason: "class not declared by the package"
            }
        );
    }

    fn start_of(r: Result<(&ActivityDecl, Start), Refusal>) -> Start {
        r.expect("accepted").1
    }

    /// singleTop reuses only the top (ActivityStarter.java:2441-2448), singleTask any instance in the task, clearing above (:2510-2533); none to reuse: a new instance.
    #[test]
    fn single_top_and_task_reuse_their_instance_else_start_new() {
        let mut main = decl(MAIN);
        fn in_task(m: &ActivityDecl) -> Target<'_> {
            Target {
                in_task: true,
                ..target(m)
            }
        }
        fn on_top(m: &ActivityDecl) -> Target<'_> {
            Target {
                in_task: true,
                top_is_class: true,
                ..target(m)
            }
        }
        main.launch_mode = LAUNCH_SINGLE_TOP;
        assert_eq!(start_of(check(&call(), target(&main))), Start::New);
        assert_eq!(start_of(check(&call(), in_task(&main))), Start::New);
        assert_eq!(
            start_of(check(&call(), on_top(&main))),
            Start::Reuse { clear_top: false }
        );
        main.launch_mode = LAUNCH_SINGLE_TASK;
        assert_eq!(start_of(check(&call(), target(&main))), Start::New);
        assert_eq!(
            start_of(check(&call(), in_task(&main))),
            Start::Reuse { clear_top: true }
        );
        main.launch_mode = LAUNCH_MODE_STANDARD;
        assert_eq!(start_of(check(&call(), on_top(&main))), Start::New);
        main.launch_mode = 3; // singleInstance
        assert_eq!(
            code_of(check(&call(), target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "launch mode singleInstance / singleInstancePerTask"
            }
        );
    }

    #[test]
    fn refuses_an_activity_declared_in_another_process() {
        let mut main = decl(MAIN);
        main.process = Some(format!("{PACKAGE}:remote"));
        assert_eq!(
            code_of(check(&call(), target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "activity in another process"
            }
        );
    }

    #[test]
    fn an_activity_declared_in_the_package_process_is_accepted() {
        let mut main = decl(MAIN);
        main.process = Some(PACKAGE.to_owned());
        assert!(check(&call(), target(&main)).is_ok());
    }

    #[test]
    fn refuses_intent_flags_other_than_new_task() {
        let main = decl(MAIN);
        let clear_top = 0x0400_0000;
        let c = Call {
            flags: clear_top,
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "intent flags other than NEW_TASK"
            }
        );
    }

    #[test]
    fn refuses_new_task_into_another_affinity() {
        let mut main = decl(MAIN);
        main.task_affinity = Some("org.example.tasks".to_owned());
        let c = Call {
            flags: FLAG_ACTIVITY_NEW_TASK,
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "NEW_TASK into another task affinity"
            }
        );
    }
}
