// SPDX-License-Identifier: MIT
//! The trap body (QEMU bridge plan 2.6(3), Q5-8, Q6-3): the trap type, the callback the QEMU engine installs, the host call a binding or JNI trap makes, and the JNI route. Moved here from `engine/qemu/mod.rs`, so the identity side can share it. The crossing counter (`counters.rs`, feature `crossing-count`) counts each binding crossing here, where the slot is chosen.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use atl_engine_api::AtlLanes;

use super::jni;
use super::objects::{self, Class, Refused, Sig};
use super::qemu::DownSlot;
use super::{unsupported, va_list};
use crate::engine::Down;
use crate::engine::bindings::OBJECT_STUBS;
use crate::engine::bindings::QEMU_THUNKS as THUNKS;

/// A trap from the engine, decoded from `atl_trap_kind`. `Boot` x0 = the auxv QEMU built for the boot executable (x1, its return stub, the engine keeps); `Call` and `Bind` carry their lanes (`atl_lanes`, filled by the guest's convention, qemu/arch.rs `Arch::rules`: AAPCS64 x0..x7 in `regs[0..8]`, v0..v7 in `fp[0..8]`, sixteen stack words; AAPCS32 r0..r3 in `regs[0..4]`, sixteen stack words); `Report` x0.
#[derive(Clone, Copy, Debug)]
pub enum Trap {
    /// BOOT: the boot executable's auxv (x0).
    Boot(u64),
    /// `svc #CALL`: the guest's lanes.
    Call(AtlLanes),
    /// A pool thunk: its id (a JNI slot below `OBJECT_STUBS`, else a bound host function), and its lanes.
    Bind { id: u32, lanes: AtlLanes },
    /// A value reported for a gate: x0.
    Report(u64),
}

/// What a trap answers, the guest's result registers: x0 (every trap; BOOT's exit status), x1 (a 32-bit guest's r1: the high word of a double-word result, [`guest_ret32`]; 0 on aarch64) and v0 (CALL and BIND: the FP result lane, 0 when the callee returns an integer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    /// The guest's x0 (r0).
    pub x0: u64,
    /// The guest's x1 (r1).
    pub x1: u64,
    /// The guest's v0, low 64 bits.
    pub v0: u64,
}

/// A trap callback. Its answer is [`Answer`] (REPORT ignores it).
pub type OnTrap = fn(Trap) -> Answer;

/// The host function and lanes of each pool slot, read by the binding trap ([`call_binding`]). The QEMU placement sets a slot once (`place::bind_result`), so a slot is bound before any trap reads it.
pub(super) static TARGETS: [OnceLock<(usize, Sig)>; THUNKS] = [const { OnceLock::new() }; THUNKS];

/// The default trap callback: a JNI trap calls the host object's table (`call_object`), a binding trap its host target (`call_binding`), each with the lanes read by the process guest's convention (qemu/arch.rs `Arch::rules`) and answered in its result registers. Other kinds abort until a route is built; BOOT is the start, answered by `qemu::run` or by the gate's own callback.
pub fn on_trap(trap: Trap) -> Answer {
    match trap {
        Trap::Bind { id, lanes } if (id as usize) < OBJECT_STUBS => {
            call_object(super::qemu::arch().rules(), id, &lanes)
        }
        Trap::Bind { id, lanes } => call_binding(super::qemu::arch().rules(), id, &lanes),
        other => unsupported(&format!("qemu crossing: {other:?} is not routed yet")),
    }
}

/// Added to a trap's id before its slot is read: 0 in every build; the `qe_id` break (feature `break-qe_id`) makes it 1, so each binding calls its neighbour's host function.
#[cfg(not(atl_nb_qe_id))]
const ID_SKEW: usize = 0;
#[cfg(atl_nb_qe_id)]
const ID_SKEW: usize = 1;

/// Calls the host function of the slot `id + ID_SKEW` with the guest's lanes (convention `guest`; [`call_host`], placed and answered by the slot's lanes). Every argument lane is forwarded, fp included. The counter sees the slot that ran, so a skewed dispatch shows up as a wrong count.
pub(super) fn call_binding(guest: HostRules, id: u32, lanes: &AtlLanes) -> Answer {
    let Some(slot) = usize::try_from(id)
        .ok()
        .map(|i| i + ID_SKEW)
        .filter(|&s| s < THUNKS)
    else {
        unsupported("qemu crossing: binding id past the pool")
    };
    let Some(&(target, sig)) = TARGETS[slot].get() else {
        unsupported("qemu crossing: binding trap for an unbound slot")
    };
    #[cfg(feature = "crossing-count")]
    crate::counters::count_binding(slot, target);
    call_host(guest, target, None, lanes, sig, &[])
}

/// The object crossing counter's one entry (QEMU bridge plan section 5, per proxy table + slot): a guest->host call of slot `slot` of table `table` to host `target`. QEMU's [`call_object`] calls it with the table `lane_of` found; the identity object shell reaches it through [`count_object_shell`]. Break `qe_jni_count`: [`call_object`] skips it, so only identity counts.
#[cfg(feature = "crossing-count")]
fn count_object(table: objects::TableId, slot: usize, target: usize) {
    crate::counters::count_object(table, slot, target);
}

/// The identity object shell's counter (`atl-bridgegen` `x86::entries_asm`, counted): slot `slot` ran `target` for guest object `guest` (the shell's %rdi before its body rewrote it), whose table is read from its word 0. Runs on guest FS: `table_of` reads atomics only, no TLS, no libc.
///
/// # Safety
/// `guest` is the object the guest called through: readable for one word (the shell has already read its word 1).
#[cfg(feature = "crossing-count")]
#[unsafe(export_name = "atl_nb_count_object")]
pub unsafe extern "C" fn count_object_shell(slot: usize, target: usize, guest: usize) {
    // SAFETY: caller contract.
    let table = unsafe { objects::table_of(guest) }.unwrap_or(crate::counters::UNREGISTERED);
    count_object(table, slot, target);
}

/// A guest JNI call (pool id `id` < `OBJECT_STUBS`, Q6-2): x0 is the guest's JNIEnv or JavaVM, and the call runs on the host object it stands for. That object's own table slot `id` runs with x0 replaced by the host object and the other lanes forwarded; its result is the answer. The descriptor (`jni::lane_of`, from the guest object's table) gives the result class; a refused call aborts by name ([`refuse`]).
/// Every `...` and `...V` call crosses as its `...A` sibling, its arguments read by the method's shorty into a `jvalue[]` (va_list.rs), so the host callee has the `A` form's prototype on every host.
/// Deliberate breaks: `jni_swap` exchanges x3 and x4 of CallStaticIntMethod only, before the arguments are read; `jni_guest_env` passes the guest's env through unmapped.
fn call_object(rules: HostRules, id: u32, lanes: &AtlLanes) -> Answer {
    let guest = lanes.regs[0] as usize;
    // SAFETY: x0 is the object pointer the guest called through.
    let (table, lane) =
        unsafe { objects::lane_of(guest, id as usize) }.unwrap_or_else(|r| refuse(r));
    // SAFETY: `guest` is a guest env or vm: `lane_of` found its table, which only `guest_env` / `guest_vm` objects carry.
    let host = unsafe { objects::host_of(guest) };
    let mut lanes = *lanes;
    // break `jni_swap`: before the arguments are read, so the call reads the exchanged lanes
    #[cfg(atl_nb_jni_swap)]
    if id as usize == jni::CALL_STATIC_INT_METHOD {
        lanes.regs.swap(3, 4);
    }
    let name = lane.name;
    let bad = |b: va_list::Bad| -> Vec<u64> {
        refuse(match b {
            va_list::Bad::Shorty(c) => Refused::Shorty { name, c },
            va_list::Bad::Past => Refused::VariadicWindow { name },
        })
    };
    // the arguments of a `...V` call (Q6-7) or a `...` call: read per the method's shorty into a jvalue array, which the `...A` sibling takes in the lane after the fixed parameters
    let mut args: Vec<u64> = Vec::new();
    let mut va_lane = None;
    let mut sig_lane = lane;
    let va_k = lane.params.iter().position(|c| *c == Class::VaList);
    // the `...A` sibling's descriptor: the prototype the host call is placed by
    let a_lane = |a: usize| {
        // SAFETY: `a` is a slot of the same guest object's table as the `lane_of` above.
        unsafe { objects::lane_of(guest, a) }
            .unwrap_or_else(|r| refuse(r))
            .1
    };
    let slot = if let Some(k) = va_k {
        // lanes k - 1 and k located by the guest's rules (`int_arg`: every earlier JNI parameter is one word)
        let method = *rules.int_arg(&mut lanes, k - 1) as usize;
        // SAFETY: `host` is the host env behind the guest env; lane k - 1 is the call's jmethodID (jni.h: every V slot's va_list follows its methodID).
        let params = unsafe { jni::method_params(host, method) }
            .unwrap_or_else(|| refuse(Refused::NoShorty { name }));
        let va = *rules.int_arg(&mut lanes, k) as usize;
        // SAFETY: lane k is the guest's va_list -- a pointer to it (AAPCS64), its `__ap` (AAPCS32: the one-word struct passed by value) -- and the guest made the call with the method's arguments (JNI contract).
        args = unsafe {
            if rules.word_bits == AAPCS64.word_bits {
                va_list::jvalues(va, params).unwrap_or_else(bad)
            } else {
                va_list::jvalues32(va, usize::MAX, params).unwrap_or_else(bad)
            }
        };
        va_lane = Some(k);
        let a = jni::a_slot(id as usize);
        sig_lane = a_lane(a);
        a
    } else if lane.variadic {
        // a `...` call, both widths: its k fixed parameters come first, the methodID last (lane k - 1)
        let k = lane.params.len();
        let method = *rules.int_arg(&mut lanes, k - 1) as usize;
        // SAFETY: as the V case: lane k - 1 is the call's jmethodID (jni.h: every `...` slot's arguments follow its methodID).
        let params = unsafe { jni::method_params(host, method) }
            .unwrap_or_else(|| refuse(Refused::NoShorty { name }));
        args = if rules.word_bits == AAPCS64.word_bits {
            // AAPCS64: the arguments are the lanes after the named registers, read as the list `va_start` would save (va_list.rs `jvalues_lanes`)
            va_list::jvalues_lanes(&lanes, k, params).unwrap_or_else(bad)
        } else {
            // AAPCS32 passes `...` in r0-r3 then the stack, one run of 4-byte words with the fixed parameters (base PCS: no fp registers for a variadic call); packed into an 8-aligned image those lanes are the list `va_start` would read, starting at the word after the k fixed parameters (all one word: env, object or class, methodID last)
            let words = rules.int_lanes + rules.stack_words;
            let image = va_list::image32(
                lanes.regs[..rules.int_lanes]
                    .iter()
                    .chain(&lanes.stack[..rules.stack_words])
                    .copied(),
            );
            let base = image.as_ptr() as usize;
            // SAFETY: `image` holds `words` 4-byte words from `base`; the reader stops before `base + 4 * words`.
            unsafe { va_list::jvalues32(base + 4 * k, base + 4 * words, params) }
                .unwrap_or_else(bad)
        };
        va_lane = Some(k);
        let a = jni::variadic_a_slot(id as usize);
        sig_lane = a_lane(a);
        a
    } else if table == objects::ENV
        && rules.word_bits != AAPCS64.word_bits
        && jni::is_a_slot(id as usize)
    {
        // a 32-bit guest's `...A` call: its jvalue array as the host reads it (va_list.rs `jvalues_a32`: a reference entry's high word cleared), in the lane after the methodID
        let k = lane.params.len() - 1;
        let method = *rules.int_arg(&mut lanes, k - 1) as usize;
        // SAFETY: lane k - 1 is the call's jmethodID (jni.h: every A slot's jvalue array follows its methodID).
        let params = unsafe { jni::method_params(host, method) }
            .unwrap_or_else(|| refuse(Refused::NoShorty { name }));
        let at = *rules.int_arg(&mut lanes, k) as usize;
        if at != 0 {
            // SAFETY: lane k is the guest's jvalue array, one entry per shorty parameter (JNI contract).
            args = unsafe { va_list::jvalues_a32(at, params) };
            va_lane = Some(k);
        }
        id as usize
    } else {
        id as usize
    };
    // SAFETY: `slot` is `id` or its A sibling, below the length of the table `lane_of` read, which jni.rs builds from the host object's table, so it is a slot of `host`'s own table.
    let target = unsafe { objects::host_slot(host, slot) };
    // counted as the guest's slot `id`, with the host function that ran
    #[cfg(all(feature = "crossing-count", not(atl_nb_qe_jni_count)))]
    count_object(table, id as usize, target);
    #[cfg(not(all(feature = "crossing-count", not(atl_nb_qe_jni_count))))]
    let _ = table;
    #[cfg(atl_nb_jni_guest_env)]
    let arg0 = guest as u64;
    #[cfg(not(atl_nb_jni_guest_env))]
    let arg0 = host as u64;
    // host words replace guest lanes after the lanes are brought into the host's image (`call_host`), so a 32-bit guest's word never truncates them: the host object in lane 0, a `...V` or `...` call's jvalue array in its va_list lane (an int lane: `place`)
    let mut host_words = vec![(0, arg0)];
    host_words.extend(va_lane.map(|k| (k, args.as_ptr() as u64)));
    // the prototype of the slot that runs: the `A` sibling's for a `...` or `...V` call, which never has a variadic one
    let sig = Sig {
        ret: sig_lane.ret,
        params: Some(sig_lane.params),
    };
    call_host(rules, target, Some(name), &lanes, sig, &host_words)
}

/// The up call per host ISA (`cfg(target_arch)` only here): `call(target, lanes, fp, sig) -> (x result, fp result bits)`, same interface on each. riscv64 places the lanes by the prototype into LP64D and calls from asm; every other host takes them as one variadic call (x86_64 SysV the reference).
#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
use riscv64 as host_call;
#[cfg(not(target_arch = "riscv64"))]
mod variadic;
#[cfg(not(target_arch = "riscv64"))]
use variadic as host_call;

/// Calls `target` with the guest lanes `lanes` and answers its result by `sig.ret`: a double from the fp result register (its bits in v0), a float as its low 32 bits with the upper ones clear (AArch64 reads s0; LP64D's fa0 holds it NaN-boxed), a void as x0 = 0, anything else from x0; then shaped into the guest's result registers ([`guest_answer`]).
/// The lanes enter in the guest's convention `guest` and are first brought into AAPCS64's image ([`up_lanes`]), the one every host call reads.
/// Then `host_words` (integer argument index, host value) replace those arguments in the image: host values the guest's word cannot carry (an object call's host object and jvalue array); every argument before each is one word. `jni`: the JNI function an object call runs, which names a refused result (ART's functions have no dynamic symbol).
/// Breaks: `v_drop` zeroes the fp lanes; `drop_stack` moves the stack words down one; `ret_int` reads an fp result from x0; per host, `al_zero` (variadic.rs) and `up_no_nanbox` (riscv64.rs).
fn call_host(
    guest: HostRules,
    target: usize,
    jni: Option<&str>,
    lanes: &AtlLanes,
    sig: Sig,
    host_words: &[(usize, u64)],
) -> Answer {
    let mut lanes = up_lanes(guest, lanes, sig.params).unwrap_or_else(|| refuse_up(target));
    for &(k, v) in host_words {
        *AAPCS64.int_arg(&mut lanes, k) = v;
    }
    #[cfg(atl_nb_v_drop)]
    {
        lanes.fp = [0; atl_engine_api::LANE_FP];
    }
    #[cfg(atl_nb_drop_stack)]
    {
        lanes.stack.copy_within(1.., 0);
        lanes.stack[atl_engine_api::LANE_STACK - 1] = 0;
    }
    let ret = fp_result(sig.ret);
    let (x, v) = host_call::call(target, &lanes, ret.is_fp(), sig);
    let answer = match ret {
        Class::F32 => Answer {
            v0: v & 0xFFFF_FFFF,
            ..Answer::default()
        },
        Class::F64 => Answer {
            v0: v,
            ..Answer::default()
        },
        Class::Void => Answer::default(),
        _ => Answer {
            x0: x,
            ..Answer::default()
        },
    };
    guest_answer(
        guest,
        sig.ret,
        ilp32_narrow(guest, sig.ret, target, jni, answer),
    )
}

/// Host pointers and handles a 32-bit guest was refused, every `REFUSED_*` key together: the measurement phase4-32bit P4b names its largest uncertainty; a gate on a 32-bit guest asserts it 0. Moved only by [`report_refused`], so it equals the sum of the atl-diag refused counts under those keys.
pub static ILP32_REFUSED: AtomicUsize = AtomicUsize::new(0);
/// Serialises every test that moves [`ILP32_REFUSED`] (the test threads share it): a refusal from another test would land between one test's two reads.
#[cfg(test)]
pub(in crate::engine) static REFUSED_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// atl-diag keys of a host pointer refused to a 32-bit guest, by where it was going.
pub(in crate::engine) const REFUSED_RESULT: &str = "native-bridge.ilp32-result";
pub(in crate::engine) const REFUSED_ARG: &str = "native-bridge.ilp32-argument";
pub(in crate::engine) const REFUSED_OUT: &str = "native-bridge.ilp32-out";
/// A guest handle the table does not hold (handles.rs), or no slot left for a host object.
pub(in crate::engine) const REFUSED_HANDLE: &str = "native-bridge.ilp32-handle";

/// Reports one host pointer refused to a 32-bit guest under `key` (`REFUSED_*`), `cause` naming the function and position, and counts it in [`ILP32_REFUSED`] (the one place that moves it).
pub(in crate::engine) fn report_refused(key: &str, cause: &str) {
    ILP32_REFUSED.fetch_add(1, Ordering::Relaxed);
    atl_diag::count(atl_diag::REFUSED, key, Some(cause));
}

/// A pointer result for a 32-bit guest (`guest`): at or above 4 GiB it cannot reach the guest, whose word would truncate it to another address; refused instead -- the answer becomes null (the C error value of a pointer result), counted in [`ILP32_REFUSED`] and reported with the JNI function's name (`jni`, e.g. `GetDirectBufferAddress` of a buffer host code allocated) or else the host function's symbol name. Below 4 GiB, and every other class or guest, unchanged.
fn ilp32_narrow(
    guest: HostRules,
    ret: Class,
    target: usize,
    jni: Option<&str>,
    answer: Answer,
) -> Answer {
    if guest.word_bits != 32 || ret != Class::Ptr || answer.x0 <= WORD {
        return answer;
    }
    let who = jni.map_or_else(|| symbol_name(target), |n| format!("JNI {n}"));
    report_refused(REFUSED_RESULT, &format!("{who} answered {:#x}", answer.x0));
    Answer { x0: 0, ..answer }
}

/// The dynamic symbol `addr` starts, or the address when `dladdr` names none.
fn symbol_name(addr: usize) -> String {
    // SAFETY: Dl_info is plain pointers; all zero is a valid value.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: dladdr reads only the address; `info` is a local it fills.
    let found = unsafe { libc::dladdr(addr as *const std::ffi::c_void, &mut info) } != 0;
    if found && !info.dli_sname.is_null() {
        // SAFETY: dladdr's symbol name, NUL-terminated, in a loaded image.
        unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }
            .to_string_lossy()
            .into_owned()
    } else {
        format!("{addr:#x}")
    }
}

/// Integer words read from a 32-bit guest for a call with no prototype: r0..r3 and twelve stack words, which AAPCS64's image holds whole (eight registers, sixteen stack words).
const RAW_WORDS: [Class; 16] = [Class::Int {
    bits: 32,
    signed: false,
}; 16];

/// The guest lanes `lanes` (convention `guest`) as AAPCS64's image, which the host call reads ([`call_host`]): aarch64's own lanes as they are; a 32-bit guest's placed by the parameters' classes ([`place`], words widened by signedness, a pair joined), a call without a prototype as zero-extended words ([`RAW_WORDS`]). None when the image cannot carry them (R-1).
fn up_lanes(guest: HostRules, lanes: &AtlLanes, params: Option<&[Class]>) -> Option<AtlLanes> {
    if guest.word_bits == AAPCS64.word_bits {
        return Some(*lanes);
    }
    place(params.unwrap_or(&RAW_WORDS), lanes, guest, UP_IMAGE)
}

/// The image a host up call carries: AAPCS64's with eight stack words, what `host_call` passes (variadic.rs `first8`, x86_64; the bridgegen `lane_refusal` build check of proxy rows assumes the same). The aarch64 guest's own window ([`AAPCS64`]) is wider: only down calls write the extra words.
const UP_IMAGE: HostRules = HostRules {
    stack_words: 8,
    ..AAPCS64
};

/// The guest's result registers for the host answer `answer` of class `ret`: aarch64 takes it as it is; a 32-bit guest r0 / r0:r1 ([`guest_ret32`]).
fn guest_answer(guest: HostRules, ret: Class, answer: Answer) -> Answer {
    if guest.word_bits == AAPCS64.word_bits {
        return answer;
    }
    let [x0, x1] = guest_ret32(ret, answer);
    Answer { x0, x1, v0: 0 }
}

/// The result class the call is made with: an fp result in x0 is the break `ret_int`.
#[cfg(not(atl_nb_ret_int))]
fn fp_result(ret: Class) -> Class {
    ret
}
/// Break `ret_int`: an fp result is read from x0, as an integer's is.
#[cfg(atl_nb_ret_int)]
fn fp_result(_ret: Class) -> Class {
    Class::Int {
        bits: 64,
        signed: true,
    }
}

/// Refuses an up call whose arguments do not fit the lane image on one side (R-1): with fp-to-int on the host, a prototype's ninth fp argument is a guest stack word but a host register, so the counts differ. Named and counted, as [`refuse_down`].
fn refuse_up(target: usize) -> ! {
    static REFUSED: AtomicUsize = AtomicUsize::new(0);
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused up call to {target:#x}: its arguments need more stack words on one side than the lane image carries ({} each; R-1, refusals: {n})",
        atl_engine_api::LANE_STACK
    ))
}

/// Refusals by kind, in [`refuse`]'s order: a forged id, a slot with no function, a `va_list` without a shorty source, a shorty character outside jni's, an x0 that is no proxy object, a `...` call's arguments past the lanes' stack window. A refusal aborts, so each count reaches the abort message only.
static REFUSED: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];

/// The named abort of a refused guest object call: the message says which call and why, with the count of that kind.
fn refuse(r: Refused) -> ! {
    let (kind, why) = match r {
        Refused::Forged { id, table, len } => (
            0,
            format!("object id {id} past the {table} table ({len} entries)"),
        ),
        Refused::Reserved { id, table } => (
            1,
            format!("{table} slot {id} is reserved (jni.h reserved*), no function"),
        ),
        Refused::NotProxy { id, guest } => (
            4,
            format!("object call {id} on {guest:#x}, which no proxy table backs"),
        ),
        Refused::NoShorty { name } => (
            2,
            format!("JNI {name} takes a va_list and the runtime gave no getMethodShorty"),
        ),
        Refused::Shorty { name, c } => (
            3,
            format!(
                "JNI {name}: shorty character {:?} is not a JNI type",
                char::from(c)
            ),
        ),
        Refused::VariadicWindow { name } => (
            5,
            format!("JNI {name}: a `...` call's arguments run past the lanes' stack window"),
        ),
    };
    let n = REFUSED[kind].fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused {why} (refusals of this kind: {n})"
    ))
}

/// Host->guest entry (Q6-4, bridgegen down.rs): a down shell's entry calls it with the shell's `DownSlot` literal and its frame's lanes.
///
/// # Safety
/// `slot` is the leaked `DownSlot` place.rs `down` wrote into the shell; `host` is the shell's frame, an `AtlLanes` live for the call and not otherwise accessed until it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_nb_down_dispatch(slot: *const DownSlot, host: *mut AtlLanes) {
    // SAFETY: per the contract above: a 'static binding and the shell's frame.
    down_body(unsafe { &*slot }, unsafe { &mut *host });
}

/// A JNI method's reference arguments for a 32-bit guest, in host lanes `host` placed by [`HOST`] for classes `params` (`down_params` of [`Down::Env`]: env, class or `this`, then the shorty's): each reference at or above 4 GiB -- ART's transition references point into the method's managed frame on the host stack -- becomes a local reference to the same object (jni.rs `local_ref`), which ART keeps below 4 GiB and pops with the method's frame. A reference that fits passes as ART made it. Arguments past the host's stack window are left to [`place`], which refuses the call.
fn local_refs(params: &[Class], host: &mut AtlLanes) {
    let env = host.regs[0] as usize;
    let mut at = Assign::new(HOST);
    for (k, &class) in params.iter().enumerate() {
        let Some(slot) = at.next(class) else { return };
        // parameter 0 is the env itself; every later pointer of a JNI method is a reference (shorty `L`, the implicit class or `this`)
        if k == 0 || class != Class::Ptr {
            continue;
        }
        let lane = match slot {
            Slot::X(i, _) => &mut host.regs[i],
            Slot::Stack(i, _) => &mut host.stack[i],
            Slot::V(_) => continue,
        };
        // break `p5_no_local_ref`: the transition reference crosses as it is, so `place` refuses it (the arm app gate goes red)
        if *lane > WORD && !cfg!(atl_nb_p5_no_local_ref) {
            // SAFETY: `env` is the host JNIEnv the runtime called this native method with (parameter 0 of `Down::Env`), the lane one of its reference arguments.
            *lane = unsafe { super::jni::local_ref(env, *lane as usize) } as u64;
        }
    }
}

/// The host->guest body of down slot `d` (Q6-4): `host` is the shell's frame with the host call's lanes; the guest call's lanes are placed by the slot's classes (`place`), and the guest's answer goes back in `host.ret[0]` and `host.fp[0]`, which the shell returns.
fn down_body(d: &DownSlot, host: &mut AtlLanes) {
    let rules = super::qemu::arch().rules();
    // an ILP32 callback's host object (`Down::Handle`, the host's first integer) -> its handle before placement; a handle issued for this call only is freed when the guest returns
    let scoped = d.kind.host_object(&mut host.regs[0]);
    // a 32-bit guest's JNI method: its reference arguments made to fit, while x0 is still the host env
    if d.kind == Down::Env && rules.word_bits != AAPCS64.word_bits {
        local_refs(&d.params, host);
    }
    // the first argument of a JNI method, of JNI_OnLoad or of an SL callback is a host env, vm or interface: the guest's own object stands for it (jni.rs, sles.rs), and it is the first integer; an slObjectCallback's 6th is the 6th integer. Rewritten in the host's lanes before placement, so a 32-bit guest's placement sees the guest object (guest-addressable), never the host's
    #[cfg(not(atl_nb_down_noenv))]
    {
        let (mut first, mut sixth) = (host.regs[0] as usize, *HOST.int_arg(host, 5) as usize);
        d.kind.guest_args(&mut first, &mut sixth);
        host.regs[0] = first as u64;
        *HOST.int_arg(host, 5) = sixth as u64;
    }
    let Some(mut guest) = place(&d.params, host, HOST, rules) else {
        refuse_down(d.target, &d.params)
    };
    // break `down_noenv` (qemu-jni gate, checks o, m and x): the kind is not read, so the guest gets the host's env or vm
    #[cfg(atl_nb_down_noenv)]
    let _ = d.kind;
    super::qemu::call_down(d.target, &mut guest);
    if let Some(h) = scoped {
        super::handles::release(h);
    }
    // a 32-bit guest's r0 / r0:r1 joined and widened first (`host_ret32`); aarch64's x0 and v0 are already the host's lanes
    let (x, v) = if rules.word_bits == AAPCS64.word_bits {
        (guest.ret[0], guest.fp[0])
    } else {
        let class = match d.ret {
            b'V' => Class::Void,
            c => shorty_class(c).unwrap_or_else(|| {
                unsupported(&format!(
                    "qemu crossing: down result shorty {:?} has no class",
                    c as char
                ))
            }),
        };
        let a = host_ret32(class, [guest.ret[0], guest.ret[1]]);
        (a.x0, a.v0)
    };
    (host.ret[0], host.fp[0]) = host_result(d.ret, x, v, HOST);
}

// The shells' frame is an `atl_lanes`: bridgegen's offsets (down.rs, written into the shells' asm) must be the struct's.
const _: () = {
    use super::qemu::down_abi as p;
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(AtlLanes, regs) == p::LANES_REGS_AT);
    assert!(offset_of!(AtlLanes, fp) == p::LANES_FP_AT);
    assert!(offset_of!(AtlLanes, stack) == p::LANES_STACK_AT);
    assert!(offset_of!(AtlLanes, ret) == p::LANES_RET_AT);
    assert!(size_of::<AtlLanes>() == p::LANES_SIZE);
};

/// How a calling convention fills and reads a call's lanes: the host's (bridgegen down.rs `HostAbi`, generated per host ISA) or the guest's ([`AAPCS64`], [`AAPCS32`]).
#[derive(Clone, Copy, Debug)]
pub(in crate::engine) struct HostRules {
    /// integer argument registers in the lanes' x bank
    int_lanes: usize,
    /// fp argument registers in the lanes' v bank; 0 = core registers only (AAPCS32 base standard)
    fp_lanes: usize,
    /// an fp argument past the fp registers takes a free integer register before the stack
    fp_to_int: bool,
    /// a narrow integer result is widened to 64 bits by its type
    extend_int_ret: bool,
    /// a float in an fp register is NaN-boxed (argument and result)
    nan_box_f32: bool,
    /// stack words of the lanes' window this side carries: the host shells copy [`HOST`]'s, the engine's target reads and writes the guest's
    stack_words: usize,
    /// bits of one integer register and one stack word: 64, or 32 (a 64-bit value then takes an aligned pair of lanes, [`Assign::next`])
    word_bits: u32,
}

impl HostRules {
    /// Integer argument `k` of a call whose arguments before it are one word each (pointers, `int`s: a JNI V slot's env/object/methodID, an SL callback's first five) in `lanes` filled by these rules: register `k` below `int_lanes`, else stack word `k - int_lanes` (AAPCS32: the 5th integer is the first word at sp; AAPCS64: the 9th). Refuses an index past the window.
    pub(in crate::engine) fn int_arg(self, lanes: &mut AtlLanes, k: usize) -> &mut u64 {
        // break `regs_k_raw`: register k on every guest, as before (the AAPCS32 unit test goes red)
        #[cfg(atl_nb_regs_k_raw)]
        return &mut lanes.regs[k];
        #[cfg(not(atl_nb_regs_k_raw))]
        if k < self.int_lanes {
            &mut lanes.regs[k]
        } else {
            let w = k - self.int_lanes;
            if w >= self.stack_words {
                unsupported("qemu crossing: an integer argument past the lanes' stack window");
            }
            &mut lanes.stack[w]
        }
    }
}

/// The aarch64 guest's rules (qemu/arch.rs `Arch::Aarch64`), AAPCS64: eight integer registers, an fp argument past v7 goes to the stack, results and floats unshaped (a narrow value's upper bits are unspecified, s-registers are read low); sixteen stack words (the aarch64 engine's window, atl-qemu-engine engine/aarch64.rs `STACK_SLOTS`). Also the image every host call reads ([`up_lanes`]).
pub(in crate::engine) const AAPCS64: HostRules = HostRules {
    int_lanes: 8,
    fp_lanes: 8,
    fp_to_int: false,
    extend_int_ret: false,
    nan_box_f32: false,
    stack_words: atl_engine_api::LANE_STACK,
    word_bits: 64,
};

/// The arm guest's rules, AAPCS32 base standard (Android armeabi-v7a is softfp: no `Tag_ABI_VFP_args`, the P2 guest ABI gate): r0..r3, no fp argument register, so a float or double travels in core registers or on the stack as an integer of its size would; 4-byte words; the arm engine's whole 16-word window (atl-qemu-engine engine/arm.rs `STACK_SLOTS`). Rule numbers: AAPCS32 2026Q3, Parameter Passing (base PCS). qemu/arch.rs `Arch::Arm`.
pub(in crate::engine) const AAPCS32: HostRules = HostRules {
    int_lanes: 4,
    fp_lanes: 0,
    fp_to_int: true,
    extend_int_ret: false,
    nan_box_f32: false,
    stack_words: atl_engine_api::LANE_STACK,
    word_bits: 32,
};

/// The build's host rules (64-bit hosts with eight fp argument registers: SysV xmm0..7, LP64D fa0..7).
const HOST: HostRules = {
    use super::qemu::down_abi as p;
    HostRules {
        int_lanes: p::HOST_INT_LANES,
        fp_lanes: 8,
        fp_to_int: p::HOST_FP_TO_INT,
        extend_int_ret: p::HOST_EXTEND_INT_RET,
        nan_box_f32: p::HOST_NAN_BOX_F32,
        stack_words: p::HOST_STACK_WORDS,
        word_bits: 64,
    }
};
const _: () = assert!(HOST.int_lanes <= atl_engine_api::LANE_REGS);
const _: () = assert!(HOST.stack_words <= atl_engine_api::LANE_STACK);
const _: () = assert!(AAPCS64.stack_words <= atl_engine_api::LANE_STACK);
const _: () = assert!(AAPCS32.int_lanes <= atl_engine_api::LANE_REGS);

/// The result shorty type of a down call: the shorty's first byte; JNI_OnLoad / JNI_OnUnload (no shorty) answer a `jint` / nothing, so `I`.
pub(in crate::engine) fn down_ret(shorty: Option<&[u8]>) -> u8 {
    shorty.and_then(|s| s.first().copied()).unwrap_or(b'I')
}

/// The host's answer words (x0 lane, v0 lane) for a guest result of shorty type `ret`: AAPCS64 leaves the bits above a narrow result unspecified (w0, s0 with zero upper bits), so a host that needs them shaped gets them so -- integers widened by their type (`Z`/`C` zero-, `B`/`S`/`I` sign-extended), a float NaN-boxed.
fn host_result(ret: u8, x: u64, v: u64, rules: HostRules) -> (u64, u64) {
    let x = if rules.extend_int_ret {
        match ret {
            b'Z' => u64::from(x as u8),
            b'C' => u64::from(x as u16),
            b'B' => x as i8 as i64 as u64,
            b'S' => x as i16 as i64 as u64,
            b'I' => x as i32 as i64 as u64,
            _ => x,
        }
    } else {
        x
    };
    let v = if rules.nan_box_f32 && ret == b'F' {
        v | 0xFFFF_FFFF_0000_0000
    } else {
        v
    };
    (x, v)
}

/// The lane classes of a down call's arguments (Q6-4). A JNI method's env and class (or `this`) come first, a `@CriticalNative` method's none; JNI_OnLoad and JNI_OnUnload, which ART calls with no shorty, take `(JavaVM*, void*)`. The shorty is `ArtMethod::GetShorty`: its first byte is the result, each further byte one parameter.
pub(in crate::engine) fn down_params(
    kind: Down,
    shorty: Option<&[u8]>,
) -> Result<Vec<Class>, String> {
    let (implicit, s): (&[Class], &[u8]) = match (kind, shorty) {
        (Down::Vm, None) => return Ok(vec![Class::Ptr, Class::Ptr]),
        (Down::Env, Some(s)) => (&[Class::Ptr, Class::Ptr], s),
        // an ILP32 callback with a host object first: that object is the shorty's first `L`
        (Down::Plain | Down::Handle(_), Some(s)) => (&[], s),
        // an SL callback: the interface first, then the shorty's (sles.rs gives one per callback type)
        (Down::Itf(_), Some(s)) => (&[Class::Ptr], s),
        (kind, shorty) => {
            return Err(format!("down call of kind {kind:?} with shorty {shorty:?}"));
        }
    };
    let Some((ret, args)) = s.split_first() else {
        return Err("down call with an empty shorty".into());
    };
    if !b"VZBCSIJFDL".contains(ret) {
        return Err(format!("shorty result {:?}: no such type", *ret as char));
    }
    let mut params = implicit.to_vec();
    for &c in args {
        params.push(
            shorty_class(c)
                .ok_or_else(|| format!("shorty parameter {:?}: no such type", c as char))?,
        );
    }
    // break `shorty_swap` (qemu-jni gate, checks m, x and R): the first shorty parameter's class is flipped, so the host's argument reads the other bank; the 23-integer R-1 call then needs sixteen host stack words, which fit, and is not refused
    #[cfg(atl_nb_shorty_swap)]
    if let Some(p) = params.get_mut(2) {
        *p = if p.is_int() {
            Class::F64
        } else {
            Class::Int {
                bits: 64,
                signed: true,
            }
        };
    }
    Ok(params)
}

/// The class of a JNI shorty parameter letter (jni.h: `jboolean` u8, `jbyte` i8, `jchar` u16, `jshort` i16, `jint` i32, `jlong` i64, a reference `L`), None for a letter jni has no parameter type for.
fn shorty_class(c: u8) -> Option<Class> {
    let int = |bits, signed| Some(Class::Int { bits, signed });
    match c {
        b'Z' => int(8, false),
        b'B' => int(8, true),
        b'C' => int(16, false),
        b'S' => int(16, true),
        b'I' => int(32, true),
        b'J' => int(64, true),
        b'L' => Some(Class::Ptr),
        b'F' => Some(Class::F32),
        b'D' => Some(Class::F64),
        _ => None,
    }
}

/// Where one argument sits in an image: integer registers, an fp register, or stack words (indices into the lanes' regs, fp, stack), with the lanes it takes there: 2 for a 64-bit value on a 32-bit side (low word first, little-endian), else 1.
#[derive(Clone, Copy)]
enum Slot {
    X(usize, usize),
    V(usize),
    Stack(usize, usize),
}

/// The low 32 bits of a lane: one word of a 32-bit side.
const WORD: u64 = 0xFFFF_FFFF;

/// Assigns argument slots under `rules`, in declaration order (one walk per side of [`place`]).
struct Assign {
    rules: HostRules,
    x: usize,
    v: usize,
    stack: usize,
}

impl Assign {
    fn new(rules: HostRules) -> Self {
        Self {
            rules,
            x: 0,
            v: 0,
            stack: 0,
        }
    }

    /// The next slot of an argument of class `class`: an integer (a `va_list` lane holds a pointer) from the integer registers, a float or double from the fp registers; an fp argument past them (or every one, with no fp registers) takes a free integer register first where `fp_to_int` (RISC-V, AAPCS32), else the stack; the rest the stack. None past the side's stack words (`stack_words`).
    /// A 32-bit side (AAPCS32 2026Q3, Parameter Passing (base PCS)) gives a 64-bit value two words starting at an even lane: C.3 rounds the register number up to even, C.7 the stack offset to a double-word (the stack is 8-aligned at a call: The Stack, SP mod 8 = 0). So such a value is never split (after C.3 the register number is 0, 2 or 4, and C.5 needs one below 4 with words left over). An argument that does not fit the registers closes them (C.6): every later one goes to the stack, a word argument too. Narrow integers take one word (B.2).
    fn next(&mut self, class: Class) -> Option<Slot> {
        let words = if self.rules.word_bits == 32 && class.bytes(32) == 8 {
            2
        } else {
            1
        };
        let slot = if class.is_fp() && self.v < self.rules.fp_lanes {
            self.v += 1;
            Slot::V(self.v - 1)
        } else if !class.is_fp() || self.rules.fp_to_int {
            // C.3
            let x = self.x.next_multiple_of(words);
            // C.4
            if x + words <= self.rules.int_lanes {
                self.x = x + words;
                Slot::X(x, words)
            } else {
                // C.6
                self.x = self.rules.int_lanes;
                self.stack(words)
            }
        } else {
            self.stack(words)
        };
        match slot {
            Slot::Stack(s, n) if s + n > self.rules.stack_words => None,
            s => Some(s),
        }
    }

    /// The next `words` stack words, aligned to their count (C.7, C.8).
    fn stack(&mut self, words: usize) -> Slot {
        let s = self.stack.next_multiple_of(words);
        self.stack = s + words;
        Slot::Stack(s, words)
    }
}

/// The value of `n` lanes from `i`: one lane, or a 32-bit side's pair (low word first).
fn read(lanes: &[u64], i: usize, n: usize) -> u64 {
    if n == 2 {
        (lanes[i] & WORD) | (lanes[i + 1] << 32)
    } else {
        lanes[i]
    }
}

/// Stores `value` into `n` lanes from `i`, as [`read`] reads them.
fn write(lanes: &mut [u64], i: usize, n: usize, value: u64) {
    if n == 2 {
        lanes[i] = value & WORD;
        lanes[i + 1] = value >> 32;
    } else {
        lanes[i] = value;
    }
}

/// A value of class `class` read from a side of `from` bits, as a side of `to` bits holds it. Between sides of one width it is unchanged (a 64-bit value on a 32-bit side was joined by [`read`]). Into 64 bits from 32: an integer is extended by its signedness (B.2 left a narrower one extended to the word; a 64-bit host's `long` parameter reads all 64 bits), a pointer zero-extended, a float's bits stay low. Into 32 bits from 64: the low word (SysV and LP64D leave the bits above a 32-bit argument to the callee); a host pointer at or above 4 GiB never reaches here ([`place`] refuses it, phase4-32bit P4b).
fn convert(class: Class, value: u64, from: u32, to: u32) -> u64 {
    if from == to || class.bytes(32) == 8 {
        return value;
    }
    match (class, to) {
        (Class::Int { signed: true, .. }, 64) => i64::from(value as u32 as i32) as u64,
        _ => value & WORD,
    }
}

/// The `to` side's lanes for a call whose arguments have the classes `params`, read from `from`, the `from` side's image (x[0..int_lanes], v[0..fp_lanes], its stack words). One placement for both directions: down = (host rules -> guest rules), up = (guest rules -> host rules), the guest's [`AAPCS64`] or [`AAPCS32`].
/// Every ABI here takes integers and fp arguments by class in declaration order ([`Assign::next`]: SysV 6 integer registers, LP64D and AAPCS64 8, AAPCS32 4; eight fp registers, none on AAPCS32's base standard). A value crossing between word widths is [`convert`]ed. A float going into an fp register of a `nan_box_f32` side is NaN-boxed (RISC-V: upper 32 bits ones; AArch64 leaves them zero). A pointer at or above 4 GiB going to a 32-bit side cannot be held by its word: refused as a result is ([`ilp32_narrow`]) -- passed null, counted in [`ILP32_REFUSED`], reported. `None` when either side needs more stack words than it carries (R-1): with fp-to-int on one side, the other can need more stack words.
fn place(
    params: &[Class],
    from: &AtlLanes,
    from_rules: HostRules,
    to_rules: HostRules,
) -> Option<AtlLanes> {
    let mut to = AtlLanes::default();
    let (mut src, mut dst) = (Assign::new(from_rules), Assign::new(to_rules));
    for (k, &class) in params.iter().enumerate() {
        let value = match src.next(class)? {
            Slot::X(i, n) => read(&from.regs, i, n),
            Slot::V(i) => from.fp[i],
            Slot::Stack(i, n) => read(&from.stack, i, n),
        };
        let value = if class == Class::Ptr && to_rules.word_bits == 32 && value > WORD {
            report_refused(REFUSED_ARG, &format!("argument {k} = {value:#x}"));
            0
        } else {
            convert(class, value, from_rules.word_bits, to_rules.word_bits)
        };
        match dst.next(class)? {
            Slot::X(i, n) => write(&mut to.regs, i, n, value),
            Slot::V(i) if class == Class::F32 && to_rules.nan_box_f32 => {
                to.fp[i] = value | 0xFFFF_FFFF_0000_0000;
            }
            Slot::V(i) => to.fp[i] = value,
            Slot::Stack(i, n) => write(&mut to.stack, i, n, value),
        }
    }
    Some(to)
}

/// A 32-bit guest's result lanes `[r0, r1]` for a host answer of class `ret` (AAPCS32 2026Q3, Result Return (base PCS)): a double-word (`long long`, softfp `double`) in r0 (low word) and r1; a word (`int`, pointer, softfp `float`) in r0; an integer narrower than a word zero- or sign-extended to it (SysV returns `bool`/`char`/`short` in the low bits of %rax with the rest unspecified, so the extension is made here); void none.
fn guest_ret32(ret: Class, answer: Answer) -> [u64; 2] {
    let pair = |v: u64| [v & WORD, v >> 32];
    match ret {
        Class::Void => [0, 0],
        Class::F64 => pair(answer.v0),
        Class::F32 => [answer.v0 & WORD, 0],
        Class::Int { bits: 64, .. } => pair(answer.x0),
        Class::Int { bits, signed } if bits < 32 => {
            let shift = 64 - u32::from(bits);
            let v = if signed {
                ((answer.x0 << shift) as i64 >> shift) as u64
            } else {
                answer.x0 << shift >> shift
            };
            [v & WORD, 0]
        }
        _ => [answer.x0 & WORD, 0],
    }
}

/// The host answer (x0 lane, fp lane) of a 32-bit guest's result `[r0, r1]` of class `ret`: [`guest_ret32`] read the other way, an integer of a word or less extended to the host's 64 bits by its signedness ([`convert`]), a pointer zero-extended. A host that shapes narrow results further (LP64D, `host_result`) does so after.
fn host_ret32(ret: Class, r: [u64; 2]) -> Answer {
    let joined = (r[0] & WORD) | (r[1] << 32);
    let (x0, v0) = match ret {
        Class::Void => (0, 0),
        Class::F64 => (0, joined),
        Class::F32 => (0, r[0] & WORD),
        Class::Int { bits: 64, .. } => (joined, 0),
        _ => (convert(ret, r[0], 32, 64), 0),
    };
    Answer { x0, x1: 0, v0 }
}

/// Refuses a down call whose arguments need more stack words than one side's window (R-1, Q6-4): the host shell's `HOST_STACK_WORDS`, the guest's `stack_words` (aarch64 8). Named and counted: a shorty this size fails on every call, so the count says how often it was asked; the integer / fp argument counts size the call.
fn refuse_down(target: usize, params: &[Class]) -> ! {
    static REFUSED: AtomicUsize = AtomicUsize::new(0);
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    let ints = params.iter().filter(|c| c.is_int()).count();
    let fps = params.iter().filter(|c| c.is_fp()).count();
    let guest = super::qemu::arch().rules().stack_words;
    unsupported(&format!(
        "qemu crossing: refused down call to {target:#x} ({ints} integer, {fps} fp arguments): its arguments need more stack words than the lane image carries (host {}, guest {guest}; R-1, refusals: {n})",
        HOST.stack_words
    ))
}

#[cfg(test)]
mod place_tests {
    use super::*;

    /// Argument 5 (an SL callback's 6th, `down_body`; a JNI V slot's lane, `call_object`): AAPCS64 x5; AAPCS32 r0..r3 then the words at sp, so stack word 1 (AAPCS32 2026Q3, base PCS: core registers r0-r3, then the stack in order).
    #[test]
    fn int_arg_follows_the_guest_rules() {
        let mut l = AtlLanes::default();
        l.regs[3] = 0x33;
        l.regs[5] = 0x55;
        l.stack[1] = 0x51;
        assert_eq!(*AAPCS64.int_arg(&mut l, 5), 0x55);
        assert_eq!(*AAPCS32.int_arg(&mut l, 5), 0x51);
        assert_eq!(*AAPCS32.int_arg(&mut l, 3), 0x33);
        *AAPCS32.int_arg(&mut l, 4) = 0x40;
        assert_eq!(l.stack[0], 0x40);
    }

    /// x86_64 SysV (bridgegen x86.rs `DOWN`): six integer registers, fp overflow to the stack, results unshaped.
    const SYSV: HostRules = HostRules {
        int_lanes: 6,
        fp_lanes: 8,
        fp_to_int: false,
        extend_int_ret: false,
        nan_box_f32: false,
        stack_words: HOST.stack_words,
        word_bits: 64,
    };
    /// riscv64 LP64D (bridgegen riscv64.rs `DOWN`): eight integer registers, fp overflow to a free integer register, results shaped.
    const LP64D: HostRules = HostRules {
        int_lanes: 8,
        fp_lanes: 8,
        fp_to_int: true,
        extend_int_ret: true,
        nan_box_f32: true,
        stack_words: HOST.stack_words,
        word_bits: 64,
    };
    /// An integer lane on two 64-bit sides: every int class places alike there.
    const INT: Class = Class::Int {
        bits: 64,
        signed: true,
    };
    /// jni.h `jint`.
    const I32: Class = Class::Int {
        bits: 32,
        signed: true,
    };

    /// An image with `regs`, `fp` and `stack` as the banks' first lanes, the rest 0.
    fn img(regs: &[u64], fp: &[u64], stack: &[u64]) -> AtlLanes {
        let mut l = AtlLanes::default();
        l.regs[..regs.len()].copy_from_slice(regs);
        l.fp[..fp.len()].copy_from_slice(fp);
        l.stack[..stack.len()].copy_from_slice(stack);
        l
    }

    /// Up, glClearColor(1, 0, 0, 1): AArch64 writes s0..s3 with the upper 32 bits zero (1.0f = 0x3F80_0000); LP64D reads a float in fa0..fa3 only NaN-boxed (psABI: upper 32 bits ones), else as canonical NaN.
    #[test]
    fn up_floats_are_nan_boxed_on_lp64d() {
        let guest = img(&[], &[0x3F80_0000, 0, 0, 0x3F80_0000], &[]);
        let host = place(&[Class::F32; 4], &guest, AAPCS64, LP64D).expect("four fp registers");
        assert_eq!(
            host.fp[..4],
            [
                0xFFFF_FFFF_3F80_0000,
                0xFFFF_FFFF_0000_0000,
                0xFFFF_FFFF_0000_0000,
                0xFFFF_FFFF_3F80_0000
            ]
        );
        // SysV reads xmm's low 32 bits: no shaping
        let sysv = place(&[Class::F32; 4], &guest, AAPCS64, SYSV).expect("four fp registers");
        assert_eq!(sysv.fp, guest.fp);
        // a double is never boxed: 1.0 = 0x3FF0_0000_0000_0000
        let d = img(&[], &[0x3FF0_0000_0000_0000], &[]);
        let host = place(&[Class::F64], &d, AAPCS64, LP64D).expect("one fp register");
        assert_eq!(host.fp[0], 0x3FF0_0000_0000_0000);
    }

    /// Up, glTexImage2D's nine integers: AAPCS64 x0..x7 + its first stack word -> LP64D a0..a7 + its first stack word (both: eight integer registers, the rest on the stack in order).
    #[test]
    fn up_nine_integers_on_lp64d() {
        let guest = img(&[1, 2, 3, 4, 5, 6, 7, 8], &[], &[9]);
        let host = place(&[INT; 9], &guest, AAPCS64, LP64D).expect("one stack word");
        assert_eq!(host, img(&[1, 2, 3, 4, 5, 6, 7, 8], &[], &[9]));
    }

    /// Up, ten doubles then one int: AAPCS64 v0..v7, the ninth and tenth its first two stack words, the int x0; LP64D fa0..fa7, then a0, a1 (integer convention once the fp registers are used up), the int a2 -- the down test's rule read the other way.
    #[test]
    fn up_fp_overflow_takes_integer_registers_on_lp64d() {
        let guest = img(&[77], &[1, 2, 3, 4, 5, 6, 7, 8], &[9, 10]);
        let mut params = vec![Class::F64; 10];
        params.push(INT);
        let host = place(&params, &guest, AAPCS64, LP64D).expect("two guest stack words");
        assert_eq!(host, img(&[9, 10, 77], &[1, 2, 3, 4, 5, 6, 7, 8], &[]));
    }

    /// Up, glUniform4f(GLint, 4 x GLfloat): the int in a0, the floats boxed in fa0..fa3 -- banks counted apart.
    #[test]
    fn up_int_then_floats_count_banks_apart() {
        let guest = img(
            &[5],
            &[0x3F80_0000, 0x4000_0000, 0x4040_0000, 0x4080_0000],
            &[],
        );
        let mut params = vec![INT];
        params.extend([Class::F32; 4]);
        let host = place(&params, &guest, AAPCS64, LP64D).expect("registers only");
        assert_eq!(host.regs[0], 5);
        assert_eq!(
            host.fp[..4],
            [
                0xFFFF_FFFF_3F80_0000,
                0xFFFF_FFFF_4000_0000,
                0xFFFF_FFFF_4040_0000,
                0xFFFF_FFFF_4080_0000
            ]
        );
    }

    /// LP64D, ten doubles then one int: fa0..fa7 take eight, the ninth and tenth take a0 and a1 (psABI: integer convention once the fp registers are used up), the int takes a2. AAPCS64: v0..v7, then the ninth and tenth are its first two stack words, the int is x0.
    #[test]
    fn lp64d_fp_overflow_takes_integer_registers() {
        let host = img(&[9, 10, 77], &[1, 2, 3, 4, 5, 6, 7, 8], &[]);
        let mut params = vec![Class::F64; 10];
        params.push(INT);
        let guest = place(&params, &host, LP64D, AAPCS64).expect("no host stack word");
        assert_eq!(guest, img(&[77], &[1, 2, 3, 4, 5, 6, 7, 8], &[9, 10]));
    }

    /// LP64D, nine integers: a0..a7 take eight, the ninth is the host's first stack word; AAPCS64 alike (x0..x7, stack 0).
    #[test]
    fn lp64d_nine_integers() {
        let host = img(&[1, 2, 3, 4, 5, 6, 7, 8], &[], &[9]);
        let guest = place(&[INT; 9], &host, LP64D, AAPCS64).expect("one stack word");
        assert_eq!(guest, img(&[1, 2, 3, 4, 5, 6, 7, 8], &[], &[9]));
    }

    /// LP64D, twenty-five doubles: the host holds sixteen in registers (8 fp + 8 int) and nine stack words; AAPCS64 needs seventeen stack words (8 in v), one past its sixteen -> refused, never an out-of-bounds write. Twenty-four fit both.
    #[test]
    fn lp64d_guest_stack_overflow_is_refused() {
        assert!(place(&[Class::F64; 24], &AtlLanes::default(), LP64D, AAPCS64).is_some());
        assert!(place(&[Class::F64; 25], &AtlLanes::default(), LP64D, AAPCS64).is_none());
    }

    /// LP64D results (psABI: widen by type to 32 bits, then sign-extend to 64; float NaN-boxed); SysV results pass unchanged.
    #[test]
    fn host_result_shapes_narrow_answers() {
        let junk = 0xDEAD_BEEF_0000_0000;
        assert_eq!(host_result(b'Z', junk | 0x01, 0, LP64D).0, 1);
        assert_eq!(host_result(b'C', junk | 0xFFFF, 0, LP64D).0, 0xFFFF);
        assert_eq!(
            host_result(b'B', junk | 0x80, 0, LP64D).0,
            0xFFFF_FFFF_FFFF_FF80
        );
        assert_eq!(
            host_result(b'S', junk | 0x8000, 0, LP64D).0,
            0xFFFF_FFFF_FFFF_8000
        );
        assert_eq!(
            host_result(b'I', junk | 0x8000_0000, 0, LP64D).0,
            0xFFFF_FFFF_8000_0000
        );
        assert_eq!(host_result(b'J', junk | 1, 0, LP64D).0, junk | 1);
        // 1.0f = 0x3F80_0000
        assert_eq!(
            host_result(b'F', 0, 0x3F80_0000, LP64D).1,
            0xFFFF_FFFF_3F80_0000
        );
        assert_eq!(
            host_result(b'D', 0, 0x3FF0_0000_0000_0000, LP64D).1,
            0x3FF0_0000_0000_0000
        );
        assert_eq!(
            host_result(b'I', junk | 7, 0x3F80_0000, SYSV),
            (junk | 7, 0x3F80_0000)
        );
    }

    #[test]
    fn down_ret_reads_the_shorty() {
        assert_eq!(down_ret(Some(&b"FI"[..])), b'F');
        assert_eq!(down_ret(None), b'I');
    }

    /// Nine integers: SysV takes six in %rdi..%r9 and three from the stack; AAPCS64 takes eight in x0..x7 and one from its stack. Expected values follow from those two rules.
    #[test]
    fn nine_integers_split_across_both_banks() {
        let host = img(&[10, 11, 12, 13, 14, 15], &[], &[100, 101, 102]);
        let guest = place(&[INT; 9], &host, SYSV, AAPCS64).expect("three stack words fit");
        assert_eq!(guest, img(&[10, 11, 12, 13, 14, 15, 100, 101], &[], &[102]));
    }

    /// Eight doubles fill %xmm0..7 on both sides; a ninth double is the first stack word on both; an integer after them is %rdi on SysV and x0 on AAPCS64.
    #[test]
    fn ninth_double_is_the_first_stack_word() {
        let host = img(&[77], &[1, 2, 3, 4, 5, 6, 7, 8], &[9]);
        let mut params = vec![Class::F64; 9];
        params.push(INT);
        let guest = place(&params, &host, SYSV, AAPCS64).expect("one stack word");
        assert_eq!(guest, img(&[77], &[1, 2, 3, 4, 5, 6, 7, 8], &[9]));
    }

    /// The seventh integer is a stack word on SysV but a register on AAPCS64: the stack word crosses into x6, and the fp argument after it is xmm0 on SysV and v0 on AAPCS64.
    #[test]
    fn a_stack_integer_lands_in_a_guest_register() {
        let host = img(&[1, 2, 3, 4, 5, 6], &[8], &[7]);
        let mut params = vec![INT; 7];
        params.push(Class::F64);
        let guest = place(&params, &host, SYSV, AAPCS64).expect("one stack word");
        assert_eq!(guest, img(&[1, 2, 3, 4, 5, 6, 7], &[8], &[]));
    }

    /// Candy Crush Saga's JNI method (env, class + 13 integers, 2 doubles): 15 integers take nine SysV stack words past six registers, more than the shell's window held before (eight); seven AAPCS64 stack words past eight registers. Host stack word k lands in guest x(k+6) for k < 2, then guest stack word k-2.
    #[test]
    fn fifteen_integers_cross_by_the_host_stack() {
        let host = img(
            &[1, 2, 3, 4, 5, 6],
            &[0xD1, 0xD2],
            &[7, 8, 9, 10, 11, 12, 13, 14, 15],
        );
        let mut params = vec![INT; 15];
        params.extend([Class::F64, Class::F64]);
        let guest = place(&params, &host, SYSV, AAPCS64).expect("nine host stack words fit");
        assert_eq!(
            guest,
            img(
                &[1, 2, 3, 4, 5, 6, 7, 8],
                &[0xD1, 0xD2],
                &[9, 10, 11, 12, 13, 14, 15]
            )
        );
    }

    /// Candy Crush Saga's second wide JNI method (17 integers, 2 doubles): nine AAPCS64 stack words past x0..x7, eleven SysV words past six registers; both windows hold sixteen.
    #[test]
    fn seventeen_integers_cross() {
        let host = img(
            &[1, 2, 3, 4, 5, 6],
            &[0xD1, 0xD2],
            &(7..=17).collect::<Vec<u64>>(),
        );
        let mut params = vec![INT; 17];
        params.extend([Class::F64, Class::F64]);
        let guest = place(&params, &host, SYSV, AAPCS64).expect("nine guest stack words fit");
        assert_eq!(
            guest,
            img(
                &[1, 2, 3, 4, 5, 6, 7, 8],
                &[0xD1, 0xD2],
                &(9..=17).collect::<Vec<u64>>()
            )
        );
    }

    /// Twenty-two integers need sixteen SysV stack words (six registers): the shell's window holds them. Twenty-three need seventeen: refused (R-1), though AAPCS64's side (fifteen words) fits.
    #[test]
    fn seventeen_host_stack_words_are_refused() {
        assert!(place(&[INT; 22], &AtlLanes::default(), SYSV, AAPCS64).is_some());
        assert!(place(&[INT; 23], &AtlLanes::default(), SYSV, AAPCS64).is_none());
    }

    /// The up image stays at eight stack words (variadic.rs passes eight): a 32-bit guest's call with seventeen words (four in r0..r3, thirteen on its stack) needs nine AAPCS64 stack words past x0..x7 and is refused, not truncated.
    #[test]
    fn up_image_is_eight_stack_words() {
        let words = [Class::Int {
            bits: 32,
            signed: false,
        }; 17];
        assert!(up_lanes(AAPCS32, &AtlLanes::default(), Some(&words[..16])).is_some());
        assert!(up_lanes(AAPCS32, &AtlLanes::default(), Some(&words)).is_none());
    }

    /// The shorty `DIDJ` (result double; int, double, long) under a JNI method: env and class first (pointers), then the three (jni.h: `jint` signed 32, `jlong` signed 64). JNI_OnLoad (no shorty) is `(JavaVM*, void*)`; a shorty without a method's env is refused.
    #[test]
    fn shorty_gives_the_classes() {
        assert_eq!(
            down_params(Down::Env, Some(&b"DIDJ"[..])),
            Ok(vec![Class::Ptr, Class::Ptr, I32, Class::F64, INT])
        );
        assert_eq!(
            down_params(Down::Plain, Some(&b"VF"[..])),
            Ok(vec![Class::F32])
        );
        assert_eq!(
            down_params(Down::Vm, None),
            Ok(vec![Class::Ptr, Class::Ptr])
        );
        assert!(down_params(Down::Env, None).is_err());
        assert!(down_params(Down::Vm, Some(&b"V"[..])).is_err());
        assert!(down_params(Down::Env, Some(&b"QI"[..])).is_err());
    }
}

/// AAPCS32 placement (phase4-32bit P3 slice 2). Every expected image is written from the rules of AAPCS32 2026Q3 (github.com/ARM-software/abi-aa `aapcs32/aapcs32.rst`), Parameter Passing (base PCS) and Result Return (base PCS), each test naming the rules it walks; the host side from x86-64 SysV (rdi, rsi, rdx, rcx, r8, r9 = regs[0..6], xmm0..7 = fp[0..8], then the stack). Never from running `place`.
/// The guest image is what the arm engine fills (atl-qemu-engine engine/arm.rs `trap_lanes`): r0..r3 in regs[0..4], the 4-byte words at sp in stack[0..16], each a 32-bit value in a u64 lane.
#[cfg(test)]
mod aapcs32_tests {
    use super::*;

    /// x86_64 SysV, as `place_tests::SYSV`.
    const SYSV: HostRules = HostRules {
        int_lanes: 6,
        fp_lanes: 8,
        fp_to_int: false,
        extend_int_ret: false,
        nan_box_f32: false,
        stack_words: HOST.stack_words,
        word_bits: 64,
    };
    const I32: Class = Class::Int {
        bits: 32,
        signed: true,
    };
    const U32: Class = Class::Int {
        bits: 32,
        signed: false,
    };
    const I64: Class = Class::Int {
        bits: 64,
        signed: true,
    };
    /// 1.5 = 0x3FF8_0000_0000_0000 (IEEE 754 binary64), 2.5 = 0x4004_0000_0000_0000, 3.5 = 0x400C_0000_0000_0000; 1.5f = 0x3FC0_0000 (binary32).
    const D1_5: u64 = 0x3FF8_0000_0000_0000;
    const D2_5: u64 = 0x4004_0000_0000_0000;
    const D3_5: u64 = 0x400C_0000_0000_0000;
    const F1_5: u64 = 0x3FC0_0000;

    fn img(regs: &[u64], fp: &[u64], stack: &[u64]) -> AtlLanes {
        let mut l = AtlLanes::default();
        l.regs[..regs.len()].copy_from_slice(regs);
        l.fp[..fp.len()].copy_from_slice(fp);
        l.stack[..stack.len()].copy_from_slice(stack);
        l
    }

    /// `f(int a, long long b, int c)`: a r0 (C.4); b: C.3 rounds NCRN 1 -> 2, so r2 (low word) and r3, r1 unused; c: NCRN 4, C.4 fails, C.5 needs NCRN < 4, C.6, C.8 -> the first stack word. SysV: rdi, rsi, rdx.
    #[test]
    fn up_long_long_skips_to_an_even_pair() {
        let b = 0x1122_3344_5566_7788;
        let guest = img(&[7, 0, 0x5566_7788, 0x1122_3344], &[], &[9]);
        let host = place(&[I32, I64, I32], &guest, AAPCS32, SYSV).expect("fits");
        assert_eq!(host, img(&[7, b, 9], &[], &[]));
        // down, the same call read the other way: SysV leaves the bits above an int argument to the callee, so junk there must not reach r0 or the stack word
        let junk = 0xDEAD_BEEF_0000_0000;
        let from = img(&[junk | 7, b, junk | 9], &[], &[]);
        assert_eq!(place(&[I32, I64, I32], &from, SYSV, AAPCS32), Some(guest));
    }

    /// `f(int a, int b, int c, long long d, int e)`: a..c r0..r2; d: C.3 NCRN 3 -> 4, C.4 fails, C.5 needs NCRN < 4, C.6, C.7 (NSAA 0, aligned), C.8 -> stack words 0 (low) and 1; e: stack word 2. Never split across r3 and the stack.
    #[test]
    fn up_long_long_after_three_words_goes_whole_to_the_stack() {
        let d = 0x0102_0304_0506_0708;
        let guest = img(&[1, 2, 3, 0], &[], &[0x0506_0708, 0x0102_0304, 5]);
        let host = place(&[I32, I32, I32, I64, I32], &guest, AAPCS32, SYSV).expect("fits");
        assert_eq!(host, img(&[1, 2, 3, d, 5], &[], &[]));
    }

    /// `f(int a, int b, int c, int x, int e, long long d)`: a..x r0..r3; e: stack word 0 (C.6, C.8, NSAA 4); d: C.7 rounds NSAA 4 -> 8, so stack words 2 (low) and 3, word 1 padding. SysV: six integers, rdi..r9.
    #[test]
    fn stacked_long_long_is_double_word_aligned() {
        let d = 0x0A0B_0C0D_0E0F_1011;
        let guest = img(&[1, 2, 3, 4], &[], &[5, 0, 0x0E0F_1011, 0x0A0B_0C0D]);
        let params = [I32, I32, I32, I32, I32, I64];
        assert_eq!(
            place(&params, &guest, AAPCS32, SYSV),
            Some(img(&[1, 2, 3, 4, 5, d], &[], &[]))
        );
        let host = img(&[1, 2, 3, 4, 5, d], &[], &[]);
        assert_eq!(place(&params, &host, SYSV, AAPCS32), Some(guest));
    }

    /// softfp `f(float x, double y)`: the base standard has no CPRC (no co-processor argument registers), so both are core-register arguments; x a word in r0 (C.4); y double-word aligned (Fundamental Data Types: double precision, alignment 8): C.3 NCRN 1 -> 2, r2 (low) and r3. SysV: xmm0, xmm1; no fp lane on the guest side.
    #[test]
    fn softfp_floats_travel_in_core_registers() {
        let guest = img(&[F1_5, 0, D2_5 & WORD, D2_5 >> 32], &[], &[]);
        let host = place(&[Class::F32, Class::F64], &guest, AAPCS32, SYSV).expect("fits");
        assert_eq!(host, img(&[], &[F1_5, D2_5], &[]));
        assert_eq!(
            place(&[Class::F32, Class::F64], &host, SYSV, AAPCS32),
            Some(guest)
        );
    }

    /// softfp `f(double a, double b, double c)`: a r0:r1, b r2:r3 (C.3, C.4), c: C.6, C.7, C.8 -> stack words 0 and 1. SysV: xmm0..2.
    #[test]
    fn softfp_third_double_is_two_stack_words() {
        let guest = img(
            &[D1_5 & WORD, D1_5 >> 32, D2_5 & WORD, D2_5 >> 32],
            &[],
            &[D3_5 & WORD, D3_5 >> 32],
        );
        let host = place(&[Class::F64; 3], &guest, AAPCS32, SYSV).expect("fits");
        assert_eq!(host, img(&[], &[D1_5, D2_5, D3_5], &[]));
    }

    /// Into a 64-bit host: `f(int a, unsigned b, long c, void *p)` with a = -1, b = 0xFFFF_FFFF, c = -2 (`long` is a signed word on AAPCS32, Arm C and C++ Language Mappings), p = 0x8000_0000. A host `long` reads all 64 bits, so a signed word is sign-extended, an unsigned one and a pointer zero-extended.
    #[test]
    fn words_widen_by_signedness() {
        let guest = img(
            &[0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFE, 0x8000_0000],
            &[],
            &[],
        );
        let host = place(&[I32, U32, I32, Class::Ptr], &guest, AAPCS32, SYSV).expect("fits");
        assert_eq!(
            host,
            img(
                &[u64::MAX, 0xFFFF_FFFF, 0xFFFF_FFFF_FFFF_FFFE, 0x8000_0000],
                &[],
                &[]
            )
        );
    }

    /// Down, eleven doubles from SysV (xmm0..7, then three stack words): the guest needs 2 doubles in r0..r3 and 9 x 2 = 18 stack words, past the arm engine's 16-word window -> refused. Ten need 16: placed.
    #[test]
    fn guest_window_overflow_is_refused() {
        assert!(place(&[Class::F64; 10], &AtlLanes::default(), SYSV, AAPCS32).is_some());
        assert!(place(&[Class::F64; 11], &AtlLanes::default(), SYSV, AAPCS32).is_none());
    }

    /// Result Return (base PCS): a double-word in r0 (low) and r1; a word in r0; a narrow integer extended to the word by its type (a bool's 0x80 junk-topped answer stays 0x80, a signed char 0x80 = -128 = 0xFFFF_FF80); softfp float in r0, double in r0:r1; void nothing.
    #[test]
    fn results_to_the_guest() {
        let junk = 0xDEAD_BEEF_0000_0000;
        let ans = |x0, v0| Answer { x0, x1: 0, v0 };
        assert_eq!(
            guest_ret32(I64, ans(0x1122_3344_5566_7788, 0)),
            [0x5566_7788, 0x1122_3344]
        );
        assert_eq!(
            guest_ret32(I32, ans(junk | 0x8000_0001, 0)),
            [0x8000_0001, 0]
        );
        assert_eq!(guest_ret32(Class::Ptr, ans(0x1234, 0)), [0x1234, 0]);
        let u8c = Class::Int {
            bits: 8,
            signed: false,
        };
        let i8c = Class::Int {
            bits: 8,
            signed: true,
        };
        assert_eq!(guest_ret32(u8c, ans(junk | 0xFF80, 0)), [0x80, 0]);
        assert_eq!(guest_ret32(i8c, ans(junk | 0x80, 0)), [0xFFFF_FF80, 0]);
        assert_eq!(guest_ret32(Class::F32, ans(0, F1_5)), [F1_5, 0]);
        assert_eq!(
            guest_ret32(Class::F64, ans(0, D2_5)),
            [D2_5 & WORD, D2_5 >> 32]
        );
        assert_eq!(guest_ret32(Class::Void, ans(junk, junk)), [0, 0]);
    }

    /// The guest's result read back for the host: r0:r1 joined for a double-word, r0 extended by signedness for a word (r0 = 0xFFFF_FFFE as `int` = -2), a softfp double from r0:r1 into the fp lane.
    #[test]
    fn results_to_the_host() {
        let ans = |x0, v0| Answer { x0, x1: 0, v0 };
        assert_eq!(
            host_ret32(I64, [0x5566_7788, 0x1122_3344]),
            ans(0x1122_3344_5566_7788, 0)
        );
        assert_eq!(
            host_ret32(I32, [0xFFFF_FFFE, 0]),
            ans(0xFFFF_FFFF_FFFF_FFFE, 0)
        );
        assert_eq!(host_ret32(U32, [0xFFFF_FFFE, 0]), ans(0xFFFF_FFFE, 0));
        assert_eq!(
            host_ret32(Class::F64, [D2_5 & WORD, D2_5 >> 32]),
            ans(0, D2_5)
        );
    }

    /// The up call's image (`up_lanes`): `f(int a, long long b, float c)` from AAPCS32 -- a in r0, b in the even pair r2:r3 (C.3), c in the first stack word (C.5 closed the registers at r3, C.6) -- into AAPCS64: a x0 (sign-extended), b x1, c v0. Without a prototype: r0..r3 then the stack words as zero-extended integers, x0..x7 then AAPCS64's stack. aarch64 lanes pass through.
    #[test]
    fn up_lanes_bring_a_32_bit_guest_into_aapcs64() {
        let guest = img(
            &[0xFFFF_FFFF, 0xAAAA, 0x5566_7788, 0x1122_3344],
            &[],
            &[F1_5],
        );
        let a64 = up_lanes(AAPCS32, &guest, Some(&[I32, I64, Class::F32])).expect("fits");
        assert_eq!(a64.regs[..2], [u64::MAX, 0x1122_3344_5566_7788]);
        assert_eq!(a64.fp[0], F1_5);
        let words: Vec<u64> = (1..=12).collect();
        let raw = up_lanes(AAPCS32, &img(&[0x8000_0000, 2, 3, 4], &[], &words), None)
            .expect("sixteen words fit");
        assert_eq!(raw.regs[..8], [0x8000_0000, 2, 3, 4, 1, 2, 3, 4]);
        assert_eq!(raw.stack[..8], [5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(up_lanes(AAPCS64, &guest, None), Some(guest));
    }

    /// The trap's answer to the guest (`guest_answer`): a 32-bit guest's double-word result split into r0 (x0) and r1 (x1), no fp lane; aarch64's untouched.
    #[test]
    fn guest_answer_fills_r1() {
        let a = Answer {
            x0: 0x1122_3344_5566_7788,
            x1: 0,
            v0: 0,
        };
        assert_eq!(
            guest_answer(AAPCS32, I64, a),
            Answer {
                x0: 0x5566_7788,
                x1: 0x1122_3344,
                v0: 0
            }
        );
        assert_eq!(guest_answer(AAPCS64, I64, a), a);
    }

    use super::REFUSED_LOCK;

    /// Every refusal key moves the gate's counter by exactly one per report -- including the handle-table and no-window refusals no pointer check sees (ndk.rs, buffers.rs, window.rs); 4 keys x 1 report each = 4.
    #[test]
    fn every_reported_refusal_moves_the_gate_counter_once() {
        let _serial = REFUSED_LOCK.lock();
        let before = ILP32_REFUSED.load(Ordering::Relaxed);
        for key in [REFUSED_RESULT, REFUSED_ARG, REFUSED_OUT, REFUSED_HANDLE] {
            report_refused(key, "test: handle table full");
        }
        assert_eq!(ILP32_REFUSED.load(Ordering::Relaxed), before + 4);
    }

    /// A host pointer argument of a down call into a 32-bit guest: 2^32 - 1 passes whole, 2^32 (the first a 32-bit word cannot hold) is passed null and counted; an aarch64 guest gets it whole.
    #[test]
    fn ilp32_pointer_arguments_pass_below_4_gib_and_are_refused_above() {
        let _serial = REFUSED_LOCK.lock();
        let mut host = AtlLanes::default();
        host.regs[0] = 0xffff_ffff;
        // 2^32 + 0x10: truncated it would reach the guest as 0x10, another address
        host.regs[1] = 0x1_0000_0010;
        let before = ILP32_REFUSED.load(Ordering::Relaxed);
        let guest = place(&[Class::Ptr, Class::Ptr], &host, HOST, AAPCS32).expect("registers");
        assert_eq!((guest.regs[0], guest.regs[1]), (0xffff_ffff, 0));
        assert_eq!(ILP32_REFUSED.load(Ordering::Relaxed), before + 1);
        let guest = place(&[Class::Ptr, Class::Ptr], &host, HOST, AAPCS64).expect("registers");
        assert_eq!((guest.regs[0], guest.regs[1]), (0xffff_ffff, 0x1_0000_0010));
        assert_eq!(ILP32_REFUSED.load(Ordering::Relaxed), before + 1);
    }

    /// A pointer result into a 32-bit guest: the highest address a 32-bit word holds (2^32 - 1) passes whole; 2^32, the first it cannot, answers null and is counted. An aarch64 guest and a 64-bit integer result are never narrowed here (a `long long` is the r0:r1 pair, `guest_ret32`).
    #[test]
    fn ilp32_pointer_results_pass_below_4_gib_and_are_refused_above() {
        let _serial = REFUSED_LOCK.lock();
        let a = |x0| Answer { x0, x1: 0, v0: 0 };
        let before = ILP32_REFUSED.load(Ordering::Relaxed);
        assert_eq!(
            ilp32_narrow(AAPCS32, Class::Ptr, 0, None, a(0xffff_ffff)).x0,
            0xffff_ffff
        );
        assert_eq!(ILP32_REFUSED.load(Ordering::Relaxed), before);
        assert_eq!(
            ilp32_narrow(AAPCS32, Class::Ptr, 0, None, a(0x1_0000_0000)).x0,
            0
        );
        assert_eq!(ILP32_REFUSED.load(Ordering::Relaxed), before + 1);
        assert_eq!(
            ilp32_narrow(AAPCS64, Class::Ptr, 0, None, a(0x1_0000_0000)).x0,
            0x1_0000_0000
        );
        assert_eq!(
            ilp32_narrow(AAPCS32, I64, 0, None, a(0x1_0000_0000)).x0,
            0x1_0000_0000
        );
    }
}
