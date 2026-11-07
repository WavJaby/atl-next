// SPDX-License-Identifier: MIT
//! Signals under one process-wide kernel action table shared by host and guest.
//! Host handlers (ART's sigchain `SignalChain::Handler` above all) assume the host thread pointer, but a signal hitting guest code arrives on the guest's: x86_64 `rt_sigreturn` restores no FS base, and nothing between kernel and handler switches it. So every installed kernel handler is re-wrapped at kernel level (`rewrap_all`): raw `rt_sigaction` reads each action, a handler that is not the arch's signal entry moves into the entry's table and the entry takes its place; the entry switches to host FS, calls it, restores the interrupted FS. Rewrap points: NB `initialize` (after ART's `FaultManager::Init`), each guest library load, and the host side of guest `sigaction`. Gap: a host library calling `sigaction` after the last point leaves its handler unwrapped (plan D6 (a)).
//! The entry decides from the interrupted FS alone (x86_64.rs `atl_nb_signal_entry`): host TCB -> as is; guest TCB -> its record's host FS. GS is ART's `Thread*`, never read or written.
//! Guest `sigaction` / `sigaction64` (bionic's own is a raw syscall): NBS stubs (patch 0003) -- bionic's `sigaction.o` defines only these two, so the guest libc's archive link never pulls it and bionic needs no patch (measured: shipped libc's `sigaction` = `jmp sigaction_stub`). The guest handler is registered with sigchain's `sigaction`, so ART keeps its claimed signals first; the host side runs it on the guest thread pointer through `guest_handler`. Old actions naming `guest_handler` are answered with the guest handler they stand for (as Berberis `guest_signal_action.cc` does).

use std::ffi::{c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::thread::layout;
use super::{arch, thread};

/// bionic LP64 `struct sigaction` / `sigaction64` (bits/signal_types.h `__SIGACTION_BODY`), offsets gated against the generated layout.
#[repr(C)]
pub struct BionicSigaction {
    flags: c_int,
    handler: usize,
    mask: u64,
    restorer: usize,
}

const _: () = {
    assert!(layout::sigaction::SIZE == size_of::<BionicSigaction>());
    assert!(layout::sigaction::sa_flags == std::mem::offset_of!(BionicSigaction, flags));
    assert!(layout::sigaction::sa_mask == std::mem::offset_of!(BionicSigaction, mask));
    assert!(layout::sigaction::sa_restorer == std::mem::offset_of!(BionicSigaction, restorer));
    assert!(layout::sigaction64::SIZE == size_of::<BionicSigaction>());
    // bionic sigset_t (LP64) = one kernel word (the gap to sa_restorer), bit sig-1; glibc's first word is the same bits
    assert!(layout::sigaction::sa_restorer - layout::sigaction::sa_mask == size_of::<u64>());
};

/// uapi asm/signal.h `SA_RESTORER` (x86): each libc sets its own restorer, so the flag never crosses between them.
const SA_RESTORER: c_int = 0x0400_0000;

/// Kernel `struct sigaction` for x86_64 `rt_sigaction` (uapi asm/signal.h: handler, flags, restorer, mask).
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelSigaction {
    handler: usize,
    flags: u64,
    restorer: usize,
    mask: u64,
}

/// Kernel signal count (`_NSIG`); signals are 1..=NSIG.
const NSIG: usize = 64;
const _: () = assert!(arch::SIGNALS == NSIG + 1);
/// The dispositions that are no function: SIG_DFL, SIG_IGN.
const NOT_A_FUNCTION: usize = 1;

/// Guest handler per signal: what `guest_handler` calls, 0 when the guest holds none.
static GUEST: [AtomicUsize; NSIG + 1] = [const { AtomicUsize::new(0) }; NSIG + 1];

fn raw_sigaction(
    sig: usize,
    new: Option<&KernelSigaction>,
    old: Option<&mut KernelSigaction>,
) -> bool {
    let new = new.map_or(std::ptr::null(), |n| n as *const KernelSigaction);
    let old = old.map_or(std::ptr::null_mut(), |o| o as *mut KernelSigaction);
    // SAFETY: kernel-layout records or null; sigsetsize = 8 (one kernel word, as `KernelSigaction::mask`).
    unsafe { libc::syscall(libc::SYS_rt_sigaction, sig, new, old, size_of::<u64>()) == 0 }
}

/// Wraps every kernel handler not yet wrapped (see module docs); returns how many it wrapped.
pub fn rewrap_all() -> usize {
    let entry = arch::signal_entry();
    let mut n = 0;
    for sig in 1..=NSIG {
        if sig == libc::SIGKILL as usize || sig == libc::SIGSTOP as usize {
            continue;
        }
        let mut k = KernelSigaction {
            handler: 0,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        if !raw_sigaction(sig, None, Some(&mut k))
            || k.handler <= NOT_A_FUNCTION
            || k.handler == entry
        {
            continue;
        }
        arch::set_signal_handler(sig, k.handler);
        let wrapped = KernelSigaction {
            handler: entry,
            ..k
        };
        if raw_sigaction(sig, Some(&wrapped), None) {
            n += 1;
        }
    }
    n
}

/// Host `sigaction` the guest's goes through: the one that owns the process's handler table -- ART's sigchain (`libsigchain.so`, loaded in every ART process), so a guest handler is chained behind ART's claims instead of replacing them; libc's in a process without sigchain (the engine gates g1-g3), where nothing claims signals ahead of the guest.
fn host_sigaction()
-> unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int {
    static P: OnceLock<usize> = OnceLock::new();
    let p = *P.get_or_init(|| {
        // SAFETY: dlopen of an already loaded library (RTLD_NOLOAD), dlsym of a NUL-terminated name.
        let chain = unsafe {
            let h = libc::dlopen(
                c"libsigchain.so".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_NOLOAD,
            );
            if h.is_null() {
                0
            } else {
                libc::dlsym(h, c"sigaction".as_ptr()) as usize
            }
        };
        if chain != 0 {
            chain
        } else {
            libc::sigaction as *const () as usize
        }
    });
    // SAFETY: sigchain's `sigaction` has libc's signature (sigchain.cc `extern "C" int sigaction`); libc's is libc's.
    unsafe {
        std::mem::transmute::<
            usize,
            unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int,
        >(p)
    }
}

/// The host handler standing for every guest one: runs the guest's on this thread's guest thread pointer. A guest handler may `siglongjmp` out: it lands in guest code on the guest thread pointer, which is where it must be.
extern "C" fn guest_handler(sig: c_int, info: *mut libc::siginfo_t, uc: *mut c_void) {
    let f = usize::try_from(sig)
        .ok()
        .and_then(|s| GUEST.get(s))
        .map_or(0, |g| g.load(Ordering::Acquire));
    if f == 0 {
        return;
    }
    // a fault reaching a guest handler is one ART's sigchain claims declined: the app's own crash handler takes it, which often re-raises with SIG_DFL and leaves no ART dump -> report first
    if [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE].contains(&sig) {
        // SAFETY: the kernel's siginfo / ucontext of this delivery.
        unsafe { arch::fault_report("fault handed to the guest's handler", sig, info, uc, 0) };
    }
    // SAFETY: the guest's handler (SA_SIGINFO signature; a plain one ignores the extra arguments under the SysV ABI); `siginfo_t` is the kernel's for both libcs and `uc` the kernel frame.
    unsafe {
        arch::call_guest(
            &mut *thread::attach_current(),
            f,
            sig as usize,
            info as usize,
            uc as usize,
            0,
        );
    }
}

/// Host side of guest stubs `sigaction` / `sigaction64` (one LP64 layout; a `Shim` row of the binding table).
/// Errors return -1 with the host errno only: the guest's errno is its own TLS, which this side does not reach (only an invalid signal number fails here).
pub(super) extern "C" fn sigaction(
    sig: c_int,
    new: *const BionicSigaction,
    old: *mut BionicSigaction,
) -> c_int {
    let Some(slot) = usize::try_from(sig).ok().filter(|&s| s > 0 && s <= NSIG) else {
        return -1;
    };
    let host_sigaction = host_sigaction();
    let prev_guest = GUEST[slot].load(Ordering::Acquire);
    // SAFETY: a zeroed glibc sigaction is an empty one; `new` / `old` are the guest's, null or valid (sigaction contract).
    unsafe {
        let mut host_new: libc::sigaction = std::mem::zeroed();
        let host_new_ptr = if let Some(n) = new.as_ref() {
            if n.handler > NOT_A_FUNCTION {
                GUEST[slot].store(n.handler, Ordering::Release);
                host_new.sa_sigaction = guest_handler as *const () as usize;
                host_new.sa_flags = (n.flags | libc::SA_SIGINFO) & !SA_RESTORER;
            } else {
                host_new.sa_sigaction = n.handler;
                host_new.sa_flags = n.flags & !SA_RESTORER;
            }
            libc::sigemptyset(&mut host_new.sa_mask);
            std::ptr::write((&raw mut host_new.sa_mask).cast::<u64>(), n.mask);
            &raw const host_new
        } else {
            std::ptr::null()
        };
        let mut host_old: libc::sigaction = std::mem::zeroed();
        let rc = host_sigaction(sig, host_new_ptr, &mut host_old);
        if rc != 0 {
            GUEST[slot].store(prev_guest, Ordering::Release);
            return rc;
        }
        if let Some(n) = new.as_ref()
            && n.handler <= NOT_A_FUNCTION
        {
            GUEST[slot].store(0, Ordering::Release);
        }
        if let Some(o) = old.as_mut() {
            let h = host_old.sa_sigaction;
            o.handler = if h == guest_handler as *const () as usize {
                prev_guest
            } else {
                h
            };
            o.flags = host_old.sa_flags & !SA_RESTORER;
            o.mask = std::ptr::read((&raw const host_old.sa_mask).cast::<u64>());
            o.restorer = 0;
        }
    }
    rewrap_all();
    0
}
