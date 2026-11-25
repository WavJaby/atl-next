// SPDX-License-Identifier: MIT
//! The kill-default seccomp filters (installer, binderd) against the substituted-call table (atl_broker::seccomp_newer) and the stat family, plus the seccomp death report.
//! Each probe re-runs this test binary as a child with MODE_ENV set: a filter binds the thread that applies it, and the harness must stay unfiltered. A probe prints one `probe:` line; a kill ends
//! the child before it. Raw syscalls need `unsafe`, which atl-broker's own crate forbids: hence an integration test, applying the exact bytes bwrap reads (atl_broker::seccomp_filter).
#![expect(
    clippy::disallowed_types,
    reason = "a test: the probe re-runs this test binary unfiltered"
)]

use atl_broker::seccomp_newer::{Answer, ROWS, Row};
use atl_broker::{Profile, seccomp_death, seccomp_deaths, seccomp_filter};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, ExitStatus};

const MODE_ENV: &str = "ATL_SECCOMP_ROWS_PROBE";
const DIR_ENV: &str = "ATL_SECCOMP_ROWS_DIR";

/// `profile`'s filter applied to this thread from the bytes bwrap reads (sock_filter: code u16, jt u8, jf u8, k u32, host order).
fn apply(profile: Profile) {
    let bytes = seccomp_filter(profile).expect("filter bytes");
    let (whole, rest) = bytes.as_chunks::<8>();
    assert!(rest.is_empty(), "whole instructions");
    let program: Vec<seccompiler::sock_filter> = whole
        .iter()
        .map(|c| seccompiler::sock_filter {
            code: u16::from_ne_bytes([c[0], c[1]]),
            jt: c[2],
            jf: c[3],
            k: u32::from_ne_bytes([c[4], c[5], c[6], c[7]]),
        })
        .collect();
    seccompiler::apply_filter(&program).expect("apply_filter");
}

/// The probe's line; then exit, skipping libtest's own exit path (whose calls the filters need not list).
fn finish(line: &str) -> ! {
    let _ = writeln!(std::io::stdout(), "probe: {line}");
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

/// Dispatches a child run; a no-op in the harness's own run.
#[test]
fn probe() {
    let Some(mode) = std::env::var_os(MODE_ENV) else {
        return;
    };
    match mode.to_str() {
        Some("stat") => stat_probe(),
        Some("denied") => denied_probe(),
        Some("table-installer") => table_probe(Profile::Installer, |r| r.installer),
        Some("table-binderd") => table_probe(Profile::BinderDaemon, |r| r.binderd),
        #[cfg(target_arch = "riscv64")]
        Some("hwprobe-installer") => hwprobe_probe(Profile::Installer),
        #[cfg(target_arch = "riscv64")]
        Some("hwprobe-binderd") => hwprobe_probe(Profile::BinderDaemon),
        other => panic!("unknown probe mode {other:?}"),
    }
}

/// Under `profile`'s filter, riscv_hwprobe (258, uapi asm/unistd.h 244 + 14) for no key: 0 where it runs (kernel Documentation/arch/riscv/hwprobe.rst).
#[cfg(target_arch = "riscv64")]
fn hwprobe_probe(profile: Profile) -> ! {
    apply(profile);
    // SAFETY: no pairs and no cpu set: null pointers with zero counts, nothing is read or written
    let rc = unsafe { libc::syscall(258, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64) };
    finish(&format!("mode=hwprobe rc={rc}"))
}

/// Both kill-default lists run ld.so's riscv_hwprobe (gate for the 2026-11-25 kill: glibc 2.43, atl-sandbox-stub under binderd's filter).
#[cfg(target_arch = "riscv64")]
#[test]
fn riscv_hwprobe_runs_under_the_binderd_filter() {
    assert_eq!(probe_line("hwprobe-binderd", &[])["rc"], "0");
}

#[cfg(target_arch = "riscv64")]
#[test]
fn riscv_hwprobe_runs_under_the_installer_filter() {
    assert_eq!(probe_line("hwprobe-installer", &[])["rc"], "0");
}

/// Under the installer filter, the stat family through every path that reaches it: Rust std (statx through glibc's statx wrapper first -- the laptop kill -- then glibc stat64/lstat64/fstat64
/// on an error), glibc stat/lstat/fstat/fstatat directly (newfstatat on x86_64, statx where glibc's FSTATAT_USE_STATX), glibc statx, faccessat with a flag (glibc's faccessat2-first path) and access.
fn stat_probe() -> ! {
    let dir = std::path::PathBuf::from(std::env::var_os(DIR_ENV).expect("the probe dir"));
    let file = dir.join("f");
    std::fs::write(&file, b"x").expect("the probe file");
    let c = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).expect("a path");
    apply(Profile::Installer);
    assert_eq!(std::fs::metadata(&file).expect("std metadata").len(), 1);
    std::fs::symlink_metadata(&file).expect("std symlink_metadata");
    std::fs::File::open(&file)
        .and_then(|f| f.metadata())
        .expect("std File::metadata");
    assert_eq!(std::fs::read(&file).expect("std read"), b"x");
    // SAFETY: glibc calls on a NUL-terminated path with zeroed out-structs of the type each call fills
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        assert_eq!(libc::stat(c.as_ptr(), &raw mut st), 0, "stat");
        assert_eq!(libc::lstat(c.as_ptr(), &raw mut st), 0, "lstat");
        assert_eq!(
            libc::fstatat(libc::AT_FDCWD, c.as_ptr(), &raw mut st, 0),
            0,
            "fstatat"
        );
        let fd = libc::open(c.as_ptr(), libc::O_RDONLY);
        assert!(fd >= 0, "open");
        assert_eq!(libc::fstat(fd, &raw mut st), 0, "fstat");
        libc::close(fd);
        let mut sx: libc::statx = std::mem::zeroed();
        assert_eq!(
            libc::statx(
                libc::AT_FDCWD,
                c.as_ptr(),
                0,
                libc::STATX_BASIC_STATS,
                &raw mut sx
            ),
            0,
            "statx"
        );
        assert_eq!(
            libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::R_OK, libc::AT_EACCESS),
            0,
            "faccessat AT_EACCESS"
        );
        assert_eq!(libc::access(c.as_ptr(), libc::R_OK), 0, "access");
    }
    finish("mode=stat done=1")
}

/// Under the installer filter, a call no list holds and no row answers (getppid): the child must die by SIGSYS before its line.
fn denied_probe() -> ! {
    apply(Profile::Installer);
    let _ = rustix::process::getppid();
    finish("mode=denied survived=1")
}

/// Under `profile`'s filter, every row issued with arguments the kernel rejects (-1 fd / command, null pointers, size 0): prints `<name>=<enosys>:<errno>` per row. An Allow row reaches the
/// kernel (never a kill), an Enosys row gets ENOSYS from the filter; a row the filter kills ends the child before the line.
fn table_probe(profile: Profile, answer: fn(&Row) -> Answer) -> ! {
    let mut line = format!("mode=table-{profile:?}");
    apply(profile);
    for r in ROWS {
        // SAFETY: every argument is invalid (fd and command -1, null pointers, size 0), so the kernel rejects each call before it acts; only the errno is read
        let rc = unsafe { libc::syscall(r.call, -1_i64, 0_i64, 0_i64, -1_i64, 0_i64) };
        let errno = if rc == -1 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        };
        line += &format!(" {}={}:{errno}", r.name, answer(r) == Answer::Enosys);
    }
    finish(&line)
}

fn run(mode: &str, env: &[(&str, &std::ffi::OsStr)]) -> (ExitStatus, String) {
    let out = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "probe", "--nocapture", "--test-threads=1"])
        .envs(env.iter().copied())
        .env(MODE_ENV, mode)
        .output()
        .expect("the probe child");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The probe line's `key=value` pairs; panics naming the child's end when there is none (a kill).
fn probe_line(mode: &str, env: &[(&str, &std::ffi::OsStr)]) -> HashMap<String, String> {
    let (status, text) = run(mode, env);
    // libtest writes `test probe ... ` without a newline before the probe's own line
    let line = text
        .lines()
        .find_map(|l| l.split_once("probe: ").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("{mode}: no probe line (child {status}): {text}"));
    line.split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// The stat family runs under the installer filter whatever this host's glibc and Rust std substitute (gate for the 2026-11-18 laptop kill: Arch glibc 2.44, std's statx).
/// Break-checked 2026-11-18 (x86_64 WSL, glibc 2.41): statx's row removed -> the child dies by SIGSYS at std::fs::metadata, no probe line, this test fails.
#[test]
fn stat_family_runs_under_the_installer_filter() {
    let dir = std::env::temp_dir().join(format!("atl-seccomp-rows-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("probe dir");
    let line = probe_line("stat", &[(DIR_ENV, dir.as_os_str())]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(line["done"], "1");
}

/// Each kill-default profile answers every row as declared (Enosys -> ENOSYS, Allow -> the kernel's own result, never a kill); the rows are unique, named and reasoned.
#[test]
fn rows_are_answered_as_declared() {
    let mut seen = std::collections::HashSet::new();
    for r in ROWS {
        assert!(seen.insert(r.call), "{} twice", r.name);
        assert!(!r.caller.is_empty() && !r.reason.is_empty(), "{}", r.name);
    }
    for mode in ["table-installer", "table-binderd"] {
        let line = probe_line(mode, &[]);
        for r in ROWS {
            let (enosys, errno) = line[r.name].split_once(':').expect("enosys:errno");
            if enosys == "true" {
                assert_eq!(errno, libc::ENOSYS.to_string(), "{mode} {}", r.name);
            }
        }
    }
}

/// The death path fires: a denied call kills the child by SIGSYS, seccomp_death names it with the audit pointer and counts it; bwrap's 128 + SIGSYS exit is recognised; a clean exit is not.
#[test]
fn a_denied_call_is_reported_and_counted() {
    use std::os::unix::process::ExitStatusExt;
    let (status, text) = run("denied", &[]);
    assert!(!text.contains("survived"), "getppid was not denied: {text}");
    assert_eq!(status.signal(), Some(libc::SIGSYS), "{status}");
    let before = seccomp_deaths(Profile::Installer);
    let line = seccomp_death(Profile::Installer, &status).expect("a SIGSYS end is a seccomp death");
    assert!(
        line.contains("SIGSYS") && line.contains("type=1326"),
        "{line}"
    );
    assert_eq!(seccomp_deaths(Profile::Installer), before + 1);
    // bwrap's exit for a child killed by signal s is 128 + s (the laptop's installer: 159); wait status = code << 8
    let bwrap = ExitStatus::from_raw((128 + libc::SIGSYS) << 8);
    assert!(seccomp_death(Profile::Installer, &bwrap).is_some());
    assert!(seccomp_death(Profile::Installer, &ExitStatus::from_raw(0)).is_none());
    assert_eq!(seccomp_deaths(Profile::Installer), before + 2);
}
