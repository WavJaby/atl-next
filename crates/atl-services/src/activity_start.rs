// SPDX-License-Identifier: MIT
//! Whether the services start an activity for a startActivity call: the accepted subset of ActivityStarter, pure (no binder), so the tests build without the converter feature.
//! Accepted: an explicit component of the caller's own package, started from one of the caller's activities, with no result, in the caller's task and process, standard launch mode,
//! or singleTop / singleTask when no instance of the class is in that task (a new instance, as standard).
//! Every other call is refused: a SecurityException when the caller does not own the activity it names (fail-closed), otherwise the START code AOSP returns for the case
//! (ActivityManager.java:162 FIRST_START_FATAL = -100), counted by `counters::refused` under its reason. Refusals are never silent: the app sees the code, the log sees the count.
//! Refused, not modelled: implicit intents (intent filters), other packages, startActivityForResult, reusing a singleTop / singleTask instance, singleInstance, other processes, intent flags except NEW_TASK.

use crate::counters;
use crate::packages::ActivityDecl;

/// ActivityManager.START_SUCCESS: the activity is started (the record is pushed and its launch sent).
pub const START_SUCCESS: i32 = 0;
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

/// The package the component's class is looked up in: its process (`<application android:process>`, else the package name) and the class declaration, if the package has one.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub process: &'a str,
    pub decl: Option<&'a ActivityDecl>,
    /// An activity of the component's class, not finishing, is in the source's task: what a singleTop / singleTask start would reuse.
    pub in_task: bool,
}

fn refused(code: i32, reason: &'static str) -> Refusal {
    counters::refused(INTERFACE, &format!("startActivity ({reason})"));
    Refusal::Code { code, reason }
}

/// Validates one start. Ok: the declaration of the activity to start; the caller starts it in the source's task.
pub fn check<'a>(call: &Call<'_>, target: Target<'a>) -> Result<&'a ActivityDecl, Refusal> {
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
    if call.request_code >= 0 {
        return Err(refused(START_CANCELED, "startActivityForResult"));
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
    // singleTop / singleTask with no instance of the class in the source's task: ActivityStarter finds nothing to reuse (singleTop: no such top, singleTask: no such activity in the
    // task of its affinity, checked below as for NEW_TASK, which singleTask implies) and starts a new instance on top, as standard. Reusing one (onNewIntent, clear-top) is not modelled.
    match decl.launch_mode {
        LAUNCH_MODE_STANDARD => {}
        LAUNCH_SINGLE_TOP | LAUNCH_SINGLE_TASK if !target.in_task => {}
        LAUNCH_SINGLE_TOP | LAUNCH_SINGLE_TASK => {
            return Err(refused(
                START_CANCELED,
                "singleTop/singleTask instance already in the task (onNewIntent)",
            ));
        }
        _ => {
            return Err(refused(
                START_CANCELED,
                "launch mode singleInstance / singleInstancePerTask",
            ));
        }
    }
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
    Ok(decl)
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
            }),
        }
    }

    fn target(main: &ActivityDecl) -> Target<'_> {
        Target {
            process: PACKAGE,
            decl: Some(main),
            in_task: false,
        }
    }

    fn code_of(r: Result<&ActivityDecl, Refusal>) -> Refusal {
        match r {
            Err(refusal) => refusal,
            Ok(d) => panic!("accepted {}, refusal expected", d.name),
        }
    }

    #[test]
    fn accepts_an_explicit_start_in_the_callers_task() {
        let main = decl(MAIN);
        let accepted = check(&call(), target(&main)).expect("accepted");
        assert_eq!(accepted.name, MAIN);
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
            }),
            ..call()
        };
        assert!(matches!(
            code_of(check(&c, target(&main))),
            Refusal::Security(_)
        ));
    }

    #[test]
    fn refuses_start_for_result() {
        let main = decl(MAIN);
        let c = Call {
            request_code: 7,
            ..call()
        };
        assert_eq!(
            code_of(check(&c, target(&main))),
            Refusal::Code {
                code: START_CANCELED,
                reason: "startActivityForResult"
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
        };
        assert_eq!(
            code_of(check(&c, none)),
            Refusal::Code {
                code: START_CLASS_NOT_FOUND,
                reason: "class not declared by the package"
            }
        );
    }

    #[test]
    fn single_top_and_task_start_new_unless_an_instance_is_in_the_task() {
        let mut main = decl(MAIN);
        for mode in [1, 2] {
            // singleTop, singleTask (ActivityInfo LAUNCH_SINGLE_TOP / LAUNCH_SINGLE_TASK)
            main.launch_mode = mode;
            assert!(check(&call(), target(&main)).is_ok(), "mode {mode}");
            let reuse = Target {
                in_task: true,
                ..target(&main)
            };
            assert_eq!(
                code_of(check(&call(), reuse)),
                Refusal::Code {
                    code: START_CANCELED,
                    reason: "singleTop/singleTask instance already in the task (onNewIntent)"
                }
            );
        }
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
