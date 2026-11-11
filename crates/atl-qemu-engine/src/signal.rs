// SPDX-License-Identifier: MIT
//! SIGSEGV and SIGBUS: guest faults through a special handler, everything else through the process's user action.
//! [`special`] is a sigchain special handler (AOSP art/sigchainlib `SigchainAction`). In an ART process ART registers it: NativeBridge `getSignalHandler` (native_bridge.h, interface >= 2) answers the bridge's forwarder to this engine's `fault` entry, and `InitializeNativeBridge` (art/runtime/native_bridge_art_interface.cc) adds it with a full `sc_mask` and `SIGCHAIN_ALLOW_NORETURN`, behind ART's FaultManager (fault_handler.cc). The spike harness registers it the same way.
//! Sigchain's `Handler` (sigchain.cc) runs the specials in order, then the user action. A guest fault: [`special`] runs QEMU's handler and claims it. Anything else: declined, so the user action runs.
//! The other core-dump signals (FPE, ILL, QUIT, TRAP), which `signal_init` also takes: [`install`] gives them back to the pre-QEMU action ([`CORE_SIGNALS`] says why that is safe for guest delivery).
//! The user action: QEMU's `signal_init` (linux-user/signal.c) overwrites it with `host_signal_handler` and never chains the old one, and that handler calls `die_from_signal` for a fault it does not own. So [`capture_previous`] before QEMU's `main` records the action the process had, and [`install`] at BOOT, after `signal_init`, takes QEMU's for [`special`] and puts the previous one back.
//! Why a special, not the user action: a host `sigaction(SIGSEGV)` after boot (a game engine, a crash reporter; sigchain records it as the user action) replaces only the user action, so guest faults still reach QEMU, and host faults reach that newer handler as the kernel would deliver them.
//! No-return: QEMU's handler leaves a guest fault by `siglongjmp` into its CPU loop (`cpu_loop_exit_sigsegv`). Without `SIGCHAIN_ALLOW_NORETURN` sigchain sets its per-thread "handling" flag around the special and the longjmp skips the restore, so that thread's later faults skip every special (sigchain.cc 440-470). The mask sigchain sets for the special (`sc_mask`) does not outlive the longjmp: QEMU restores the interrupted mask first (`host_sigsegv_handler`: `sigprocmask(SIG_SETMASK, host_signal_mask(uc))`).
//! A fault is a guest fault when its thread is a guest thread and QEMU's own tests say so, in QEMU's order: `helper_retaddr != 0` (a helper or the translator touching guest memory, accel/tcg/user-exec.c `adjust_signal_pc`); the host pc in the code buffer (`in_code_gen_buffer(pc - tcg_splitwx_diff)`); or a write with SEGV_ACCERR to a guest page with `PAGE_WRITE_ORG`, which QEMU write-protected for translated code.
//! The last case is the one QEMU un-protects (`page_unprotect` invalidates the translations; the host write retries). For any other SEGV_ACCERR write QEMU dies, so only that case is routed.
//! Guest threads: every bridge guest thread is a host thread with an attached CPU, registered at attach (attach.rs); the BOOT thread at [`install`]; a raw guest clone (QEMU's own clone path) at its THREAD trap (trap.rs). Detach unregisters. A raw clone that faults before its THREAD trap is classified as a host fault; closing that window needs a QEMU thread-start hook, which is a patch (owner decision).
//! `sigaction` route: the plain symbol, the one QEMU's own objects bind to (in the engine `.so` both share one dynamic reference; xtask qemu_spike.rs checks it is undefined there). The process's global lookup binds it: libsigchain's in an ART process, whose launcher lists libsigchain before libc (xtask packages.rs). Engine and QEMU therefore always read and write the same table. Raw syscalls only where sigchain must be bypassed ([`kernel_default`], the fatal unblock).
//! Signal context: atomics, thread-local `Cell`s, data set before install, `in_code_gen_buffer`, `page_get_flags` (QEMU's own handler calls it in the same context; it may take QEMU's mmap lock) and `sigaction`; nothing allocates. [`special`] holds no value with a destructor: QEMU's handler may longjmp over its frame.

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::engine::{helper_retaddr_slot, lib};

const SIGNALS: [c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];
/// QEMU's `PAGE_WRITE_ORG` (include/exec/page-protection.h): the page is writable for the guest; QEMU clears the host write permission while it holds translated code.
const PAGE_WRITE_ORG: c_int = 0x0010;
/// x86_64 `REG_ERR` bit 1: the faulting access was a write.
const PF_WRITE: u64 = 0x2;
/// `si_code` of a SIGSEGV protection violation (uapi asm-generic/siginfo.h); the libc crate does not export it.
const SEGV_ACCERR: c_int = 2;

/// The other host signals `signal_init` takes: `core_dump_signal` minus SEGV/BUS, mapped 1:1. Not SIGABRT: QEMU remaps the guest's to a host realtime signal (`signal_table_init`) and installs on that, leaving host SIGABRT alone.
/// QEMU needs none of them on a host thread, and its handler there loads `get_task_state(thread_cpu)` with `thread_cpu` NULL before anything else (`host_signal_handler`), so a host `ud2` / `__builtin_trap` died by SIGSEGV instead of SIGILL. [`install`] gives them back to the process.
/// Guest delivery does not use these host actions: guest ILL/FPE/TRAP from translated code are CPU exceptions queued by the cpu loop (`force_sig`), host ones with `si_code > 0` were `die_from_signal` anyway, and a guest `sigaction` for any of them re-installs QEMU's handler for that host signal (`do_sigaction`).
const CORE_SIGNALS: [c_int; 4] = [libc::SIGFPE, libc::SIGILL, libc::SIGQUIT, libc::SIGTRAP];

/// Per signal (index as the signal list it was taken for): a sigaction record.
struct Actions<const N: usize>([libc::sigaction; N]);

// SAFETY: sigaction records hold handler addresses and masks only; written once before any reader.
unsafe impl<const N: usize> Sync for Actions<N> {}
// SAFETY: as for Sync.
unsafe impl<const N: usize> Send for Actions<N> {}

/// The user action before QEMU's `main`; [`install`] restores it.
static PREVIOUS: OnceLock<Actions<2>> = OnceLock::new();
/// As [`PREVIOUS`], for [`CORE_SIGNALS`].
static PREVIOUS_CORE: OnceLock<Actions<4>> = OnceLock::new();
/// QEMU's handlers as `signal_init` installed them; [`special`] runs them for guest faults. Set at [`install`]: until then [`special`] declines everything.
static QEMU: OnceLock<Actions<2>> = OnceLock::new();
/// `helper_retaddr` of the BOOT thread: the only slot the `boot-only` break reads.
static BOOT_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's `helper_retaddr` instance (dlsym ran on this thread); 0 for a thread that is not a guest thread.
    static SLOT: Cell<usize> = const { Cell::new(0) };
}

/// Why a fault belongs to QEMU (module doc order).
#[derive(Clone, Copy)]
enum Reason {
    Helper,
    Code,
    Accerr,
}

/// Faults [`special`] saw so far, by the reason they were classified as guest faults, and those it declined.
#[derive(Clone, Copy, Debug, Default)]
pub struct Routed {
    /// `helper_retaddr != 0`: a fault in a QEMU helper, or in the translator's fetch.
    pub helper: u64,
    /// The host pc in the code buffer: a fault in translated code.
    pub code: u64,
    /// A write to a write-protected translated-code page: QEMU un-protects it and the write retries.
    pub accerr: u64,
    /// Not a guest fault: passed on to the user action (or ended as the kernel would, [`host_disposition`]).
    pub host: u64,
}

static HELPER_FAULTS: AtomicU64 = AtomicU64::new(0);
static CODE_FAULTS: AtomicU64 = AtomicU64::new(0);
static ACCERR_FAULTS: AtomicU64 = AtomicU64::new(0);
static HOST_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Faults routed so far.
pub fn routed() -> Routed {
    Routed {
        helper: HELPER_FAULTS.load(Ordering::Relaxed),
        code: CODE_FAULTS.load(Ordering::Relaxed),
        accerr: ACCERR_FAULTS.load(Ordering::Relaxed),
        host: HOST_FAULTS.load(Ordering::Relaxed),
    }
}

/// The current action for `sig` (under sigchain: its user-action record); None if `sigaction` fails.
fn current(sig: c_int) -> Option<libc::sigaction> {
    // SAFETY: a zeroed sigaction is a valid out buffer.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: query only (null new action); old is a valid out pointer.
    (unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } == 0).then_some(old)
}

fn query(sig: c_int) -> libc::sigaction {
    current(sig).unwrap_or_else(|| panic!("sigaction({sig}) query failed"))
}

/// Records the process's current actions for [`SIGNALS`] and [`CORE_SIGNALS`]. Before QEMU's `main` ([`crate::run`]).
pub(crate) fn capture_previous() {
    let _ = PREVIOUS.set(Actions(SIGNALS.map(query)));
    let _ = PREVIOUS_CORE.set(Actions(CORE_SIGNALS.map(query)));
}

/// Registers the calling thread as a guest thread. Its `helper_retaddr` is resolved on this thread, so it is this thread's instance. Returns the slot.
pub(crate) fn register_thread() -> usize {
    let slot = helper_retaddr_slot().expect("QEMU library exports helper_retaddr") as usize;
    SLOT.with(|s| s.set(slot));
    slot
}

/// The calling thread is no guest thread any more (its CPU is detached): its faults are host faults.
pub(crate) fn unregister_thread() {
    SLOT.with(|s| s.set(0));
}

/// At BOOT, after QEMU's `signal_init`, on the guest thread: registers the BOOT thread, takes QEMU's handlers for [`special`] and restores the previous user action.
pub(crate) fn install() {
    if QEMU.get().is_some() {
        return;
    }
    let previous = &PREVIOUS.get().expect("capture_previous before install").0;
    BOOT_SLOT.store(register_thread(), Ordering::Release);
    let qemu = SIGNALS.map(query);
    for (act, sig) in qemu.iter().zip(SIGNALS) {
        // deliver calls it as a three-argument function
        assert!(
            act.sa_flags & libc::SA_SIGINFO != 0 && act.sa_sigaction > libc::SIG_IGN,
            "signal {sig}: QEMU's handler is no SA_SIGINFO function"
        );
    }
    if QEMU.set(Actions(qemu)).is_err() {
        return;
    }
    // break `no-core-restore` (gate w): QEMU's handler keeps the core-dump signals, so a host trap (SIGILL) on a thread with no CPU dies inside it by SIGSEGV
    if !cfg!(atl_qe_no_core_restore) {
        let core = &PREVIOUS_CORE
            .get()
            .expect("capture_previous before install")
            .0;
        for (act, sig) in core.iter().zip(CORE_SIGNALS) {
            // SAFETY: act is a complete record from the capture query.
            let rc = unsafe { libc::sigaction(sig, act, std::ptr::null_mut()) };
            assert_eq!(rc, 0, "sigaction({sig}) restore failed");
        }
    }
    // break `no-restore` (gates e, x): QEMU's handler stays the user action, so host faults reach its die_from_signal; break `user-route` (gate y) keeps it too, as the guest route
    if cfg!(atl_qe_no_restore) || cfg!(atl_qe_user_route) {
        return;
    }
    for (act, sig) in previous.iter().zip(SIGNALS) {
        // SAFETY: act is a complete record from the capture query.
        let rc = unsafe { libc::sigaction(sig, act, std::ptr::null_mut()) };
        assert_eq!(rc, 0, "sigaction({sig}) restore failed");
    }
}

#[cfg(target_arch = "x86_64")]
fn host_pc(uc: *mut c_void) -> usize {
    // SAFETY: the kernel's ucontext for this signal (SA_SIGINFO third argument).
    unsafe { (*uc.cast::<libc::ucontext_t>()).uc_mcontext.gregs[libc::REG_RIP as usize] as usize }
}

#[cfg(target_arch = "x86_64")]
fn host_is_write(uc: *mut c_void) -> bool {
    // SAFETY: as for host_pc.
    let err = unsafe { (*uc.cast::<libc::ucontext_t>()).uc_mcontext.gregs[libc::REG_ERR as usize] };
    (err as u64) & PF_WRITE != 0
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!("atl-qemu-engine: host_pc and host_is_write for this host arch are not written yet");

/// A write with SEGV_ACCERR to a guest page that QEMU write-protected for translated code. QEMU's `host_sigsegv_handler` un-protects it and the write retries. The preconditions are QEMU's (is_write, is_valid, SEGV_ACCERR); the flag is `page_unprotect`'s own test.
fn is_tb_write(info: *mut libc::siginfo_t, uc: *mut c_void) -> bool {
    // SAFETY: the kernel's siginfo of this delivery.
    let (code, addr) = unsafe { ((*info).si_code, (*info).si_addr() as usize) };
    if code != SEGV_ACCERR || !host_is_write(uc) {
        return false;
    }
    let l = lib();
    let guest = addr.wrapping_sub(*l.guest_base);
    // h2g_valid (include/user/guest-host.h): the address lies in the guest space
    if guest > *l.guest_addr_max {
        return false;
    }
    // SAFETY: page_get_flags takes a guest address and answers 0 for one it does not track.
    let flags = unsafe { (l.page_get_flags)(guest as u64) };
    flags & PAGE_WRITE_ORG != 0
}

/// Why a fault belongs to QEMU, or None. A thread that is not a guest thread never belongs to QEMU: QEMU's handler would read `thread_cpu`, which is NULL there.
fn classify(info: *mut libc::siginfo_t, uc: *mut c_void) -> Option<Reason> {
    let slot = if cfg!(atl_qe_boot_only) {
        BOOT_SLOT.load(Ordering::Acquire)
    } else {
        SLOT.with(Cell::get)
    };
    if slot == 0 {
        return None;
    }
    // SAFETY: slot is a guest thread's helper_retaddr instance, read on that thread: a plain word.
    if unsafe { (slot as *const usize).read_volatile() } != 0 {
        return Some(Reason::Helper);
    }
    let pc = host_pc(uc) as isize - *lib().splitwx_diff;
    // SAFETY: in_code_gen_buffer only compares p against the buffer bounds.
    if unsafe { (lib().in_code_gen_buffer)(pc as *const c_void) } {
        return Some(Reason::Code);
    }
    // break `drop-accerr` (gate f): the write-protected page's fault is declined
    (!cfg!(atl_qe_drop_accerr) && is_tb_write(info, uc)).then_some(Reason::Accerr)
}

/// Runs QEMU's handler `act` (an SA_SIGINFO function, checked at [`install`]) as the kernel would have. It returns (a retried write) or longjmps into the CPU loop.
fn deliver(act: &libc::sigaction, sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    // SAFETY: an SA_SIGINFO handler takes (int, siginfo_t *, void *); install checked the flag and that it is a function.
    let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
        unsafe { std::mem::transmute(act.sa_sigaction) };
    f(sig, info, uc);
}

/// Kernel SIG_DFL for `sig`, bypassing sigchain: its `sigaction` only records the action while the signal is claimed, so the kernel would keep the chain's handler. Zeroed kernel `struct sigaction` (handler, flags, restorer, mask) = SIG_DFL, no flags, empty mask.
fn kernel_default(sig: c_int) {
    let dfl = [0u64; 4];
    // SAFETY: raw rt_sigaction(sig, &dfl, NULL, 8); dfl is a kernel sigaction, 8 the kernel sigset size.
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigaction,
            libc::c_long::from(sig),
            dfl.as_ptr(),
            std::ptr::null::<u64>(),
            8usize,
        )
    };
}

/// Ends the process by `sig` from inside the handler: kernel default, unblock, re-send to this thread, which delivers on the way out.
fn die_by(sig: c_int) {
    kernel_default(sig);
    let set = 1u64 << (sig - 1);
    // SAFETY: raw rt_sigprocmask(SIG_UNBLOCK, &set, NULL, 8) on this thread; set is one kernel sigset word. The chain's sigprocmask would strip the claimed signal from the mask.
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::c_long::from(libc::SIG_UNBLOCK),
            &raw const set,
            std::ptr::null::<u64>(),
            8usize,
        )
    };
    // SAFETY: tgkill to this thread: unblocked now, so it is delivered on the way out of the handler, with the kernel default (process exit).
    unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            libc::c_long::from(libc::getpid()),
            libc::c_long::from(libc::gettid()),
            libc::c_long::from(sig),
        )
    };
}

/// A signal that is not the engine's, where sigchain's handling of the user action differs from the kernel's; true when handled here. Otherwise false: sigchain runs the user action.
/// SIG_DFL and a process-sent signal (`si_code <= SI_USER`): QEMU's fatal path `die_with_signal` (linux-user/signal.c) does `sigaction(sig, SIG_DFL)`, `kill(getpid(), sig)`, `sigsuspend`; sigchain's SIG_DFL branch reverts the kernel action and returns, which consumes the signal (exit 1 on one thread, a sleep forever on several). Ended here by the signal, as the kernel would.
/// SIG_IGN and a kernel fault: the kernel cannot ignore a synchronous fault and ends the process; sigchain's SIG_IGN return re-runs the instruction forever. Set to the kernel default, so the re-run ends it.
fn host_disposition(sig: c_int, info: *mut libc::siginfo_t) -> bool {
    // SAFETY: the kernel's siginfo of this delivery.
    let code = unsafe { (*info).si_code };
    let Some(user) = current(sig) else {
        return false;
    };
    match user.sa_sigaction {
        // break `no-fatal` (gates i, j): the kill is left to sigchain's SIG_DFL revert
        libc::SIG_DFL if code <= libc::SI_USER && !cfg!(atl_qe_no_fatal) => {
            die_by(sig);
            true
        }
        // break `ign-loop` (gate k): sigchain's SIG_IGN return re-faults forever
        libc::SIG_IGN if code > libc::SI_USER && !cfg!(atl_qe_ign_loop) => {
            kernel_default(sig);
            true
        }
        _ => false,
    }
}

fn count(reason: Reason) {
    let counter = match reason {
        Reason::Helper => &HELPER_FAULTS,
        Reason::Code => &CODE_FAULTS,
        Reason::Accerr => &ACCERR_FAULTS,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The special handler for SIGSEGV and SIGBUS (`art::SigchainAction::sc_sigaction`): true when the engine took the signal. A guest fault goes to QEMU's handler, which may not return; anything else is declined unless [`host_disposition`] ends it. Declines everything before [`install`].
pub extern "C" fn special(sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) -> bool {
    let Some(qemu) = QEMU.get() else {
        return false;
    };
    let Some(i) = SIGNALS.iter().position(|&s| s == sig) else {
        return false;
    };
    // break `fault-route` (gate d): no guest fault reaches QEMU; break `user-route` (gate y): guest faults go to the user action, which keeps QEMU's handler (install)
    let reason = if cfg!(atl_qe_fault_route) || cfg!(atl_qe_user_route) {
        None
    } else {
        classify(info, uc)
    };
    if let Some(reason) = reason {
        count(reason);
        deliver(&qemu.0[i], sig, info, uc);
        return true;
    }
    // break `chain-order` (gate e): every fault, host ones too, goes to QEMU
    if cfg!(atl_qe_chain_order) {
        deliver(&qemu.0[i], sig, info, uc);
        return true;
    }
    HOST_FAULTS.fetch_add(1, Ordering::Relaxed);
    host_disposition(sig, info)
}
