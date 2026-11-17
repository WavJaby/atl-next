// SPDX-License-Identifier: MIT
#![cfg(atl_nb_identity)]
//! Spike gate g3 (bridge-1 route N): a SIGSEGV raised in guest code reaches a host handler installed through the engine (`engine::install_signal`) on the host thread pointer, and the guest resumes on its own.
//! Stages:
//! - A, always: guest = `g3_guest_load` below on a stand-in guest TCB: `mov (%rdi), %eax` faults on NULL; the handler records its FS, sets RAX = 41, skips the 2-byte load; the guest adds 1 and stores the FS it resumed on. Want: handler FS = host, resumed FS = guest, return 42.
//! - Control (the probe responds), always: the same fault with the handler installed by plain `sigaction`, no engine entry -> the handler must see the guest FS.
//! - B, with a native-bridge build: guest bionic `strlen(NULL)`; the handler emulates a return (RIP = [RSP], RSP += 8, RAX = 7), valid while the fault is before strlen's first push (bionic x86_64 SSE2 strlen pushes nothing). Want: handler FS = host, 7, the guest FS the crossing saw at return = the guest TCB, host intact.
//! - C, with a native-bridge build: guest `sigaction(SIGSEGV, NULL, &old)`, then guest code calls `old.sa_sigaction` (a guest crash reporter chaining to the action it replaced), with the host handler unwrapped and then wrapped. Want: never the raw host address; one address both times; the handler runs once, on the host FS.
//! - D, with a native-bridge build: the install hook registered, a host handler installed late by plain `sigaction`, delivered by guest `raise`. Want: handler on host FS, install counter +1.
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

use atl_native_bridge::engine::{self, Engine, Guest, HostThread, Tp, identity};
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
    HANDLER_FS.store(identity::fs_base(), Ordering::SeqCst);
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
    let host = identity::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    // SAFETY: a leaked FAKE_TCB_WORDS-word stand-in TCB (bionic's: 11 words); the record is leaked.
    unsafe { identity::init_tcb(tcb, &raw mut *ht) };
    reset();
    MODE.store(0, Ordering::SeqCst);
    // SAFETY: g3_guest_load reads [rdi] (NULL -> the handler resumes it) and touches no TLS.
    let ret = unsafe {
        identity::Identity.call(
            ht,
            Tp::Guest(tcb),
            g3_guest_load as *const () as usize,
            &[0; 4],
        )
    };
    if identity::fs_base() != host {
        // SAFETY: this thread's own thread pointer, read above.
        unsafe { identity::set_fs_base(host) };
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
    let host = identity::fs_base();
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

/// Guest libc's `name` (`__loader_dlopen("libc.so")` + `__loader_dlsym`); 0 when absent.
fn guest_libc(g: &mut Guest, name: &std::ffi::CStr) -> Result<usize, String> {
    let dlopen = need(&g.linker, "__dl___loader_dlopen")?;
    let dlsym = need(&g.linker, "__dl___loader_dlsym")?;
    // SAFETY: __loader_dlopen / __loader_dlsym (const char*, int | const char*, const void* caller).
    Ok(unsafe {
        let h = g.call(
            dlopen,
            c"libc.so".as_ptr() as usize,
            common::RTLD_NOW,
            g.exe_entry,
        );
        g.call(dlsym, h, name.as_ptr() as usize, g.exe_entry)
    })
}

fn stage_b(g: &mut Guest, self0: libc::pthread_t, host: usize) -> Result<(), String> {
    engine::install_signal(libc::SIGSEGV, on_segv)?;
    let strlen = guest_libc(g, c"strlen")?;
    if strlen == 0 {
        return Err("stage B: guest dlsym(strlen) = NULL".into());
    }
    let tp = g.thread().guest_tp();
    reset();
    MODE.store(1, Ordering::SeqCst);
    let before = HITS.load(Ordering::SeqCst);
    // SAFETY: strlen(NULL) faults on its first load; the handler emulates its return.
    let ret = unsafe { g.call(strlen, 0, 0, 0) };
    host_intact(g, self0, host, "guest strlen(NULL)")?;
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
    if g.thread().guest_tp() != tp {
        return Err(format!(
            "stage B: guest FS at return {:#x}, want {tp:#x}",
            g.thread().guest_tp()
        ));
    }
    println!(
        "g3: stage B ok: guest bionic strlen(NULL) -> host handler on host FS, emulated return {ret} on guest FS {tp:#x}"
    );
    Ok(())
}

/// Stage C: guest `sigaction(SIGSEGV, NULL, &old)` then a call of `old.sa_sigaction` from guest code, as a guest crash reporter chains to the action it replaced.
/// Twice: first with the host handler installed unwrapped (plain `sigaction`: the old action is the handler itself), then wrapped (the guest sigaction rewrapped it: the old action is the arch's entry, which must answer what it wraps). Both must name one guest-callable address that runs the handler on the host FS.
fn stage_c(g: &mut Guest, self0: libc::pthread_t, host: usize) -> Result<(), String> {
    let sigaction = guest_libc(g, c"sigaction")?;
    if sigaction == 0 {
        return Err("stage C: guest dlsym(sigaction) = NULL".into());
    }
    install_plain()?;
    let tp = g.thread().guest_tp();
    // SAFETY: C records for which all-zero is a valid value.
    let (mut info, mut uc): (libc::siginfo_t, libc::ucontext_t) =
        unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    let mut seen = [0usize; 2];
    for (i, how) in ["unwrapped", "wrapped"].iter().enumerate() {
        // bionic LP64 struct sigaction: flags, handler, mask, restorer (signal.rs `BionicSigaction`)
        let mut old = [0usize; 4];
        // SAFETY: bionic `int sigaction(int, const struct sigaction*, struct sigaction*)`; `old` is its 4-word out record.
        let rc = unsafe {
            g.call(
                sigaction,
                libc::SIGSEGV as usize,
                0,
                old.as_mut_ptr() as usize,
            )
        };
        host_intact(g, self0, host, "guest sigaction")?;
        if rc as u32 != 0 {
            return Err(format!("stage C ({how}): guest sigaction returned {rc}"));
        }
        let handler = old[1];
        if handler == on_segv as *const () as usize {
            return Err(format!(
                "stage C ({how}): the guest's old action is the raw host handler {handler:#x}"
            ));
        }
        reset();
        MODE.store(0, Ordering::SeqCst);
        let before = HITS.load(Ordering::SeqCst);
        // SAFETY: the old action's handler, called from guest code as a chaining guest handler does, with a zeroed siginfo / ucontext (on_segv's stage A mode only rewrites the ucontext's registers).
        unsafe {
            g.call(
                handler,
                libc::SIGSEGV as usize,
                &raw mut info as usize,
                &raw mut uc as usize,
            )
        };
        host_intact(g, self0, host, "guest call of the old action")?;
        if HITS.load(Ordering::SeqCst) != before + 1 {
            return Err(format!(
                "stage C ({how}): old action {handler:#x} did not reach the host handler once"
            ));
        }
        let hfs = HANDLER_FS.load(Ordering::SeqCst);
        if hfs != host {
            return Err(format!(
                "stage C ({how}): host handler ran on FS {hfs:#x}, host {host:#x} (guest {tp:#x})"
            ));
        }
        seen[i] = handler;
    }
    if seen[0] != seen[1] {
        return Err(format!(
            "stage C: old action {:#x} unwrapped, {:#x} wrapped: the entry must answer what it wraps",
            seen[0], seen[1]
        ));
    }
    println!(
        "g3: stage C ok: guest old action {:#x} runs the host handler on host FS {host:#x}",
        seen[0]
    );
    Ok(())
}

// this executable's `sigaction` & co. (exported, build.rs), as the launcher's loader defines them: the late install of stage D binds here
atl_sigaction_hook::interpose!();

/// Stage D's handler: records its FS only (no TLS).
extern "C" fn on_usr1(_: i32, _: *mut libc::siginfo_t, _: *mut c_void) {
    HANDLER_FS.store(identity::fs_base(), Ordering::SeqCst);
    HITS.fetch_add(1, Ordering::SeqCst);
}

/// Stage D: the install hook registered (`hook_installs`, as NB `initialize` does), then a host library installs a SIGUSR1 handler late with plain `sigaction` (bound to this executable's interposer), then guest bionic `raise(SIGUSR1)` delivers it while the guest FS is loaded.
/// Want: the handler runs once on the host FS, and the install counter says the hook wrapped it. Without the hook the handler runs on the guest FS (Alto's Odyssey: JavaScriptCore's SIGXCPU handler, installed after `initialize`).
fn stage_d(g: &mut Guest, self0: libc::pthread_t, host: usize) -> Result<(), String> {
    let raise = guest_libc(g, c"raise")?;
    if raise == 0 {
        return Err("stage D: guest dlsym(raise) = NULL".into());
    }
    if !engine::signal::hook_installs() {
        return Err("stage D: no interposer setter in this executable".into());
    }
    let before = engine::signal::wrapped_at_install();
    // SAFETY: zeroed sigaction is empty; on_usr1 has the SA_SIGINFO signature.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_usr1 as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut())
    };
    if rc != 0 {
        return Err("stage D: sigaction(SIGUSR1) failed".into());
    }
    let tp = g.thread().guest_tp();
    reset();
    let hits = HITS.load(Ordering::SeqCst);
    // SAFETY: bionic `int raise(int)`.
    let ret = unsafe { g.call(raise, libc::SIGUSR1 as usize, 0, 0) };
    host_intact(g, self0, host, "guest raise")?;
    if ret as u32 != 0 || HITS.load(Ordering::SeqCst) != hits + 1 {
        return Err(format!(
            "stage D: raise returned {ret}, handler ran {} times, want once",
            HITS.load(Ordering::SeqCst) - hits
        ));
    }
    let hfs = HANDLER_FS.load(Ordering::SeqCst);
    if hfs != host {
        return Err(format!(
            "stage D: late-installed host handler ran on FS {hfs:#x}, host {host:#x} (guest {tp:#x})"
        ));
    }
    let wrapped = engine::signal::wrapped_at_install() - before;
    if wrapped != 1 {
        return Err(format!(
            "stage D: install hook wrapped {wrapped} handlers, want 1"
        ));
    }
    println!(
        "g3: stage D ok: late host sigaction(SIGUSR1) wrapped at install ({wrapped}), guest raise ran it on host FS {host:#x}"
    );
    Ok(())
}

fn main() -> ExitCode {
    let mut v = common::Verdict::new("g3");
    v.stage(Some(stage_a()));
    v.stage(Some(control()));
    let Some(env) = common::nb_env() else {
        println!("g3: stage B, C, D SKIP (ATL_NB / ATL_NB_LIBCXX unset)");
        v.stage(None);
        v.stage(None);
        v.stage(None);
        return v.finish();
    };
    let self0 = common::self0();
    let host = identity::fs_base();
    let started = common::start(&env).and_then(|g| {
        host_intact(&g, self0, host, "__linker_init")?;
        Ok(g)
    });
    match started {
        Ok(mut g) => {
            v.stage(Some(stage_b(&mut g, self0, host)));
            v.stage(Some(stage_c(&mut g, self0, host)));
            // last: once the hook is registered, every later install in this process is wrapped (stage C's unwrapped case and the control need none)
            v.stage(Some(stage_d(&mut g, self0, host)));
        }
        Err(e) => {
            v.stage(Some(Err(e.clone())));
            v.stage(Some(Err(e.clone())));
            v.stage(Some(Err(e)));
        }
    }
    v.finish()
}
