// SPDX-License-Identifier: MIT
//! Gate g5: the signal entry's last-chance report fires when it finds no host thread pointer (`no_host_fs` -> `fault_report`).
//! Child (`g5 --child`): SIGUSR1 goes through the engine's signal entry while FS points at a zeroed fake TCB (not a glibc TCB, no guest-state slot, no bootstrap record) -> the entry must
//! write the report on stderr, then still run the host handler (TLS-free, sets a flag) and put the fake FS back; the child restores its own FS and exits 0 when the handler ran.
//! Parent: runs the child, checks its exit and that stderr carries the report line naming sig 10 and the fake FS. Code that runs only when things break needs a test that breaks it.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]
#![allow(
    clippy::disallowed_types,
    reason = "the gate re-runs its own test binary to read the report on the child's stderr; not a runtime process"
)]

use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};

use atl_native_bridge::engine::arch;

/// What `no_host_fs` passes as `why` (x86_64.rs).
const WHY: &str = "atl-native-bridge: fatal-signal report: no host thread pointer for the handler: sig 10 ";
/// The fake TCB: one page, zeroed -> `%fs:16` (tcbhead_t.self) != FS and every bionic slot 0.
const FAKE_LEN: usize = 4096;

static HANDLED: AtomicBool = AtomicBool::new(false);

/// The host handler the entry calls; runs on the fake FS, so it touches no TLS.
extern "C" fn on_usr1(_sig: i32, _info: *mut libc::siginfo_t, _uc: *mut libc::c_void) {
    HANDLED.store(true, Ordering::Release);
}

fn child() -> ExitCode {
    if !arch::supported() {
        eprintln!("g5: FAIL the engine is not supported here (no FSGSBASE)");
        return ExitCode::FAILURE;
    }
    arch::set_signal_handler(libc::SIGUSR1 as usize, on_usr1 as *const () as usize);
    // SAFETY: zeroed sigaction is empty; the engine entry has the SA_SIGINFO signature.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = arch::signal_entry();
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut())
    };
    if rc != 0 {
        eprintln!("g5: FAIL sigaction");
        return ExitCode::FAILURE;
    }
    let fake = vec![0u8; FAKE_LEN].into_boxed_slice();
    let fake_fs = fake.as_ptr() as usize;
    println!("g5: fake fs {fake_fs:#x}");
    // SAFETY: between the two wrfsbase only raw syscalls run (no TLS use); FS is restored before any Rust code.
    unsafe {
        std::arch::asm!(
            "rdfsbase {own}",
            "wrfsbase {fake}",
            "mov eax, {getpid}",
            "syscall",
            "mov edi, eax",
            "mov esi, eax",
            "mov edx, {sig}",
            "mov eax, {tgkill}",
            "syscall",
            "wrfsbase {own}",
            own = out(reg) _,
            fake = in(reg) fake_fs,
            getpid = const libc::SYS_getpid,
            tgkill = const libc::SYS_tgkill,
            sig = const libc::SIGUSR1,
            out("rax") _, out("rdi") _, out("rsi") _, out("rdx") _, out("rcx") _, out("r11") _,
        );
    }
    drop(fake);
    if HANDLED.load(Ordering::Acquire) {
        ExitCode::SUCCESS
    } else {
        eprintln!("g5: FAIL the host handler did not run");
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--child") {
        return child();
    }
    let out = match std::env::current_exe().and_then(|exe| Command::new(exe).arg("--child").output()) {
        Ok(o) => o,
        Err(e) => {
            println!("g5: FAIL spawning the child: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let fake = stdout
        .lines()
        .find_map(|l| l.strip_prefix("g5: fake fs "))
        .unwrap_or("?");
    let mut whys = Vec::new();
    if !out.status.success() {
        whys.push(format!("child {}: {}", out.status, stderr.trim()));
    }
    match stderr.lines().find(|l| l.starts_with(WHY)) {
        None => whys.push(format!("no report line {WHY:?} on stderr: {:?}", stderr.trim())),
        Some(l) if !l.contains(&format!(" fs {fake} ")) => {
            whys.push(format!("report does not name the fake fs {fake}: {l}"))
        }
        Some(_) => {}
    }
    if whys.is_empty() {
        println!("g5: pass no_host_fs report fired (fs {fake}), host handler ran, FS restored");
        ExitCode::SUCCESS
    } else {
        for w in &whys {
            println!("g5: FAIL {w}");
        }
        ExitCode::FAILURE
    }
}
