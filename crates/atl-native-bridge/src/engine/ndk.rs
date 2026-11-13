// SPDX-License-Identifier: MIT
//! The lane-mapped class: host sides of the NDK functions with a lane the host cannot take as the guest passes it (build.rs `ENV_SHIMS` and `CALLBACKS`, `Entry::Mapped`). Each row maps those lanes, then calls the same function of the host library of its soname (bindings.rs `system_symbol`).
//! - `JNIEnv*`: the guest passes its own env (jni.rs `GuestObj`); mapped to the host env (jni.rs `host_env`).
//! - a guest function pointer (GS2.2 callback-param, tr-first blocker 5): mapped to a down shell ([`super::Active::down`], plain kind, the callback's shorty), so the host library calls a host address that enters the guest. The host may call it during the call or later, from any thread: the shell attaches a thread on its first down call (qemu `call_down`, identity `attach_entry`). Shells are deduplicated by (function, shorty) and never freed, so a callback re-posted every frame takes one. The callback's own arguments are values or the guest's user data (build.rs `CALLBACKS` review), passed unchanged.
//!
//! The other arguments and the results are host values already: object references are the host's, and an AAssetManager, ANativeWindow or pixel pointer names host memory the guest reads in the same address space. Every row's lanes are ints, at most six (atl_bridgegen `jni::route` refuses the rest at build time), so one six-word forwarder serves all.

use super::bindings::{self, LaneMap, MappedRow};
use super::{jni, unsupported};

/// The forwarded host function: six int lanes (SysV's integer registers; a shorter prototype ignores the rest), one int result (a void one's is never read).
type HostFn = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize) -> usize;

/// `Entry::Mapped` row `(soname, symbol, maps)`'s call: each mapped lane replaced, then `symbol` of the host library `soname` with the six lanes. Aborts when the host library lacks the symbol (bindings.rs `target` binds the row only when it has it) or no down shell is left.
pub(super) fn call_mapped((soname, symbol, maps): MappedRow, mut lanes: [usize; 6]) -> usize {
    let f = bindings::system_symbol(soname, symbol).unwrap_or_else(|| {
        unsupported(&format!(
            "{symbol}: lane-mapped row with no {soname} in SYSTEM_LIB"
        ))
    });
    for &(i, map) in maps {
        match map {
            // SAFETY: lane `i` is the guest's own `JNIEnv*` (made by jni.rs `guest_env`), as the NDK function receives it (build.rs proved the position from its prototype).
            LaneMap::Env => lanes[i] = unsafe { jni::host_env(lanes[i]) },
            // break `cb_down_skip` (app gate, rows looperNow / looperLater): the guest's own address reaches the host, which calls guest code natively (QEMU: AArch64 bytes run as x86-64)
            #[cfg(not(atl_nb_cb_down_skip))]
            LaneMap::Down(shorty) if lanes[i] != 0 => {
                lanes[i] = super::active()
                    .down(lanes[i], super::identity::Down::Plain, Some(shorty))
                    .unwrap_or_else(|e| unsupported(&format!("{symbol}: callback lane {i}: {e}")));
            }
            LaneMap::Down(_) => {}
        }
    }
    // SAFETY: the host library's export of this name, whose prototype (the NDK header's) takes at most six int lanes and answers an int lane or void (route); extra lanes sit in registers it never reads.
    let f: HostFn = unsafe { std::mem::transmute(f) };
    // SAFETY: as above; every other lane is a host value in this address space, every mapped one the host's own.
    unsafe { f(lanes[0], lanes[1], lanes[2], lanes[3], lanes[4], lanes[5]) }
}
