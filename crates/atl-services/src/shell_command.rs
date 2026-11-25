// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/PackageManagerShellCommand.java, frameworks/libs/modules-utils/java/com/android/modules/utils/BasicShellCommandHandler.java
//! `cmd package` as the package service answers it through the shell-command hook (PackageManager::shell_command, libbinder_rs patch 0011).
//! Only `list packages` exists; its output and error wording are AOSP's (PackageManagerShellCommand.runList / runListPackages, BasicShellCommandHandler.handleDefaultCommands).

use std::io::Write;

use crate::packages::Packages;

/// `cmd package <args>` for the one command answered here: `list packages`, one `package:<name>` line per package `caller_uid` may see.
/// Every line goes to `out` (AOSP prints through getOutPrintWriter); `_err` is never written. Status -1 on error, as AOSP. Write errors are ignored, as AOSP's PrintWriter ignores them.
pub fn run_package(
    packages: &Packages,
    caller_uid: u32,
    args: &[&str],
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> i32 {
    match args {
        ["list", "packages"] => {
            for p in packages.all() {
                if packages.visible(caller_uid, p.name()).is_some() {
                    let _ = writeln!(out, "package:{}", p.name());
                }
            }
            0
        }
        ["list", "packages", opt, ..] => {
            let _ = writeln!(out, "Error: Unknown option: {opt}");
            -1
        }
        ["list"] => {
            let _ = writeln!(out, "Error: didn't specify type of data to list");
            -1
        }
        ["list", kind, ..] => {
            let _ = writeln!(out, "Error: unknown list type '{kind}'");
            -1
        }
        // AOSP prints its help here (BasicShellCommandHandler.java:301-302, onHelp to out); this subset has no help text: status -1 only.
        [] => -1,
        [cmd, ..] => {
            let _ = writeln!(out, "Unknown command: {cmd}");
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::PackageRecord;
    use crate::packages::fixture::record;
    use std::collections::BTreeMap;

    const SHELL: u32 = 2000;

    /// com.a (uid 10001) and com.b (uid 10002): two third-party apps.
    fn two() -> Packages {
        let b = PackageRecord {
            uid: 10_002,
            ..record("com.b")
        };
        Packages::new(vec![record("com.a"), b], BTreeMap::new())
    }

    fn run(packages: &Packages, uid: u32, args: &[&str]) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = run_package(packages, uid, args, &mut out, &mut err);
        (
            status,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn list_packages_as_shell_prints_every_package() {
        assert_eq!(
            run(&two(), SHELL, &["list", "packages"]),
            (
                0,
                "package:com.a\npackage:com.b\n".to_owned(),
                String::new()
            )
        );
    }

    #[test]
    fn list_packages_leaves_out_a_package_the_caller_may_not_see() {
        // com.a's uid sees itself, not com.b (another app's uid).
        assert_eq!(
            run(&two(), 10_001, &["list", "packages"]),
            (0, "package:com.a\n".to_owned(), String::new())
        );
    }

    /// BasicShellCommandHandler.handleDefaultCommands: `Unknown command: <cmd>` on out, status -1.
    #[test]
    fn unknown_command_fails_with_text_on_out() {
        assert_eq!(
            run(&two(), SHELL, &["install"]),
            (-1, "Unknown command: install\n".to_owned(), String::new())
        );
    }

    /// The `list` arguments this command does not answer: AOSP's text on out, status -1, nothing on err.
    #[test]
    fn list_arguments_fail_with_text_on_out() {
        for (args, text) in [
            (
                &["list"][..],
                "Error: didn't specify type of data to list\n",
            ),
            (&["list", "bogus"][..], "Error: unknown list type 'bogus'\n"),
            (
                &["list", "packages", "-f"][..],
                "Error: Unknown option: -f\n",
            ),
        ] {
            assert_eq!(
                run(&two(), SHELL, args),
                (-1, text.to_owned(), String::new())
            );
        }
    }

    /// No arguments: AOSP prints its help (see run_package); this subset prints nothing, status -1.
    #[test]
    fn no_arguments_fail_with_status_only() {
        assert_eq!(run(&two(), SHELL, &[]), (-1, String::new(), String::new()));
    }
}
