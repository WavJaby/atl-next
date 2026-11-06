// SPDX-License-Identifier: MIT
//! atl-am: `am start -n <package>/<class>` reduced to the binder call, as the shell uid with no caller app: IActivityTaskManager.startActivity(caller null, "com.android.shell", intent with
//! the component and FLAG_ACTIVITY_NEW_TASK), what ActivityManagerShellCommand.runStartActivity ends in. atld spawns it for its control command `start` (a home screen tap later in the run).
//! Prints `am: start <package>/<class> -> <START code>`; exit 0 = a success code (>= 0, ActivityManager.START_SUCCESS and the delivered/to-front codes).

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate atl_parcelables;
extern crate binder;

use std::io::Write;

use anyhow::{Context, Result, bail};
use atl_aidl::aidl::android::app::IActivityTaskManager::IActivityTaskManager;
use atl_parcelables::android::content::{ComponentName, Intent};
use binder::{ProcessState, Strong};

/// The calling package the shell's starts carry (ActivityManagerShellCommand: SHELL_PACKAGE_NAME).
const SHELL_PACKAGE: &str = "com.android.shell";
/// Intent.FLAG_ACTIVITY_NEW_TASK: a start from outside an activity needs it (ContextImpl.startActivity).
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [verb, component] = args.as_slice() else {
        bail!("usage: atl-am start <package>/<class>");
    };
    if verb != "start" {
        bail!("atl-am: only `start` is modelled, not {verb}");
    }
    let Some((package, class)) = component.split_once('/') else {
        bail!("atl-am: component {component} is not <package>/<class>");
    };
    // a class starting with `.` is relative to the package, as ComponentName.unflattenFromString reads it
    let class = match class.strip_prefix('.') {
        Some(rest) => format!("{package}.{rest}"),
        None => class.to_owned(),
    };
    ProcessState::start_thread_pool();
    let atm: Strong<dyn IActivityTaskManager> =
        binder::check_interface("activity_task").context("getService activity_task")?;
    let intent = Intent {
        component: Some(ComponentName {
            package: package.to_owned(),
            class: class.clone(),
        }),
        flags: FLAG_ACTIVITY_NEW_TASK,
        ..Default::default()
    };
    let code = atm
        .startActivity(
            None,
            Some(SHELL_PACKAGE),
            None,
            Some(&intent),
            None,
            None,
            None,
            -1,
            0,
            None,
            None,
        )
        .context("startActivity")?;
    writeln!(std::io::stderr(), "am: start {package}/{class} -> {code}")?;
    if code < 0 {
        bail!("am: start refused with {code}");
    }
    Ok(())
}
