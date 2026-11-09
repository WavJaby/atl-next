// SPDX-License-Identifier: MIT
//! Spike gate g3 (bridge-1 route N): a SIGSEGV raised in guest code reaches a host handler installed through the engine (`engine::install_signal`) on the host thread pointer, and the guest resumes on its own.
//! Stages:
//! - A, always: guest = `g3_guest_load` below on a stand-in guest TCB: `mov (%rdi), %eax` faults on NULL; the handler records its FS, sets RAX = 41, skips the 2-byte load; the guest adds 1 and stores the FS it resumed on. Want: handler FS = host, resumed FS = guest, return 42.
//! - Control (the probe responds), always: the same fault with the handler installed by plain `sigaction`, no engine entry -> the handler must see the guest FS.
//! - B, with a native-bridge build: guest bionic `strlen(NULL)`; the handler emulates a return (RIP = [RSP], RSP += 8, RAX = 7), valid while the fault is before strlen's first push (bionic x86_64 SSE2 strlen pushes nothing). Want: handler FS = host, 7, the guest FS the crossing saw at return = the guest TCB, host intact.
//!
//! The handler touches no TLS (atomics, `rdfsbase`, the ucontext), so the control is safe on the guest FS.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::ffi::c_void;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use atl_native_bridge::engine::{self, Engine, HostThread, Tp, arch};
use common::{host_intact, need};

/// Words of the stand-in guest TCB of stage A (as g1 stage 0).
const FAKE_TCB_WORDS: usize = 64;
/// Bytes of `mov (%rdi), %eax` (8b 07), the faulting load the handler skips in stage A.
const LOAD_BYTES: i64 = 2;
/// RAX the handler gives stage A's load; the guest adds 1.
const LOADED: i64 = 41;
/// Stage B's emulated strlen return value.
const STRLEN_RET: i64 = 7;

std::arch::global_asm!(
    ".text",
    ".globl g3_guest_load",
    "g3_guest_load:",
    "    mov (%rdi), %eax",
    "    add $1, %eax",
    "    rdfsbase %rdx",
    "    mov %rdx, g3_resumed_fs(%rip)",
    "    ret",
    ".bss",
    ".p2align 3",
    ".globl g3_resumed_fs",
    "g3_resumed_fs: .quad 0",
    ".text",
    options(att_syntax)
);

unsafe extern "C" {
    fn g3_guest_load(p: usize) -> usize;
    static g3_resumed_fs: AtomicUsize;
}

/// How the handler resumes: 0 skip the load (stage A), 1 emulate a return (stage B).
static MODE: AtomicUsize = AtomicUsize::new(0);
static HANDLER_FS: AtomicUsize = AtomicUsize::new(0);
static FAULT_ADDR: AtomicUsize = AtomicUsize::new(usize::MAX);
static HITS: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_segv(_: i32, info: *mut libc::siginfo_t, uc: *mut c_void) {
    HANDLER_FS.store(arch::fs_base(), Ordering::SeqCst);
    HITS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the kernel passes a valid siginfo and ucontext to an SA_SIGINFO handler.
    unsafe {
        FAULT_ADDR.store((*info).si_addr() as usize, Ordering::SeqCst);
        let g = &mut (*uc.cast::<libc::ucontext_t>()).uc_mcontext.gregs;
        let (rip, rsp, rax) = (
            libc::REG_RIP as usize,
            libc::REG_RSP as usize,
            libc::REG_RAX as usize,
        );
        if MODE.load(Ordering::SeqCst) == 0 {
            g[rip] += LOAD_BYTES;
            g[rax] = LOADED;
        } else {
            g[rip] = *(g[rsp] as *const i64);
            g[rsp] += 8;
            g[rax] = STRLEN_RET;
        }
    }
}

fn reset() {
    HANDLER_FS.store(0, Ordering::SeqCst);
    FAULT_ADDR.store(usize::MAX, Ordering::SeqCst);
    // SAFETY: an atomic in the asm's .bss.
    unsafe { g3_resumed_fs.store(0, Ordering::SeqCst) };
}

/// Plain sigaction, no engine entry: the control's handler runs on whatever FS the fault left.
fn install_plain() -> Result<(), String> {
    // SAFETY: zeroed sigaction is empty; on_segv has the SA_SIGINFO signature.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_segv as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut())
    };
    if rc != 0 {
        return Err("sigaction failed".into());
    }
    Ok(())
}

/// Stage A's fault on a stand-in TCB; (handler FS, resumed FS, return, guest TCB).
fn fault_a() -> Result<(usize, usize, usize, usize), String> {
    let self0 = common::self0();
    let host = arch::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    // SAFETY: a leaked FAKE_TCB_WORDS-word stand-in TCB (bionic's: 11 words); the record is leaked.
    unsafe { arch::init_tcb(tcb, &raw mut *ht) };
    reset();
    MODE.store(0, Ordering::SeqCst);
    // SAFETY: g3_guest_load reads [rdi] (NULL -> the handler resumes it) and touches no TLS.
    let ret = unsafe {
        arch::Identity.call(
            ht,
            Tp::Guest(tcb),
            g3_guest_load as *const () as usize,
            &[0; 4],
        )
    };
    if arch::fs_base() != host {
        // SAFETY: this thread's own thread pointer, read above.
        unsafe { arch::set_fs_base(host) };
        return Err("FS not host after the guest call".into());
    }
    common::probe(self0)?;
    if FAULT_ADDR.load(Ordering::SeqCst) != 0 {
        return Err(format!(
            "fault address {:#x}, want 0",
            FAULT_ADDR.load(Ordering::SeqCst)
        ));
    }
    // SAFETY: as in reset.
    let resumed = unsafe { g3_resumed_fs.load(Ordering::SeqCst) };
    Ok((
        HANDLER_FS.load(Ordering::SeqCst),
        resumed,
        ret as u32 as usize,
        tcb,
    ))
}

fn stage_a() -> Result<(), String> {
    let host = arch::fs_base();
    engine::install_signal(libc::SIGSEGV, on_segv)?;
    let (hfs, resumed, ret, tcb) = fault_a().map_err(|e| format!("stage A: {e}"))?;
    if hfs != host {
        return Err(format!(
            "stage A: handler ran on FS {hfs:#x}, host {host:#x} (guest {tcb:#x})"
        ));
    }
    if resumed != tcb {
        return Err(format!(
            "stage A: guest resumed on FS {resumed:#x}, want {tcb:#x}"
        ));
    }
    if ret != LOADED as usize + 1 {
        return Err(format!("stage A: returned {ret}, want {}", LOADED + 1));
    }
    println!(
        "g3: stage A ok: handler on host FS {host:#x}, guest resumed on {tcb:#x}, returned {ret}"
    );
    Ok(())
}

fn control() -> Result<(), String> {
    install_plain()?;
    let (hfs, _, _, tcb) = fault_a().map_err(|e| format!("control: {e}"))?;
    if hfs != tcb {
        return Err(format!(
            "control: unwrapped handler ran on FS {hfs:#x}, expected the guest's {tcb:#x} (the probe does not respond)"
        ));
    }
    println!("g3: control ok: without the engine entry the handler runs on the guest FS {tcb:#x}");
    Ok(())
}

fn stage_b(env: &common::Nb) -> Result<(), String> {
    let self0 = common::self0();
    let host = arch::fs_base();
    let mut g = common::start(env)?;
    host_intact(&g, self0, host, "__linker_init")?;
    engine::install_signal(libc::SIGSEGV, on_segv)?;
    let dlopen = need(&g.linker, "__dl___loader_dlopen")?;
    let dlsym = need(&g.linker, "__dl___loader_dlsym")?;
    // SAFETY: __loader_dlopen / __loader_dlsym (const char*, int | const char*, const void* caller).
    let strlen = unsafe {
        let h = g.call(
            dlopen,
            c"libc.so".as_ptr() as usize,
            common::RTLD_NOW,
            g.exe_entry,
        );
        g.call(dlsym, h, c"strlen".as_ptr() as usize, g.exe_entry)
    };
    if strlen == 0 {
        return Err("stage B: guest dlsym(strlen) = NULL".into());
    }
    let tp = g.thread().guest_fs();
    reset();
    MODE.store(1, Ordering::SeqCst);
    let before = HITS.load(Ordering::SeqCst);
    // SAFETY: strlen(NULL) faults on its first load; the handler emulates its return.
    let ret = unsafe { g.call(strlen, 0, 0, 0) };
    host_intact(&g, self0, host, "guest strlen(NULL)")?;
    if HITS.load(Ordering::SeqCst) != before + 1 {
        return Err("stage B: the handler did not run once".into());
    }
    let hfs = HANDLER_FS.load(Ordering::SeqCst);
    if hfs != host {
        return Err(format!(
            "stage B: handler ran on FS {hfs:#x}, host {host:#x}"
        ));
    }
    if ret != STRLEN_RET as usize {
        return Err(format!("stage B: strlen returned {ret}, want {STRLEN_RET}"));
    }
    if g.thread().guest_fs() != tp {
        return Err(format!(
            "stage B: guest FS at return {:#x}, want {tp:#x}",
            g.thread().guest_fs()
        ));
    }
    println!(
        "g3: stage B ok: guest bionic strlen(NULL) -> host handler on host FS, emulated return {ret} on guest FS {tp:#x}"
    );
    Ok(())
}

fn main() -> ExitCode {
    let a = stage_a();
    let c = control();
    let b = match common::nb_env() {
        Some(env) => Some(stage_b(&env)),
        None => {
            println!("g3: stage B SKIP (ATL_NB / ATL_NB_LIBCXX unset)");
            None
        }
    };
    common::verdict("g3", [Some(a), Some(c), b])
}
