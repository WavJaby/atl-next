// SPDX-License-Identifier: MIT
//! The sandboxes' seccomp filters, one per [`Profile`]. bwrap installs it (`--seccomp FD`) before the sandboxed program runs, so every thread inherits it.
//! BinderDaemon: a measured allow-list, everything else kills ([`binderd_program`]; derivation in arch.rs).
//! App: the first rule of the planned per-app filter (seccompiler 0.5); every other syscall is allowed, so this is not yet the allow-list.
//! Installer: a measured allow-list like BinderDaemon's ([`arch::INSTALLER_SYSCALLS`]), socket only for AF_UNIX; anything else kills the process.
//! App rule: `setpriority` with a negative priority returns 0 and changes nothing; a non-negative priority reaches the kernel unchanged.
//! Condition defended: an unprivileged user cannot lower nice (RLIMIT_NICE hard limit 0 here; real Android's init sets 40/40, init.rc:42-43). ART starts HandlerThreads at THREAD_PRIORITY_FOREGROUND (-2),
//! so Process.setThreadPriority throws SecurityException (android_util_Process.cpp:115) and the app's queued-work looper dies with the app.
//! Delete the rule when thread priority goes through the desktop (rtkit or the realtime portal): the ideal, not now.
//! The rule grants nothing: a refused priority stays as it is, only the reported result changes. seccomp cannot count, so its evidence is the test below and `xtask run-apk` with and without `--no-app-seccomp`.

mod arch;

use std::collections::BTreeMap;
use std::io;

use crate::Profile;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

/// setpriority(which, who, prio): the priority is the third argument.
const PRIORITY_ARG: u8 = 2;
/// Bit 31 set: the 32-bit int is negative. A Dword comparison reads the argument's low 32 bits, the int the kernel reads, whatever the upper half holds.
const NEGATIVE: u64 = 0x8000_0000;

/// `profile`'s program.
pub fn program(profile: Profile) -> io::Result<BpfProgram> {
    match profile {
        Profile::App => app_program(),
        Profile::BinderDaemon => binderd_program(),
        Profile::Installer => installer_program(),
    }
}

/// socket(domain, type, protocol) and socketpair(domain, type, protocol, sv): the domain is the first argument.
const DOMAIN_ARG: u8 = 0;

/// binderd's filter: an allow-list ([`arch::BINDERD_SYSCALLS`], measured), socketpair only for AF_UNIX, anything else kills the process (SECCOMP_RET_KILL_PROCESS: the run then fails on "binderd ended", never on a half-working daemon).
/// Covers bwrap's pid 1 in the sandbox too (it waits for binderd), since bwrap installs the filter before it forks it.
fn binderd_program() -> io::Result<BpfProgram> {
    allow_list(arch::BINDERD_SYSCALLS, &[libc::SYS_socketpair])
}

/// The installer's filter: an allow-list ([`arch::INSTALLER_SYSCALLS`], measured the same way), socket only for AF_UNIX (its namespace has no network to reach anyway), anything else kills the process.
fn installer_program() -> io::Result<BpfProgram> {
    allow_list(arch::INSTALLER_SYSCALLS, &[libc::SYS_socket])
}

/// An allow-list program: `calls` are allowed with any arguments, each of `unix_only` only when its domain is AF_UNIX, every other call kills the process.
fn allow_list(calls: &[i64], unix_only: &[i64]) -> io::Result<BpfProgram> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = calls.iter().map(|&n| (n, vec![])).collect();
    for &call in unix_only {
        let unix = SeccompCondition::new(
            DOMAIN_ARG,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            u64::from(libc::AF_UNIX.unsigned_abs()),
        )
        .map_err(io::Error::other)?;
        rules.insert(
            call,
            vec![SeccompRule::new(vec![unix]).map_err(io::Error::other)?],
        );
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch::HOST,
    )
    .map_err(io::Error::other)?;
    filter.try_into().map_err(io::Error::other)
}

/// The app filter as seccompiler's program: setpriority with a negative priority returns 0 (`SECCOMP_RET_ERRNO` with errno 0, the kernel's return value -0), every other call is allowed.
fn app_program() -> io::Result<BpfProgram> {
    let negative = SeccompCondition::new(
        PRIORITY_ARG,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ge,
        NEGATIVE,
    )
    .map_err(io::Error::other)?;
    let rule = SeccompRule::new(vec![negative]).map_err(io::Error::other)?;
    let rules = BTreeMap::from([(libc::SYS_setpriority, vec![rule])]);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(0),
        arch::HOST,
    )
    .map_err(io::Error::other)?;
    filter.try_into().map_err(io::Error::other)
}

/// `profile`'s filter as the bytes bwrap reads from `--seccomp`'s fd: a `struct sock_filter` array in host byte order (code u16, jt u8, jf u8, k u32).
pub fn filter(profile: Profile) -> io::Result<Vec<u8>> {
    Ok(program(profile)?
        .iter()
        .flat_map(|i| {
            let mut bytes = Vec::with_capacity(8);
            bytes.extend(i.code.to_ne_bytes());
            bytes.push(i.jt);
            bytes.push(i.jf);
            bytes.extend(i.k.to_ne_bytes());
            bytes
        })
        .collect())
}

#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "a test: the probe re-runs this test binary unfiltered"
)]
mod tests {
    use super::*;
    use rustix::process::{Resource, getpriority_process, getrlimit, setpriority_process};
    use std::collections::HashMap;
    use std::io::Write;
    use std::process::Command;

    /// The probe runs in a child process: the filter binds the thread that applies it, and the test harness must not be filtered.
    const MODE_ENV: &str = "ATL_SECCOMP_PROBE";
    /// The kernel's MAX_NICE (kernel/sched/core.c, `nice_to_rlimit`): an unprivileged nice `n` needs RLIMIT_NICE >= MAX_NICE - n + 1.
    const KERNEL_MAX_NICE: i32 = 19;
    /// The priority Process.setThreadPriority(THREAD_PRIORITY_FOREGROUND) asks for (ART's HandlerThread default in QueuedWork).
    const FOREGROUND: i32 = -2;

    fn rlimit_for(nice: i32) -> u64 {
        u64::try_from(KERNEL_MAX_NICE - nice + 1).expect("a nice value the kernel accepts")
    }

    /// Prints one line: whether each call succeeded (`ok` or `err:<errno>`) and the nice value after it. Run as a child with MODE_ENV set; a no-op otherwise.
    #[test]
    fn probe() {
        let Some(mode) = std::env::var_os(MODE_ENV) else {
            return;
        };
        if mode.to_str() == Some("filtered") {
            seccompiler::apply_filter(&program(Profile::App).expect("program"))
                .expect("apply_filter");
        }
        let outcome = |r: rustix::io::Result<()>| match r {
            Ok(()) => "ok".to_owned(),
            Err(e) => format!("err:{}", e.raw_os_error()),
        };
        let before = getpriority_process(None).expect("getpriority");
        let negative = outcome(setpriority_process(None, FOREGROUND));
        let after_negative = getpriority_process(None).expect("getpriority");
        let plain = outcome(setpriority_process(None, 5));
        let after_plain = getpriority_process(None).expect("getpriority");
        // stdout, not print!: the workspace denies print_stdout (logging goes through tracing)
        let _ = writeln!(
            std::io::stdout(),
            "probe: mode={} before={before} negative={negative} nice_after_negative={after_negative} plain={plain} nice_after_plain={after_plain}",
            mode.to_string_lossy()
        );
    }

    fn run_probe(mode: &str) -> HashMap<String, String> {
        let exe = std::env::current_exe().expect("the test binary");
        let out = Command::new(exe)
            .args([
                "--exact",
                "seccomp::tests::probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MODE_ENV, mode)
            .output()
            .expect("the probe child");
        let text = String::from_utf8_lossy(&out.stdout);
        // libtest writes `test <name> ... ` without a newline before the probe's own line, so the probe text is not at the line's start
        let line = text
            .lines()
            .find_map(|l| l.split_once("probe: ").map(|(_, rest)| rest))
            .unwrap_or_else(|| panic!("no probe line in {text}"));
        line.split_whitespace()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    /// The filter answers a negative priority with success and leaves the nice value alone, and a non-negative priority still reaches the kernel (nice 0 -> 5).
    /// Without the filter the kernel decides: a negative nice succeeds only when RLIMIT_NICE allows it, else EACCES (`err:13`), the SecurityException's cause.
    #[test]
    fn negative_priority_is_answered_by_the_filter_and_refused_by_the_kernel_without_it() {
        let filtered = run_probe("filtered");
        assert_eq!(filtered["negative"], "ok");
        assert_eq!(filtered["nice_after_negative"], filtered["before"]);
        assert_eq!(filtered["plain"], "ok");
        assert_eq!(filtered["nice_after_plain"], "5");

        let plain = run_probe("plain");
        let limit = getrlimit(Resource::Nice).current;
        if limit.is_none_or(|l| l >= rlimit_for(FOREGROUND)) {
            assert_eq!(plain["negative"], "ok");
            assert_eq!(plain["nice_after_negative"], FOREGROUND.to_string());
        } else {
            assert_eq!(plain["negative"], format!("err:{}", libc::EACCES));
            assert_eq!(plain["nice_after_negative"], plain["before"]);
        }
        assert_eq!(plain["plain"], "ok");
        assert_eq!(plain["nice_after_plain"], "5");
    }

    /// bwrap reads whole `sock_filter` instructions: the bytes are the program's instruction count times the struct's size.
    #[test]
    fn filter_bytes_are_whole_instructions() {
        let program = program(Profile::App).expect("program");
        let bytes = filter(Profile::App).expect("bytes");
        assert!(!program.is_empty());
        assert_eq!(
            bytes.len(),
            program.len() * std::mem::size_of::<seccompiler::sock_filter>()
        );
        assert_eq!(std::mem::size_of::<seccompiler::sock_filter>(), 8);
    }
}
