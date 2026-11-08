// SPDX-License-Identifier: MIT
//! The fault chain for SIGSEGV and SIGBUS. QEMU's `signal_init` (linux-user/signal.c) overwrites every core-dump signal's handler without chaining the old one, and its `host_sigsegv_handler` calls `die_from_signal` for a fault it does not own.
//! So: [`capture_previous`] before QEMU's `main` (the handler the process had, e.g. ART's sigchain later); [`install`] at BOOT, after `signal_init`, puts [`atl_fault`] in front of QEMU's handler.
//! Guest fault = QEMU's own test: `helper_retaddr != 0` (a helper or the translator touching guest memory, accel/tcg/user-exec.c `adjust_signal_pc`) or the host pc inside the code buffer (`in_code_gen_buffer(pc - tcg_splitwx_diff)`, signal.c `host_sigsegv_handler`). Those go to QEMU's handler, which longjmps into the guest's signal delivery; everything else to the previous handler.
//! Not routed yet (Q3): a host write to a guest page QEMU write-protected for translated code (`handle_sigsegv_accerr_write`), and threads other than the BOOT thread (`helper_retaddr` is resolved there).
//! Signal context: only atomics, loads of data set before install, `in_code_gen_buffer` (QEMU calls it from the same context) and `sigaction`; nothing allocates. [`atl_fault`] holds no value with a destructor: QEMU's handler may longjmp over its frame.

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::engine::{helper_retaddr_slot, lib};

const SIGNALS: [c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];

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
/// `helper_retaddr` of the BOOT thread.
static HELPER_RETADDR: AtomicPtr<usize> = AtomicPtr::new(std::ptr::null_mut());

static GUEST_FAULTS: AtomicU64 = AtomicU64::new(0);
static HOST_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Faults routed so far: (to QEMU's handler, to the previous handler).
pub fn routed() -> (u64, u64) {
    (
        GUEST_FAULTS.load(Ordering::Relaxed),
        HOST_FAULTS.load(Ordering::Relaxed),
    )
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

/// Puts [`atl_fault`] in front of QEMU's handlers. At BOOT: after QEMU's `signal_init`, on the guest thread.
pub(crate) fn install() {
    if CHAIN.get().is_some() {
        return;
    }
    let previous = PREVIOUS.get().expect("capture_previous before install").0;
    let slot = helper_retaddr_slot().expect("QEMU library exports helper_retaddr");
    HELPER_RETADDR.store(slot.cast_mut(), Ordering::Release);
    let qemu = SIGNALS.map(query);
    if CHAIN.set(Chain { previous, qemu }).is_err() {
        return;
    }
    for (i, sig) in SIGNALS.into_iter().enumerate() {
        // QEMU's mask and flags (sigfillset, SA_SIGINFO): the same blocking as the handler it replaces
        let mut act = qemu[i];
        act.sa_sigaction = atl_fault as *const () as usize;
        act.sa_flags |= libc::SA_SIGINFO;
        // SAFETY: act is a complete sigaction (copied from the kernel's answer); the handler has the SA_SIGINFO signature.
        let rc = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
        assert_eq!(rc, 0, "sigaction({sig}) install failed");
    }
}

#[cfg(target_arch = "x86_64")]
fn host_pc(uc: *mut c_void) -> usize {
    // SAFETY: the kernel's ucontext for this signal (SA_SIGINFO third argument).
    unsafe { (*uc.cast::<libc::ucontext_t>()).uc_mcontext.gregs[libc::REG_RIP as usize] as usize }
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!("atl-qemu-engine: host_pc for this host arch is not written yet");

/// QEMU's own ownership test for a host fault (module doc).
fn is_guest_fault(uc: *mut c_void) -> bool {
    let slot = HELPER_RETADDR.load(Ordering::Acquire);
    // SAFETY: the BOOT thread's helper_retaddr (resolved at install); a plain word read.
    if !slot.is_null() && unsafe { slot.read_volatile() } != 0 {
        return true;
    }
    let pc = host_pc(uc) as isize - *lib().splitwx_diff;
    // SAFETY: in_code_gen_buffer only compares p against the buffer bounds.
    unsafe { (lib().in_code_gen_buffer)(pc as *const c_void) }
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

extern "C" fn atl_fault(sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    let Some(chain) = CHAIN.get() else {
        return;
    };
    let i = usize::from(sig != SIGNALS[0]);
    let guest = is_guest_fault(uc);
    // break `fault-route` (gate d): guest faults miss QEMU; break `chain-order` (gate e): host faults go to QEMU
    let to_qemu = if cfg!(atl_qe_fault_route) {
        false
    } else if cfg!(atl_qe_chain_order) {
        true
    } else {
        guest
    };
    if to_qemu {
        GUEST_FAULTS.fetch_add(1, Ordering::Relaxed);
        deliver(&chain.qemu[i], sig, info, uc);
    } else {
        HOST_FAULTS.fetch_add(1, Ordering::Relaxed);
        deliver(&chain.previous[i], sig, info, uc);
    }
}
