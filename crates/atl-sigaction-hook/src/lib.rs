// SPDX-License-Identifier: MIT
//! The process-wide seam where a host signal handler is installed: an executable that invokes `interpose!()` defines `sigaction`, `signal`, `bsd_signal`, `sigset`, `sysv_signal` (exported, `link_arg`), so every library's call binds to it ahead of libsigchain and libc. Each forwards to the next definition (`dlsym(RTLD_NEXT)`: libsigchain where the executable links it, else libc), then calls the hook a later-loaded library registers through the exported setter `SETTER` -- the native bridge wraps the new kernel action there, so a handler a host library installs at any time runs on the host thread pointer (atl-native-bridge engine/signal.rs).
//! A macro, not `#[no_mangle]` items here: an rlib's definitions would also be pulled into every cdylib linking this crate, binding that library's own calls to a copy whose RTLD_NEXT skips libsigchain.
//! `bsd_signal` goes through `signal` (as sigchain's own does on LP64-less targets): glibc's is an alias of its `signal`, which would skip sigchain. `sigset` / `sysv_signal` forward to libc's (sigchain has none).
//! Boundary crate (C ABI exports, `dlsym`).

use std::ffi::{CStr, c_int};
use std::sync::atomic::{AtomicUsize, Ordering};

#[doc(hidden)]
pub use libc;

/// What the hook receives: the signal whose action was just set (or re-read: it is called after every successful call, so it must be idempotent and async-signal-safe -- `sigaction` may be called from a handler).
pub type Hook = extern "C" fn(c_int);
/// The exported setter's type.
pub type Setter = unsafe extern "C" fn(Hook);
/// The setter's symbol name, looked up with `dlsym(RTLD_DEFAULT)`; absent in a process whose executable does not invoke `interpose!()`.
pub const SETTER: &CStr = c"atl_sigaction_hook_set";
/// The interposed functions; with `SETTER` the executable's exported symbols (xtask-harness's hwui bindings gate holds the launcher's reverse row to these).
pub const INTERPOSED: [&str; 5] = ["sigaction", "signal", "bsd_signal", "sigset", "sysv_signal"];

/// The linker argument exporting the interposer from an executable (executables export no symbols by default).
pub fn link_arg() -> String {
    let setter = SETTER.to_str().expect("SETTER is ASCII");
    let syms: Vec<String> = INTERPOSED
        .iter()
        .chain(std::iter::once(&setter))
        .map(|s| format!("--export-dynamic-symbol={s}"))
        .collect();
    format!("-Wl,{}", syms.join(","))
}

static HOOK: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub fn set(h: Hook) {
    HOOK.store(h as usize, Ordering::Release);
}

#[doc(hidden)]
pub fn notify(sig: c_int) {
    let h = HOOK.load(Ordering::Acquire);
    if h != 0 {
        // SAFETY: only `set` stores here, from a `Hook`.
        let h = unsafe { std::mem::transmute::<usize, Hook>(h) };
        h(sig);
    }
}

/// The next definition of `name` after this executable, cached in `cache`; 0 when none.
#[doc(hidden)]
pub fn next(cache: &AtomicUsize, name: &CStr) -> usize {
    let p = cache.load(Ordering::Acquire);
    if p != 0 {
        return p;
    }
    // SAFETY: a NUL-terminated name; RTLD_NEXT from this code = the objects after the executable it is linked into.
    let p = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) } as usize;
    cache.store(p, Ordering::Release);
    p
}

type SigactionFn =
    unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int;
type SignalFn = unsafe extern "C" fn(c_int, libc::sighandler_t) -> libc::sighandler_t;

/// # Safety
/// libc's `sigaction` contract.
#[doc(hidden)]
pub unsafe fn sigaction(
    sig: c_int,
    new: *const libc::sigaction,
    old: *mut libc::sigaction,
) -> c_int {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let f = next(&NEXT, c"sigaction");
    if f == 0 {
        // SAFETY: this thread's errno.
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return -1;
    }
    // SAFETY: the next `sigaction` has libc's signature; the arguments are the caller's.
    let rc = unsafe { std::mem::transmute::<usize, SigactionFn>(f)(sig, new, old) };
    if rc == 0 && !new.is_null() {
        notify(sig);
    }
    rc
}

/// `signal`-shaped forwarder to the next `name`, then the hook.
#[doc(hidden)]
pub fn signal_like(
    cache: &AtomicUsize,
    name: &CStr,
    sig: c_int,
    h: libc::sighandler_t,
) -> libc::sighandler_t {
    let f = next(cache, name);
    if f == 0 {
        // SAFETY: this thread's errno.
        unsafe { *libc::__errno_location() = libc::ENOSYS };
        return libc::SIG_ERR;
    }
    // SAFETY: the next `name` has `signal`'s signature (glibc signal.h: signal, sigset, sysv_signal).
    let old = unsafe { std::mem::transmute::<usize, SignalFn>(f)(sig, h) };
    if old != libc::SIG_ERR {
        notify(sig);
    }
    old
}

#[doc(hidden)]
pub fn signal(sig: c_int, h: libc::sighandler_t) -> libc::sighandler_t {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    signal_like(&NEXT, c"signal", sig, h)
}

#[doc(hidden)]
pub fn sigset(sig: c_int, h: libc::sighandler_t) -> libc::sighandler_t {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    signal_like(&NEXT, c"sigset", sig, h)
}

#[doc(hidden)]
pub fn sysv_signal(sig: c_int, h: libc::sighandler_t) -> libc::sighandler_t {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    signal_like(&NEXT, c"sysv_signal", sig, h)
}

/// Defines the interposer in the invoking crate; invoke once, in an executable only (module docs).
#[macro_export]
macro_rules! interpose {
    () => {
        #[unsafe(no_mangle)]
        unsafe extern "C" fn sigaction(
            sig: ::std::ffi::c_int,
            new: *const $crate::libc::sigaction,
            old: *mut $crate::libc::sigaction,
        ) -> ::std::ffi::c_int {
            // SAFETY: the caller's arguments under libc's contract.
            unsafe { $crate::sigaction(sig, new, old) }
        }
        #[unsafe(no_mangle)]
        extern "C" fn signal(
            sig: ::std::ffi::c_int,
            h: $crate::libc::sighandler_t,
        ) -> $crate::libc::sighandler_t {
            $crate::signal(sig, h)
        }
        #[unsafe(no_mangle)]
        extern "C" fn bsd_signal(
            sig: ::std::ffi::c_int,
            h: $crate::libc::sighandler_t,
        ) -> $crate::libc::sighandler_t {
            $crate::signal(sig, h)
        }
        #[unsafe(no_mangle)]
        extern "C" fn sigset(
            sig: ::std::ffi::c_int,
            h: $crate::libc::sighandler_t,
        ) -> $crate::libc::sighandler_t {
            $crate::sigset(sig, h)
        }
        #[unsafe(no_mangle)]
        extern "C" fn sysv_signal(
            sig: ::std::ffi::c_int,
            h: $crate::libc::sighandler_t,
        ) -> $crate::libc::sighandler_t {
            $crate::sysv_signal(sig, h)
        }
        #[unsafe(no_mangle)]
        extern "C" fn atl_sigaction_hook_set(h: $crate::Hook) {
            $crate::set(h)
        }
        const _: $crate::Setter = atl_sigaction_hook_set;
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_arg_exports_setter() {
        // the macro's setter fn name must equal SETTER (dlsym finds it by that name)
        assert!(super::link_arg().ends_with("--export-dynamic-symbol=atl_sigaction_hook_set"));
        assert_eq!(super::SETTER, c"atl_sigaction_hook_set");
    }
}
