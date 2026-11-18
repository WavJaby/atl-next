// SPDX-License-Identifier: MIT
//! The trap body (QEMU bridge plan 2.6(3), Q5-8, Q6-3): the trap type, the callback the QEMU engine installs, the host call a binding or JNI trap makes, and the JNI route. Moved here from `engine/qemu/mod.rs`, so the identity side can share it. The crossing counter (`counters.rs`, feature `crossing-count`) counts each binding crossing here, where the slot is chosen.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use atl_engine_api::AtlLanes;

use super::jni;
use super::objects::{self, Class, Refused, Sig};
use super::{unsupported, va_list};
use crate::engine::Down;
use crate::engine::bindings::OBJECT_STUBS;
use crate::engine::bindings::qemu::THUNKS;

/// A trap from the engine, decoded from `atl_trap_kind`. `Boot` x0 = the auxv QEMU built for the boot executable (x1, its return stub, the engine keeps); `Call` and `Bind` carry their lanes (`atl_lanes`, AAPCS64: x0..x7 in `regs[0..8]`, v0..v7 in `fp[0..8]`, eight stack words); `Report` x0.
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

/// The host function and lanes of each pool slot, read by the binding trap ([`call_binding`]). The QEMU placement sets a slot once (`place::bind_result`), so a slot is bound before any trap reads it.
pub(super) static TARGETS: [OnceLock<(usize, Sig)>; THUNKS] = [const { OnceLock::new() }; THUNKS];

/// The default trap callback: a JNI trap calls the host object's table (`call_object`), a binding trap its host target (`call_binding`), and each answers with x0 and v0 from the host result. Other kinds abort until a route is built; BOOT is the start, answered by `qemu::run` or by the gate's own callback.
pub fn on_trap(trap: Trap) -> Answer {
    match trap {
        Trap::Bind { id, lanes } if (id as usize) < OBJECT_STUBS => call_object(id, &lanes),
        Trap::Bind { id, lanes } => call_binding(id, &lanes),
        other => unsupported(&format!("qemu crossing: {other:?} is not routed yet")),
    }
}

/// Added to a trap's id before its slot is read: 0 in every build; the `qe_id` break (`ATL_NB_BREAK`) makes it 1, so each binding calls its neighbour's host function.
#[cfg(not(atl_nb_qe_id))]
const ID_SKEW: usize = 0;
#[cfg(atl_nb_qe_id)]
const ID_SKEW: usize = 1;

/// Calls the host function of the slot `id + ID_SKEW` with the guest's lanes ([`call_host`], placed and answered by the slot's lanes). Every argument lane is forwarded, fp included. The counter sees the slot that ran, so a skewed dispatch shows up as a wrong count.
pub(super) fn call_binding(id: u32, lanes: &AtlLanes) -> Answer {
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
    call_host(target, lanes, sig)
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
/// Deliberate breaks: `jni_swap` exchanges x3 and x4 of CallStaticIntMethod only; `jni_guest_env` passes the guest's env through unmapped.
fn call_object(id: u32, lanes: &AtlLanes) -> Answer {
    let guest = lanes.regs[0] as usize;
    // SAFETY: x0 is the object pointer the guest called through.
    let (table, lane) =
        unsafe { objects::lane_of(guest, id as usize) }.unwrap_or_else(|r| refuse(r));
    // SAFETY: `guest` is a guest env or vm: `lane_of` found its table, which only `guest_env` / `guest_vm` objects carry.
    let host = unsafe { objects::host_of(guest) };
    let mut lanes = *lanes;
    // a `...V` call (Q6-7): its va_list read per the method's shorty into a jvalue array, which the `...A` sibling takes in the va_list's lane
    let args: Vec<u64>;
    let slot = match lane.params.iter().position(|c| *c == Class::VaList) {
        None => id as usize,
        Some(k) => {
            let name = lane.name;
            // SAFETY: `host` is the host env behind the guest env; lane k - 1 is the call's jmethodID (jni.h: every V slot's va_list follows its methodID).
            let params = unsafe { jni::method_params(host, lanes.regs[k - 1] as usize) }
                .unwrap_or_else(|| refuse(Refused::NoShorty { name }));
            // SAFETY: lane k is the guest's va_list pointer, and the guest made the call with the method's arguments (JNI contract).
            args = unsafe { va_list::jvalues(lanes.regs[k] as usize, params) }
                .unwrap_or_else(|c| refuse(Refused::Shorty { name, c }));
            lanes.regs[k] = args.as_ptr() as u64;
            jni::a_slot(id as usize)
        }
    };
    // SAFETY: `slot` is `id` or its A sibling, below the length of the table `lane_of` read, which jni.rs builds from the host object's table, so it is a slot of `host`'s own table.
    let target = unsafe { objects::host_slot(host, slot) };
    // counted as the guest's slot `id`, with the host function that ran
    #[cfg(all(feature = "crossing-count", not(atl_nb_qe_jni_count)))]
    count_object(table, id as usize, target);
    #[cfg(not(all(feature = "crossing-count", not(atl_nb_qe_jni_count))))]
    let _ = table;
    #[cfg(atl_nb_jni_swap)]
    if id as usize == jni::CALL_STATIC_INT_METHOD {
        lanes.regs.swap(3, 4);
    }
    #[cfg(atl_nb_jni_guest_env)]
    let arg0 = guest as u64;
    #[cfg(not(atl_nb_jni_guest_env))]
    let arg0 = host as u64;
    lanes.regs[0] = arg0;
    // the fixed parameters place the call (a `...V` slot's va_list lane now holds the jvalue array, an int lane: `place`); a variadic slot's `...` follows them unclassed, so it crosses as the guest set it
    let sig = Sig {
        ret: lane.ret,
        params: (!lane.variadic).then_some(lane.params),
    };
    call_host(target, &lanes, sig)
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

/// Calls `target` with the guest lanes `lanes` and answers its result by `sig.ret`: a double from the fp result register (its bits in v0), a float as its low 32 bits with the upper ones clear (AArch64 reads s0; LP64D's fa0 holds it NaN-boxed), a void as x0 = 0, anything else from x0.
/// Breaks: `v_drop` zeroes the fp lanes; `drop_stack` moves the stack words down one; `ret_int` reads an fp result from x0; per host, `al_zero` (variadic.rs) and `up_no_nanbox` (riscv64.rs).
fn call_host(target: usize, lanes: &AtlLanes, sig: Sig) -> Answer {
    #[allow(unused_mut, reason = "only the breaks below change the lanes")]
    let mut lanes = *lanes;
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
    match ret {
        Class::F32 => Answer {
            x0: 0,
            v0: v & 0xFFFF_FFFF,
        },
        Class::F64 => Answer { x0: 0, v0: v },
        Class::Void => Answer { x0: 0, v0: 0 },
        _ => Answer { x0: x, v0: 0 },
    }
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
#[cfg_attr(
    not(target_arch = "riscv64"),
    allow(dead_code, reason = "only the placing host call refuses")
)]
fn refuse_up(target: usize) -> ! {
    static REFUSED: AtomicUsize = AtomicUsize::new(0);
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused up call to {target:#x}: its arguments need more than eight stack words on one side, the lane image holds eight (R-1, refusals: {n})"
    ))
}

/// Refusals by kind, in [`refuse`]'s order: a forged id, a slot with no function, a `va_list` without a shorty source, a shorty character outside jni's, an x0 that is no proxy object. A refusal aborts, so each count reaches the abort message only.
static REFUSED: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];

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
    };
    let n = REFUSED[kind].fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused {why} (refusals of this kind: {n})"
    ))
}

/// Host->guest entry (Q6-4, bridgegen down.rs): the shell of down slot `slot` calls it with its frame's lanes.
///
/// # Safety
/// `host` is the shell's frame, an `AtlLanes` live for the call and not otherwise accessed until it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_nb_down_dispatch(slot: usize, host: *mut AtlLanes) {
    // SAFETY: the shell's frame, per the contract above.
    down_body(slot, unsafe { &mut *host });
}

/// The host->guest body of down slot `slot` (Q6-4): `host` is the shell's frame with the host call's lanes; the guest call's lanes are placed by the slot's classes (`place`), and the guest's answer goes back in `host.ret[0]` and `host.fp[0]`, which the shell returns.
fn down_body(slot: usize, host: &mut AtlLanes) {
    let Some(d) = super::qemu::down_slot(slot) else {
        unsupported("qemu crossing: down shell without a binding")
    };
    let Some(mut guest) = place(&d.params, host, HOST, AAPCS64) else {
        refuse_down(d.target)
    };
    // the first argument of a JNI method, of JNI_OnLoad or of an SL callback is a host env, vm or interface: the guest's own object stands for it (jni.rs, sles.rs), and it is the first integer, in x0; an slObjectCallback's 6th is the 6th integer, in x5
    #[cfg(not(atl_nb_down_noenv))]
    {
        let (mut first, mut sixth) = (guest.regs[0] as usize, guest.regs[5] as usize);
        d.kind.guest_args(&mut first, &mut sixth);
        (guest.regs[0], guest.regs[5]) = (first as u64, sixth as u64);
    }
    // break `down_noenv` (qemu-jni gate, checks o, m and x): the kind is not read, so the guest gets the host's env or vm
    #[cfg(atl_nb_down_noenv)]
    let _ = d.kind;
    super::qemu::call_down(d.target, &mut guest);
    (host.ret[0], host.fp[0]) = host_result(d.ret, guest.ret[0], guest.fp[0], HOST);
}

// The shells' frame is an `atl_lanes`: bridgegen's offsets (down.rs, written into the shells' asm) must be the struct's.
const _: () = {
    use super::qemu::down_pool as p;
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

/// The guest's rules, AAPCS64: eight integer registers, an fp argument past v7 goes to the stack, results and floats unshaped (a narrow value's upper bits are unspecified, s-registers are read low); eight stack words (the aarch64 engine's window, atl-qemu-engine engine/aarch64.rs `STACK_SLOTS`).
const AAPCS64: HostRules = HostRules {
    int_lanes: 8,
    fp_lanes: 8,
    fp_to_int: false,
    extend_int_ret: false,
    nan_box_f32: false,
    stack_words: 8,
    word_bits: 64,
};

/// The arm guest's rules, AAPCS32 base standard (Android armeabi-v7a is softfp: no `Tag_ABI_VFP_args`, the P2 guest ABI gate): r0..r3, no fp argument register, so a float or double travels in core registers or on the stack as an integer of its size would; 4-byte words; the arm engine's whole 16-word window (atl-qemu-engine engine/arm.rs `STACK_SLOTS`). Rule numbers: AAPCS32 2026Q3, Parameter Passing (base PCS). The arm bridge core places by it (phase4-32bit P3 slice 3); until then the unit tests and the lane assert below read it.
const AAPCS32: HostRules = HostRules {
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
    use super::qemu::down_pool as p;
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
        (Down::Plain, Some(s)) => (&[], s),
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
    // break `shorty_swap` (qemu-jni gate, checks m, x and R): the first shorty parameter's class is flipped, so the host's argument reads the other bank; the 15-integer R-1 call then fits in eight stack words and is not refused
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

/// A value of class `class` read from a side of `from` bits, as a side of `to` bits holds it. Between sides of one width it is unchanged (a 64-bit value on a 32-bit side was joined by [`read`]). Into 64 bits from 32: an integer is extended by its signedness (B.2 left a narrower one extended to the word; a 64-bit host's `long` parameter reads all 64 bits), a pointer zero-extended, a float's bits stay low. Into 32 bits from 64: the low word (SysV and LP64D leave the bits above a 32-bit argument to the callee; a host pointer at or above 4 GiB is the window's refusal, phase4-32bit P4b).
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
/// Every ABI here takes integers and fp arguments by class in declaration order ([`Assign::next`]: SysV 6 integer registers, LP64D and AAPCS64 8, AAPCS32 4; eight fp registers, none on AAPCS32's base standard). A value crossing between word widths is [`convert`]ed. A float going into an fp register of a `nan_box_f32` side is NaN-boxed (RISC-V: upper 32 bits ones; AArch64 leaves them zero). `None` when either side needs more stack words than it carries (R-1): with fp-to-int on one side, the other can need more stack words.
fn place(
    params: &[Class],
    from: &AtlLanes,
    from_rules: HostRules,
    to_rules: HostRules,
) -> Option<AtlLanes> {
    let mut to = AtlLanes::default();
    let (mut src, mut dst) = (Assign::new(from_rules), Assign::new(to_rules));
    for &class in params {
        let value = match src.next(class)? {
            Slot::X(i, n) => read(&from.regs, i, n),
            Slot::V(i) => from.fp[i],
            Slot::Stack(i, n) => read(&from.stack, i, n),
        };
        let value = convert(class, value, from_rules.word_bits, to_rules.word_bits);
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the arm bridge core answers by it (phase4-32bit P3 slice 3)"
    )
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the arm bridge core answers by it (phase4-32bit P3 slice 3)"
    )
)]
fn host_ret32(ret: Class, r: [u64; 2]) -> Answer {
    let joined = (r[0] & WORD) | (r[1] << 32);
    match ret {
        Class::Void => Answer::default(),
        Class::F64 => Answer { x0: 0, v0: joined },
        Class::F32 => Answer {
            x0: 0,
            v0: r[0] & WORD,
        },
        Class::Int { bits: 64, .. } => Answer { x0: joined, v0: 0 },
        _ => Answer {
            x0: convert(ret, r[0], 32, 64),
            v0: 0,
        },
    }
}

/// Refuses a down call whose host arguments need more than eight stack words (R-1, Q6-4): the lane image cannot carry them. Named and counted: a shorty this size fails on every call, so the count says how often it was asked.
fn refuse_down(target: usize) -> ! {
    static REFUSED: AtomicUsize = AtomicUsize::new(0);
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused down call to {target:#x}: its arguments need more than eight stack words, the lane image holds eight (R-1, refusals: {n})"
    ))
}

#[cfg(test)]
mod place_tests {
    use super::*;

    /// x86_64 SysV (bridgegen x86.rs `DOWN`): six integer registers, fp overflow to the stack, results unshaped.
    const SYSV: HostRules = HostRules {
        int_lanes: 6,
        fp_lanes: 8,
        fp_to_int: false,
        extend_int_ret: false,
        nan_box_f32: false,
        stack_words: 8,
        word_bits: 64,
    };
    /// riscv64 LP64D (bridgegen riscv64.rs `DOWN`): eight integer registers, fp overflow to a free integer register, results shaped.
    const LP64D: HostRules = HostRules {
        int_lanes: 8,
        fp_lanes: 8,
        fp_to_int: true,
        extend_int_ret: true,
        nan_box_f32: true,
        stack_words: 8,
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

    /// LP64D, seventeen doubles: the host holds them all in registers (8 fp + 8 int) plus one stack word; AAPCS64 needs nine stack words (8 in v) -> refused, never an out-of-bounds write.
    #[test]
    fn lp64d_guest_stack_overflow_is_refused() {
        assert!(place(&[Class::F64; 16], &AtlLanes::default(), LP64D, AAPCS64).is_some());
        assert!(place(&[Class::F64; 17], &AtlLanes::default(), LP64D, AAPCS64).is_none());
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

    /// Fourteen integers need eight stack words on SysV (six registers): the image holds them. Fifteen need nine: refused (R-1).
    #[test]
    fn nine_host_stack_words_are_refused() {
        assert!(place(&[INT; 14], &AtlLanes::default(), SYSV, AAPCS64).is_some());
        assert!(place(&[INT; 15], &AtlLanes::default(), SYSV, AAPCS64).is_none());
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
        stack_words: 8,
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
        let ans = |x0, v0| Answer { x0, v0 };
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
        assert_eq!(
            host_ret32(I64, [0x5566_7788, 0x1122_3344]),
            Answer {
                x0: 0x1122_3344_5566_7788,
                v0: 0
            }
        );
        assert_eq!(
            host_ret32(I32, [0xFFFF_FFFE, 0]),
            Answer {
                x0: 0xFFFF_FFFF_FFFF_FFFE,
                v0: 0
            }
        );
        assert_eq!(
            host_ret32(U32, [0xFFFF_FFFE, 0]),
            Answer {
                x0: 0xFFFF_FFFE,
                v0: 0
            }
        );
        assert_eq!(
            host_ret32(Class::F64, [D2_5 & WORD, D2_5 >> 32]),
            Answer { x0: 0, v0: D2_5 }
        );
    }
}
