// SPDX-License-Identifier: MIT
//! The trap body (QEMU bridge plan 2.6(3), Q5-8): the trap type, the callback the QEMU engine installs, and a binding trap's dispatch to its host target. Moved here from `engine/qemu/mod.rs`, so the identity side can share it. The crossing counter (`counters.rs`, feature `crossing-count`) counts each binding crossing here, where the slot is chosen.

use std::sync::atomic::{AtomicUsize, Ordering};

use atl_engine_api::AtlLanesV1;

use super::jni;
use super::unsupported;
use crate::engine::bindings::JNI_STUBS;
use crate::engine::bindings::qemu::THUNKS;

/// A trap from the engine, decoded from `atl_trap_kind`. `Boot` x0 = the auxv QEMU built for the boot executable (x1, its return stub, the engine keeps); `Call` and `Bind` carry their lanes (`atl_lanes_v1`: x0..x7, v0..v7, eight stack words); `Report` x0.
#[derive(Clone, Copy, Debug)]
pub enum Trap {
    /// BOOT: the boot executable's auxv (x0).
    Boot(u64),
    /// `svc #CALL`: the guest's lanes.
    Call(AtlLanesV1),
    /// A pool thunk: its id (a JNI slot below `JNI_STUBS`, else a bound host function), and its lanes.
    Bind { id: u32, lanes: AtlLanesV1 },
    /// A value reported for a gate: x0.
    Report(u64),
}

/// What a trap answers: x0 (every trap; BOOT's exit status) and v0 (CALL and BIND: the FP result lane, 0 when the callee returns an integer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    /// The guest's x0.
    pub x0: u64,
    /// The guest's v0, low 64 bits.
    pub v0: u64,
}

/// A trap callback. Its answer is [`Answer`] (REPORT ignores it).
pub type OnTrap = fn(Trap) -> Answer;

/// The host target of each pool slot, read by the binding trap ([`call_binding`]). The QEMU placement writes it (`qemu::place::bind`).
pub(super) static TARGETS: [AtomicUsize; THUNKS] = [const { AtomicUsize::new(0) }; THUNKS];

/// The default trap callback: a JNI trap calls the host object's table (`call_jni`), a binding trap its host target (`call_binding`), and each answers with its result in x0 (v0 is 0 until Q6-3). Other kinds abort until a route is built; BOOT is the start, answered by `qemu::run` or by the gate's own callback.
pub fn on_trap(trap: Trap) -> Answer {
    match trap {
        Trap::Bind { id, lanes } if (id as usize) < JNI_STUBS => Answer {
            x0: call_jni(id, lanes.x),
            v0: 0,
        },
        Trap::Bind { id, lanes } => Answer {
            x0: call_binding(id, lanes.x),
            v0: 0,
        },
        other => unsupported(&format!("qemu crossing: {other:?} is not routed yet")),
    }
}

/// Added to a trap's id before its slot is read: 0 in every build; the `qe_id` break (`ATL_NB_BREAK`) makes it 1, so each binding calls its neighbour's host function.
#[cfg(not(atl_nb_qe_id))]
const ID_SKEW: usize = 0;
#[cfg(atl_nb_qe_id)]
const ID_SKEW: usize = 1;

/// Calls the host function of the slot `id + ID_SKEW` with the guest's eight integer argument words. Integer arguments only: floats (v0..v7) are Q6. A callee with fewer arguments ignores the rest, under AAPCS64 and SysV alike. The counter sees the slot that ran, so a skewed dispatch shows up as a wrong count.
pub(super) fn call_binding(id: u32, args: [u64; 8]) -> u64 {
    let Some(slot) = usize::try_from(id)
        .ok()
        .map(|i| i + ID_SKEW)
        .filter(|&s| s < THUNKS)
    else {
        unsupported("qemu crossing: binding id past the pool")
    };
    let target = TARGETS[slot].load(Ordering::Acquire);
    if target == 0 {
        unsupported("qemu crossing: binding trap for an unbound slot")
    }
    #[cfg(feature = "crossing-count")]
    crate::counters::count_binding(slot, target);
    type Host = extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: `target` is the host function `bind` stored for this slot, a pointer to a C-ABI function that takes integer arguments only; the call passes it the guest's argument words.
    let host: Host = unsafe { std::mem::transmute::<usize, Host>(target) };
    let [a0, a1, a2, a3, a4, a5, a6, a7] = args;
    host(a0, a1, a2, a3, a4, a5, a6, a7)
}

/// A host JNI function as [`call_jni`] calls it: C-variadic, since JNI's `Call*Method` entries are variadic and SysV sets %al only for variadic callees. Integer arguments and result.
type HostJni = unsafe extern "C" fn(u64, ...) -> u64;

/// A guest JNI call (pool id `id` < `JNI_STUBS`, Q6-2): x0 is the guest's JNIEnv or JavaVM (a `GuestObj`), and the call runs on the host object it stands for. That object's own table slot `id` runs with x0 replaced by the host object and x1..x7 forwarded in order; its x0 is the answer. Integer lanes only: fp and stack lanes are Q6-1 and Q6-3.
/// Deliberate breaks: `jni_swap` exchanges x3 and x4 of CallStaticIntMethod only; `jni_guest_env` passes the guest's env through unmapped.
fn call_jni(id: u32, args: [u64; 8]) -> u64 {
    let [guest, a1, a2, a3, a4, a5, a6, a7] = args;
    // SAFETY: x0 of a guest JNI call is the env or vm the guest got from `guest_env` / `guest_vm`.
    let host = unsafe { jni::host_of(guest as usize) };
    // SAFETY: `id` is a slot of the guest's table, which jni.rs builds from the host object's table, so it is a slot of `host`'s own table.
    let target = unsafe { jni::host_slot(host, id as usize) };
    #[cfg(atl_nb_jni_swap)]
    let (a3, a4) = if id as usize == jni::CALL_STATIC_INT_METHOD {
        (a4, a3)
    } else {
        (a3, a4)
    };
    #[cfg(atl_nb_jni_guest_env)]
    let arg0 = guest;
    #[cfg(not(atl_nb_jni_guest_env))]
    let arg0 = host as u64;
    // SAFETY: `target` is a host JNI function (C ABI), so a pointer of the C-variadic type `HostJni`.
    let f = unsafe { std::mem::transmute::<usize, HostJni>(target) };
    // SAFETY: `f` takes the guest's words as its arguments; a callee with fewer parameters ignores the rest.
    unsafe { f(arg0, a1, a2, a3, a4, a5, a6, a7) }
}
