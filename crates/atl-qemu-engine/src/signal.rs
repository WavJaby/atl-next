// SPDX-License-Identifier: MIT
//! The fault chain for SIGSEGV and SIGBUS, in front of QEMU's handler and behind ART's sigchain.
//! QEMU's `signal_init` (linux-user/signal.c) overwrites every core-dump signal's handler without chaining the old one, and its `host_sigsegv_handler` calls `die_from_signal` for a fault it does not own.
//! So: [`capture_previous`] before QEMU's `main` records the handler the process had (sigchain's user action, or the app's); [`install`] at BOOT, after `signal_init`, sets [`atl_fault`] as the action in front of QEMU's.
//! With libsigchain linked (AOSP art/sigchainlib: this executable's `sigaction`), the order per signal is sigchain's special handlers (ART's fault manager), then [`atl_fault`], then QEMU's handler for a guest fault or the previous handler for a host fault.
//! A fault is a guest fault when its thread is a guest thread and QEMU's own tests say so, in QEMU's order: `helper_retaddr != 0` (a helper or the translator touching guest memory, accel/tcg/user-exec.c `adjust_signal_pc`); the host pc in the code buffer (`in_code_gen_buffer(pc - tcg_splitwx_diff)`); or a write with SEGV_ACCERR to a guest page with `PAGE_WRITE_ORG`, which QEMU write-protected for translated code.
//! The last case is the one QEMU un-protects (`page_unprotect` invalidates the translations; the host write retries). For any other SEGV_ACCERR write QEMU dies, so only that case is routed.
//! A guest thread registers its `helper_retaddr` at its first trap (BOOT, or the THREAD trap in trap.rs). A thread that faults before that is classified as a host fault; closing that window needs a QEMU thread-start hook, which is a patch (owner decision).
//! Signal context: atomics, thread-local `Cell`s, data set before install, `in_code_gen_buffer`, `page_get_flags` (QEMU's own handler calls it in the same context; it may take QEMU's mmap lock) and `sigaction`; nothing allocates. [`atl_fault`] holds no value with a destructor: QEMU's handler may longjmp over its frame.

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

/// Per signal (index as [`SIGNALS`]): the handler before QEMU's `main`, and QEMU's.
struct Chain {
    previous: [libc::sigaction; 2],
    qemu: [libc::sigaction; 2],
}

/// The previous handlers alone, captured before QEMU's handlers exist.
struct Previous([libc::sigaction; 2]);

// SAFETY: sigaction records hold handler addresses and masks only; written once before any reader.
unsafe impl Sync for Previous {}
// SAFETY: as for Previous.
unsafe impl Sync for Chain {}
// SAFETY: as for Previous.
unsafe impl Send for Previous {}
// SAFETY: as for Previous.
unsafe impl Send for Chain {}

static PREVIOUS: OnceLock<Previous> = OnceLock::new();
static CHAIN: OnceLock<Chain> = OnceLock::new();
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

/// Faults routed so far, by the reason they were classified as guest faults, and those that went to the previous handler.
#[derive(Clone, Copy, Debug, Default)]
pub struct Routed {
    /// `helper_retaddr != 0`: a fault in a QEMU helper, or in the translator's fetch.
    pub helper: u64,
    /// The host pc in the code buffer: a fault in translated code.
    pub code: u64,
    /// A write to a write-protected translated-code page: QEMU un-protects it and the write retries.
    pub accerr: u64,
    /// Not a guest fault: the previous handler.
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

fn query(sig: c_int) -> libc::sigaction {
    // SAFETY: a zeroed sigaction is a valid out buffer.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: query only (null new action); old is a valid out pointer.
    let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
    assert_eq!(rc, 0, "sigaction({sig}) query failed");
    old
}

/// Records the process's current SIGSEGV/SIGBUS handlers. Before QEMU's `main` ([`crate::run`]).
pub(crate) fn capture_previous() {
    let _ = PREVIOUS.set(Previous(SIGNALS.map(query)));
}

/// Registers the calling thread as a guest thread. Its `helper_retaddr` is resolved on this thread, so it is this thread's instance. Returns the slot.
pub(crate) fn register_thread() -> usize {
    let slot = helper_retaddr_slot().expect("QEMU library exports helper_retaddr") as usize;
    SLOT.with(|s| s.set(slot));
    slot
}

/// Puts [`atl_fault`] in front of QEMU's handlers and registers the BOOT thread. At BOOT: after QEMU's `signal_init`, on the guest thread.
pub(crate) fn install() {
    if CHAIN.get().is_some() {
        return;
    }
    let previous = PREVIOUS.get().expect("capture_previous before install").0;
    BOOT_SLOT.store(register_thread(), Ordering::Release);
    let qemu = SIGNALS.map(query);
    if CHAIN.set(Chain { previous, qemu }).is_err() {
        return;
    }
    for (i, sig) in SIGNALS.into_iter().enumerate() {
        // QEMU's mask and flags (sigfillset, SA_SIGINFO): the same blocking as the handler it replaces
        let mut act = qemu[i];
        act.sa_sigaction = atl_fault as *const () as usize;
        act.sa_flags |= libc::SA_SIGINFO;
        // SAFETY: act is a complete sigaction (copied from the handler query); the handler has the SA_SIGINFO signature.
        let rc = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
        assert_eq!(rc, 0, "sigaction({sig}) install failed");
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

/// Why a fault belongs to QEMU, or None for the previous handler. A thread that is not a guest thread never belongs to QEMU: QEMU's handler would read `thread_cpu`, which is NULL there.
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
    // break `drop-accerr` (gate f): the write-protected page's fault goes to the previous handler
    (!cfg!(atl_qe_drop_accerr) && is_tb_write(info, uc)).then_some(Reason::Accerr)
}

/// Runs `act`'s handler for `sig` as the kernel would have. SIG_DFL / SIG_IGN: restore that disposition and return, so the faulting instruction re-runs under it.
fn deliver(act: &libc::sigaction, sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    match act.sa_sigaction {
        libc::SIG_DFL | libc::SIG_IGN => {
            // SAFETY: act is the complete record captured by sigaction.
            unsafe { libc::sigaction(sig, act, std::ptr::null_mut()) };
        }
        h if act.sa_flags & libc::SA_SIGINFO != 0 => {
            // SAFETY: an SA_SIGINFO handler takes (int, siginfo_t *, void *).
            let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                unsafe { std::mem::transmute(h) };
            f(sig, info, uc);
        }
        h => {
            // SAFETY: a plain handler takes (int).
            let f: extern "C" fn(c_int) = unsafe { std::mem::transmute(h) };
            f(sig);
        }
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

extern "C" fn atl_fault(sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    let Some(chain) = CHAIN.get() else {
        return;
    };
    let i = usize::from(sig != SIGNALS[0]);
    // break `fault-route` (gate d): no guest fault reaches QEMU; break `chain-order` (gate e): every fault, host ones too, goes to QEMU
    let reason = if cfg!(atl_qe_fault_route) {
        None
    } else {
        classify(info, uc)
    };
    if let Some(reason) = reason {
        count(reason);
        deliver(&chain.qemu[i], sig, info, uc);
    } else if cfg!(atl_qe_chain_order) {
        deliver(&chain.qemu[i], sig, info, uc);
    } else {
        HOST_FAULTS.fetch_add(1, Ordering::Relaxed);
        deliver(&chain.previous[i], sig, info, uc);
    }
}
