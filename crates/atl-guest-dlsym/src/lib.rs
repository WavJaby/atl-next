// SPDX-License-Identifier: MIT
//! `dlsym` for static guest bionic executables (the native bridge's `linker64`), which link no libdl: Soong links them `--no-undefined`, so a `dlsym` reference fails the link.
//! The reference comes from rustup's std: `panic_abort` looks up `android_set_abort_message` with `dlsym(RTLD_DEFAULT, ..)` (library/panic_abort/src/android.rs) and skips the abort message when it gets NULL.
//! Android's own std links that function statically instead (toolchain/android_rust patches/longterm/rustc-0014-Statically-link-android_set_abort_message.patch), so this answers exactly that lookup with the statically linked function and NULL for every other name, which keeps the abort message Android's std would set.
//! Defended condition: the converter links rustup's std (not Android's patched one) into a static bionic executable. When that stops holding, delete this crate and bp2ninja's `-bionic-static-rust` in the same change.
//! Not bionic's own lookup (`__loader_dlsym`, linker dlfcn.cpp): it walks the soinfo list, which a panic during linker init may not have yet, and the linker exports only `__loader_*` names (linker.generic.map), so it would answer NULL here.
//! Proof it is reached: `atl_guest_dlsym_hits` counts answered lookups; the linker's objcopy prefixes it `__dl_` like every linker symbol.
#![no_std]

use core::ffi::{CStr, c_char, c_void};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU32, Ordering};

/// The one name answered.
const ABORT_MESSAGE: &CStr = c"android_set_abort_message";

unsafe extern "C" {
    /// bionic libc/bionic/android_set_abort_message.cpp, linked statically into the executable.
    fn android_set_abort_message(msg: *const c_char);
}

/// Lookups of `android_set_abort_message` answered: read by the spike harness from the ELF symbol table.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static atl_guest_dlsym_hits: AtomicU32 = AtomicU32::new(0);

/// `dlsym(3)`: `handle` is ignored (std passes `RTLD_DEFAULT`).
///
/// # Safety
/// `symbol` is NULL or a NUL-terminated string, as `dlsym(3)` requires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(_handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    if symbol.is_null() {
        return null_mut();
    }
    // SAFETY: non-NULL and NUL-terminated by this function's contract (dlsym(3)); read only within this call.
    let name = unsafe { CStr::from_ptr(symbol) };
    if name != ABORT_MESSAGE {
        return null_mut();
    }
    atl_guest_dlsym_hits.fetch_add(1, Ordering::Relaxed);
    android_set_abort_message as *const () as *mut c_void
}
