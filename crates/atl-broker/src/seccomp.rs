// SPDX-License-Identifier: MIT
//! The sandboxes' seccomp filters, one per [`Profile`]. bwrap installs it (`--seccomp FD`) before the sandboxed program runs, so every thread inherits it.
//! BinderDaemon: a measured allow-list, everything else kills ([`binderd_program`]; derivation in arch.rs).
//! App: the first rule of the planned per-app filter (seccompiler 0.5); every other syscall is allowed, so this is not yet the allow-list.
//! Installer: a measured allow-list like BinderDaemon's ([`arch::INSTALLER_SYSCALLS`]), socket only for AF_UNIX; anything else kills the process.
//! App rule (thread priority, two capability tiers chosen by the host's RLIMIT_NICE hard limit H, [`granted_nice`]): the stub raises the soft limit to H ([`NiceTiers`]), so a nice the kernel then accepts from any
//! current value (n >= 20 - H, kernel `can_nice`) reaches it and takes effect ("host-nice"); a lower one is answered 0 and changes nothing ("baseline"). getpriority keeps reporting the kernel's real value.
//! Condition defended: Android grants every app RLIMIT_NICE 40/40 (init.rc `setrlimit nice 40 40`), so its apps never see setpriority fail on their own threads; Process.setThreadPriority turns EACCES into
//! SecurityException (android_util_Process.cpp), which kills e.g. Firebase's executor threads and ART's queued-work looper. Hosts grant less (WSL 0, a GameMode host 30). Retires when H >= 40 (the rule then matches nothing).
//! One static floor, not the kernel's "raising is always allowed": seccomp cannot see the current nice, and a floor keeps lower/restore pairs consistent (both applied, or both ignored).
//! seccomp cannot count: the per-process evidence is the stub's `sandbox-stub: nice` line, the test below, and `xtask test app`'s priority row with and without `--no-app-seccomp`.

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

/// The kernel's nice range ends (include/linux/sched/prio.h MIN_NICE / MAX_NICE).
const MIN_NICE: i32 = -20;
const MAX_NICE: i32 = 19;
/// The RLIMIT_NICE that allows every nice: 20 - MIN_NICE (kernel `nice_to_rlimit`), what Android's init grants.
const FULL_NICE_LIMIT: u64 = 40;

/// RLIMIT_NICE's hard limit this process (and so every app it starts) is granted, capped at [`FULL_NICE_LIMIT`] (unlimited counts as full).
pub fn granted_nice() -> u64 {
    rustix::process::getrlimit(rustix::process::Resource::Nice)
        .maximum
        .map_or(FULL_NICE_LIMIT, |h| h.min(FULL_NICE_LIMIT))
}

/// The app's thread-priority tiers for a hard limit: nice values from `floor` up reach the kernel (allowed from any current nice once soft = hard), lower ones are answered without effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NiceTiers {
    pub granted: u64,
    pub floor: i32,
}

impl NiceTiers {
    /// Kernel `can_nice`: nice n needs RLIMIT_NICE >= 20 - n, so the lowest always-allowed nice is 20 - H.
    pub fn for_limit(granted: u64) -> Self {
        let granted = granted.min(FULL_NICE_LIMIT);
        let floor = MAX_NICE + 1 - i32::try_from(granted).expect("capped at 40");
        NiceTiers { granted, floor }
    }

    /// The rules matching a priority argument whose clamped nice is below the floor (each rule ANDs its conditions, the rules OR); None = no rule (nothing ignored, H = 40).
    /// The argument is compared as the unsigned low 32 bits: negatives are >= 0x8000_0000, and a value above MAX_NICE clamps to MAX_NICE.
    fn ignored(self) -> io::Result<Option<Vec<SeccompRule>>> {
        let cond = |op, v: u64| {
            SeccompCondition::new(PRIORITY_ARG, SeccompCmpArgLen::Dword, op, v)
                .map_err(io::Error::other)
        };
        let rule = |c: Vec<SeccompCondition>| SeccompRule::new(c).map_err(io::Error::other);
        let negative = || cond(SeccompCmpOp::Ge, NEGATIVE);
        Ok(match self.floor {
            f if f > MAX_NICE => Some(vec![]),
            f if f > 0 => Some(vec![
                rule(vec![negative()?])?,
                rule(vec![cond(SeccompCmpOp::Lt, u64::from(f.unsigned_abs()))?])?,
            ]),
            0 => Some(vec![rule(vec![negative()?])?]),
            f if f > MIN_NICE => Some(vec![rule(vec![
                negative()?,
                // two's complement of f as a u32
                cond(SeccompCmpOp::Lt, u64::from(f.cast_unsigned()))?,
            ])?]),
            _ => None,
        })
    }

    /// The stub's log line for this process ([`stub_args`](crate::stub_args) `--nice`).
    pub fn describe(self) -> String {
        let tier = match self.floor {
            f if f > MAX_NICE => "baseline",
            f if f <= MIN_NICE => "host-nice",
            _ => "host-nice+baseline",
        };
        format!(
            "tier={tier} granted={} kernel-applies-nice>={} lower-answered-without-effect",
            self.granted, self.floor
        )
    }
}

/// `profile`'s program.
pub fn program(profile: Profile) -> io::Result<BpfProgram> {
    match profile {
        Profile::App => app_program(NiceTiers::for_limit(granted_nice())),
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

/// The app filter as seccompiler's program: setpriority below `tiers`' floor returns 0 (`SECCOMP_RET_ERRNO` with errno 0, the kernel's return value -0), every other call is allowed.
fn app_program(tiers: NiceTiers) -> io::Result<BpfProgram> {
    let rules: BTreeMap<i64, Vec<SeccompRule>> = tiers
        .ignored()?
        .map(|r| (libc::SYS_setpriority, r))
        .into_iter()
        .collect();
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
    use rustix::process::{
        Resource, Rlimit, getpriority_process, getrlimit, setpriority_process, setrlimit,
    };
    use std::collections::HashMap;
    use std::io::Write;
    use std::process::Command;

    /// The probe runs in a child process: the filter binds the thread that applies it, and the test harness must not be filtered.
    const MODE_ENV: &str = "ATL_SECCOMP_PROBE";
    /// The probe's requests, the app row's sequence (Firebase's executor, ART's HandlerThread): THREAD_PRIORITY_BACKGROUND (10), back to the start value, THREAD_PRIORITY_FOREGROUND (-2).
    const BACKGROUND: i32 = 10;
    const FOREGROUND: i32 = -2;

    /// Prints one line: the start nice, then per request whether it succeeded (`ok` or `err:<errno>`) and the nice after it. Run as a child with MODE_ENV set; a no-op otherwise.
    /// `filtered` first raises the soft limit to the hard one, as the stub does for an app.
    #[test]
    fn probe() {
        let Some(mode) = std::env::var_os(MODE_ENV) else {
            return;
        };
        if mode.to_str() == Some("filtered") {
            let hard = getrlimit(Resource::Nice).maximum;
            setrlimit(
                Resource::Nice,
                Rlimit {
                    current: hard,
                    maximum: hard,
                },
            )
            .expect("soft = hard");
            seccompiler::apply_filter(&program(Profile::App).expect("program"))
                .expect("apply_filter");
        }
        let outcome = |r: rustix::io::Result<()>| match r {
            Ok(()) => "ok".to_owned(),
            Err(e) => format!("err:{}", e.raw_os_error()),
        };
        let before = getpriority_process(None).expect("getpriority");
        let mut line = format!("probe: mode={} before={before}", mode.to_string_lossy());
        for (name, nice) in [
            ("background", BACKGROUND),
            ("restore", before),
            ("foreground", FOREGROUND),
        ] {
            let r = outcome(setpriority_process(None, nice));
            let after = getpriority_process(None).expect("getpriority");
            line += &format!(" {name}={r} after_{name}={after}");
        }
        // stdout, not print!: the workspace denies print_stdout (logging goes through tracing)
        let _ = writeln!(std::io::stdout(), "{line}");
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

    /// Filtered: every request succeeds; a nice at or above the tiers' floor takes effect, a lower one leaves the nice as it was (getpriority reads the kernel's value).
    /// Unfiltered (the break): restoring the start value after BACKGROUND lowers nice, which the kernel refuses with EACCES (`err:13`, the SecurityException's cause) unless the soft limit allows it.
    #[test]
    fn requests_below_the_granted_limit_succeed_without_effect_and_fail_without_the_filter() {
        let tiers = NiceTiers::for_limit(granted_nice());
        let filtered = run_probe("filtered");
        let mut nice: i32 = filtered["before"].parse().expect("a nice");
        let start = nice;
        for (name, want) in [
            ("background", BACKGROUND),
            ("restore", start),
            ("foreground", FOREGROUND),
        ] {
            assert_eq!(filtered[name], "ok", "{name}: {filtered:?}");
            if want >= tiers.floor {
                nice = want;
            }
            assert_eq!(
                filtered[&format!("after_{name}")],
                nice.to_string(),
                "{name}"
            );
        }

        let plain = run_probe("plain");
        let soft = getrlimit(Resource::Nice).current.unwrap_or(FULL_NICE_LIMIT);
        let allowed = |n: i32| u64::try_from(MAX_NICE + 1 - n).is_ok_and(|need| soft >= need);
        let start: i32 = plain["before"].parse().expect("a nice");
        assert_eq!(plain["background"], "ok");
        if allowed(start) {
            assert_eq!(plain["restore"], "ok");
        } else {
            assert_eq!(plain["restore"], format!("err:{}", libc::EACCES));
            assert_eq!(plain["after_restore"], BACKGROUND.to_string());
        }
    }

    /// The floor is kernel `can_nice`'s 20 - H: WSL's 0 ignores every nice, a GameMode host's 30 applies -10..19, Android's 40 (and unlimited, capped) applies all and needs no rule.
    #[test]
    fn tiers_follow_the_kernel_limit_formula() {
        assert_eq!(NiceTiers::for_limit(0).floor, 20);
        assert_eq!(NiceTiers::for_limit(30).floor, -10);
        assert_eq!(NiceTiers::for_limit(40).floor, -20);
        assert_eq!(NiceTiers::for_limit(u64::MAX), NiceTiers::for_limit(40));
        assert!(NiceTiers::for_limit(40).ignored().expect("rules").is_none());
        assert!(
            NiceTiers::for_limit(0)
                .ignored()
                .expect("rules")
                .is_some_and(|r| r.is_empty())
        );
        for h in [0, 1, 19, 20, 21, 30, 39, 40] {
            app_program(NiceTiers::for_limit(h)).expect("a program per limit");
        }
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
