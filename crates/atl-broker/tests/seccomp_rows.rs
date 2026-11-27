// SPDX-License-Identifier: MIT
//! The kill-default seccomp filters (installer, binderd) against the substituted-call table (atl_broker::seccomp_newer) and the stat family, plus the seccomp death report.
//! Each probe re-runs this test binary as a child with MODE_ENV set: a filter binds the thread that applies it, and the harness must stay unfiltered. A probe prints one `probe:` line; a kill ends
//! the child before it. Raw syscalls need `unsafe`, which atl-broker's own crate forbids: hence an integration test, applying the exact bytes bwrap reads (atl_broker::seccomp_filter).
#![expect(
    clippy::disallowed_types,
    reason = "a test: the probe re-runs this test binary unfiltered"
)]

use atl_broker::seccomp_newer::{Answer, ROWS, Row};
use atl_broker::{Profile, seccomp_death, seccomp_deaths, seccomp_filter, seccomp_inet_filter};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, ExitStatus};

const MODE_ENV: &str = "ATL_SECCOMP_ROWS_PROBE";
const DIR_ENV: &str = "ATL_SECCOMP_ROWS_DIR";

/// `profile`'s filter applied to this thread from the bytes bwrap reads (sock_filter: code u16, jt u8, jf u8, k u32, host order).
fn apply(profile: Profile) {
    apply_net(profile, true);
}

/// [`apply`] for a process that holds INTERNET (`net`) or not: the app profiles' INTERNET rule depends on it.
fn apply_net(profile: Profile, net: bool) {
    apply_bytes(&seccomp_filter(profile, net).expect("filter bytes"));
}

/// The filter bytes of one program, applied to this thread.
fn apply_bytes(bytes: &[u8]) {
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
        Some("rows-inet") => rows_probe(true),
        Some("rows-no-inet") => rows_probe(false),
        Some("rows-no-priority-inet") => inet_only_probe(true),
        Some("rows-no-priority-no-inet") => inet_only_probe(false),
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

/// The x32 ABI's bit on x86_64 syscall numbers: a call under `nr | X32` passes a rule keyed on the plain number unless the filter names both.
const X32: i64 = 0x4000_0000;

/// `ok` when the call returned >= 0 (a socket or ring fd, closed when the child exits), else `err:<errno>`.
fn answer(rc: i64) -> String {
    if rc >= 0 {
        "ok".to_owned()
    } else {
        format!(
            "err:{}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        )
    }
}

/// socket(domain, type, 0) as a raw call. The probe's filter is already applied.
fn socket_row(domain: i32, kind: i32, nr: i64) -> String {
    // SAFETY: socket(2) takes integers only; the answer is a fd or an errno
    let rc = unsafe { libc::syscall(nr, i64::from(domain), i64::from(kind), 0_i64) };
    answer(rc)
}

/// io_uring_setup(1, zeroed params) as a raw call (struct io_uring_params, 120 bytes).
fn ring_row(nr: i64) -> String {
    let mut params = [0_u8; 120];
    // SAFETY: the kernel reads and writes one io_uring_params at params (120 bytes, zeroed); the probe child exits at once
    let rc = unsafe { libc::syscall(nr, 1_i64, params.as_mut_ptr()) };
    answer(rc)
}

/// Under the app filter, with INTERNET (`net`) or without: socket(AF_INET, SOCK_STREAM), socket(AF_INET6, SOCK_DGRAM), socket(AF_UNIX, SOCK_STREAM), io_uring_setup,
/// and on x86_64 the x32 numbers of the socket and of io_uring_setup (`inet_x32`, `io_uring_x32`). One line; the child exits after it.
fn rows_probe(net: bool) -> ! {
    apply_net(Profile::App, net);
    print_rows()
}

/// `--no-app-seccomp`'s program ([`seccomp_inet_filter`]): the INTERNET rules and io_uring_setup, without the thread-priority rule. Same rows as [`rows_probe`].
fn inet_only_probe(net: bool) -> ! {
    apply_bytes(&seccomp_inet_filter(net).expect("inet filter bytes"));
    print_rows()
}

/// The [`rows_probe`] rows under the filter already applied, one line; the child exits after it.
fn print_rows() -> ! {
    let mut rows = vec![
        (
            "inet",
            socket_row(libc::AF_INET, libc::SOCK_STREAM, libc::SYS_socket),
        ),
        (
            "inet6",
            socket_row(libc::AF_INET6, libc::SOCK_DGRAM, libc::SYS_socket),
        ),
        (
            "unix",
            socket_row(libc::AF_UNIX, libc::SOCK_STREAM, libc::SYS_socket),
        ),
        ("io_uring", ring_row(libc::SYS_io_uring_setup)),
    ];
    if cfg!(target_arch = "x86_64") {
        rows.push((
            "inet_x32",
            socket_row(libc::AF_INET, libc::SOCK_STREAM, libc::SYS_socket + X32),
        ));
        rows.push(("io_uring_x32", ring_row(libc::SYS_io_uring_setup + X32)));
    }
    let line: Vec<String> = rows.iter().map(|(k, v)| format!("{k}={v}")).collect();
    finish(&line.join(" "))
}

/// The app filter's two INTERNET-related rules, per the process's INTERNET right. Without it AF_INET and AF_INET6 sockets are refused EPERM (AOSP's netd answer, loopback
/// included), AF_UNIX and the other calls pass. io_uring_setup is EACCES either way: AOSP's app_neverallows.te denies anon_inode, so no app socket can come through
/// IORING_OP_SOCKET. The x32 numbers answer as the plain ones (x86_64). Break: removing either rule turns the matching key red.
#[test]
fn app_filter_refuses_inet_sockets_without_internet_and_io_uring_always() {
    let no_net = probe_line("rows-no-inet", &[]);
    assert_eq!(no_net["inet"], "err:1");
    assert_eq!(no_net["inet6"], "err:1");
    assert_eq!(no_net["unix"], "ok");
    assert_eq!(no_net["io_uring"], "err:13");
    let net = probe_line("rows-inet", &[]);
    assert_eq!(net["inet"], "ok");
    assert_eq!(net["inet6"], "ok");
    assert_eq!(net["unix"], "ok");
    assert_eq!(net["io_uring"], "err:13");
    if cfg!(target_arch = "x86_64") {
        assert_eq!(no_net["inet_x32"], "err:1");
        assert_eq!(no_net["io_uring_x32"], "err:13");
        assert_eq!(net["io_uring_x32"], "err:13");
    }
}

/// `--no-app-seccomp` (PM Q3): without the thread-priority rule the INTERNET rules and io_uring_setup still hold; expected values per AOSP (EPERM, EACCES).
#[test]
fn no_priority_filter_keeps_the_inet_rules_and_io_uring() {
    let no_net = probe_line("rows-no-priority-no-inet", &[]);
    assert_eq!(no_net["inet"], "err:1");
    assert_eq!(no_net["inet6"], "err:1");
    assert_eq!(no_net["unix"], "ok");
    assert_eq!(no_net["io_uring"], "err:13");
    let net = probe_line("rows-no-priority-inet", &[]);
    assert_eq!(net["inet"], "ok");
    assert_eq!(net["inet6"], "ok");
    assert_eq!(net["io_uring"], "err:13");
    if cfg!(target_arch = "x86_64") {
        assert_eq!(no_net["inet_x32"], "err:1");
        assert_eq!(no_net["io_uring_x32"], "err:13");
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
