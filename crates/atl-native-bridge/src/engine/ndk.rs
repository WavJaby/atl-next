// SPDX-License-Identifier: MIT
//! The env class: host sides of the NDK functions with JNI types in their prototype (build.rs `ENV_SHIMS`, `Entry::Env`). The guest passes its own env (jni.rs `GuestObj`), so a generic row would hand the host library a guest env: each row maps it to the host env (jni.rs `host_env`) and calls the same function of the host library of its soname (bindings.rs `system_symbol`).
//! The other arguments and the results are host values already: object references are the host's, and an AAssetManager, ANativeWindow or pixel pointer names host memory the guest reads in the same address space. Every row's lanes are ints, at most six (atl_bridgegen `jni::env_binding` refuses the rest at build time), so one six-word forwarder serves all.

use super::{bindings, jni, unsupported};

/// The forwarded host function: six int lanes (SysV's integer registers; a shorter prototype ignores the rest), one int result (a void one's is never read).
type HostFn = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize) -> usize;

/// `Entry::Env` row `(soname, symbol, env)`'s call: `lanes[env]` (the guest's `JNIEnv*`) replaced by the host env, then `symbol` of the host library `soname` with the six lanes. Aborts when the host library lacks the symbol (bindings.rs `target` binds the row only when it has it).
pub(super) fn call_env((soname, symbol, env): (&str, &str, usize), mut lanes: [usize; 6]) -> usize {
    let f = bindings::system_symbol(soname, symbol).unwrap_or_else(|| {
        unsupported(&format!(
            "{symbol}: env-class row with no {soname} in SYSTEM_LIB"
        ))
    });
    // SAFETY: lane `env` is the guest's own `JNIEnv*` (made by jni.rs `guest_env`), as the NDK function receives it (build.rs proved the position from its prototype).
    lanes[env] = unsafe { jni::host_env(lanes[env]) };
    // SAFETY: the host library's export of this name, whose prototype (the NDK header's) takes at most six int lanes and answers an int lane or void (env_binding); extra lanes sit in registers it never reads.
    let f: HostFn = unsafe { std::mem::transmute(f) };
    // SAFETY: as above; every other lane is a host value in this address space.
    unsafe { f(lanes[0], lanes[1], lanes[2], lanes[3], lanes[4], lanes[5]) }
}
