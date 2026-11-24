// SPDX-License-Identifier: MIT
//! How an installer run ended, as a scan and a session take it. The installer (atl-app-launcher installer.rs) exits 0 with its frames when the package parses, and exits 1 with the parser's verdict `parse error <code>: <message>` on stderr when it does not. Any other end has no verdict: a signal death (a seccomp kill is SIGSYS, named with its audit record by `death`) or an exit without one. PMS skips only a package that fails to parse (InstallPackageHelper "Failed to parse"); a killed installer is AOSP's "Unexpected exception occurred while parsing", which fails the start.

use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;

use crate::Profile;
use crate::control::Response;
use crate::seccomp::death;

/// The head of a failed run's text: `atl-installer exited <status>`. The billing harness (xtask-harness billing.rs) finds a run's first error by it.
pub const HEAD: &str = "atl-installer exited";

/// The verdict line `parse error <code>: <message>`: a code (negative for an INSTALL_PARSE_FAILED_* value) then `: `. A line that only contains the words is not one.
fn is_verdict(line: &str) -> bool {
    let Some((_, rest)) = line.split_once("parse error ") else {
        return false;
    };
    let Some((code, _)) = rest.split_once(": ") else {
        return false;
    };
    let digits = code.strip_prefix('-').unwrap_or(code);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// How an installer run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerEnd {
    /// Exit 0: the frames are the package.
    Parsed,
    /// The parser's verdict: the package does not parse. A scan skips it; a session fails.
    Refused(String),
    /// No verdict: a signal death or an exit without one. Starts with [`HEAD`]. A scan fails the start; a session fails.
    Failed(String),
}

impl InstallerEnd {
    /// The end of `installer`'s run that exited `status` with `stderr`.
    pub fn of(status: &ExitStatus, stderr: &str, installer: &Path) -> InstallerEnd {
        if status.success() {
            return InstallerEnd::Parsed;
        }
        let stderr = stderr.trim();
        // the verdict counts from a clean exit 1 only: a parser that printed it and then died is a failure, not a refusal
        if status.code() == Some(1)
            && let Some(line) = stderr.lines().find(|l| is_verdict(l))
        {
            return InstallerEnd::Refused(line.trim().to_owned());
        }
        let why = match death(Profile::Installer, status) {
            Some(line) => line,
            None if status.signal().is_some() => format!(
                "killed by a signal, not a seccomp kill: dmesg for {}",
                installer.display()
            ),
            None => "no parse verdict".to_owned(),
        };
        InstallerEnd::Failed(format!("{}: {why}\n{stderr}", head(status)))
    }

    /// The answer to a scan of `dir` (control `Scan`): the frames file at `frames` when parsed, the verdict as `Refused`, else an error naming the package and the end.
    pub fn scan_reply(&self, dir: &str, frames: &str) -> Response {
        match self {
            InstallerEnd::Parsed => Response::Staged {
                frames: frames.to_owned(),
            },
            InstallerEnd::Refused(verdict) => Response::Refused {
                message: verdict.clone(),
            },
            InstallerEnd::Failed(text) => Response::Error {
                message: format!("scan {dir}: {text}"),
            },
        }
    }
}

/// `atl-installer exited <status>`: the head of a refusal's or a failure's text (a session's error starts with it too).
pub fn head(status: &ExitStatus) -> String {
    format!("{HEAD} {status}")
}

#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "a test: the installer's end is a real child (sh kills itself or exits with a verdict); no runtime process starts"
)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A real child running `script` under sh: its status and stderr.
    fn ended(script: &str) -> (ExitStatus, String) {
        let out = Command::new("/bin/sh")
            .args(["-c", script])
            .output()
            .expect("sh");
        (
            out.status,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn of(script: &str) -> InstallerEnd {
        let (status, stderr) = ended(script);
        InstallerEnd::of(&status, &stderr, Path::new("/x/atl-installer"))
    }

    #[test]
    fn exit_zero_is_parsed() {
        assert_eq!(of("exit 0"), InstallerEnd::Parsed);
    }

    #[test]
    fn the_verdict_of_a_clean_exit_one_is_a_refusal() {
        assert_eq!(
            of("echo 'atl-installer: parse error -2: not an APK' >&2; exit 1"),
            InstallerEnd::Refused("atl-installer: parse error -2: not an APK".into())
        );
    }

    #[test]
    fn a_sigsys_end_fails_with_its_signal_and_audit_record() {
        // a real SIGSYS death, the signal a seccomp kill leaves
        let (status, stderr) = ended("kill -s SYS $$");
        let InstallerEnd::Failed(text) =
            InstallerEnd::of(&status, &stderr, Path::new("/x/atl-installer"))
        else {
            panic!("a SIGSYS end is a failure, not a refusal");
        };
        assert!(text.starts_with(&head(&status)), "{text}");
        assert!(text.contains("type=1326"), "{text}");
    }

    #[test]
    fn a_verdict_before_a_signal_death_is_a_failure() {
        // the parser printed its verdict, then died: no clean exit 1, so no refusal
        assert!(matches!(
            of("echo 'atl-installer: parse error -2: x' >&2; kill -s SEGV $$"),
            InstallerEnd::Failed(_)
        ));
    }

    #[test]
    fn words_that_are_not_a_verdict_fail() {
        assert!(matches!(
            of("echo 'atl-installer: XML parse error at line 3' >&2; exit 1"),
            InstallerEnd::Failed(_)
        ));
    }

    #[test]
    fn an_exit_without_a_verdict_fails() {
        assert!(matches!(
            of("echo 'atl-installer: create_vm failed' >&2; exit 1"),
            InstallerEnd::Failed(_)
        ));
    }
}
