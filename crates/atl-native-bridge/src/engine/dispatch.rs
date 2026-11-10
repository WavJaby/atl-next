// SPDX-License-Identifier: MIT
//! The trap body (QEMU bridge plan 2.6(3), Q5-8, Q6-3): the trap type, the callback the QEMU engine installs, the host call a binding or JNI trap makes, and the JNI route. Moved here from `engine/qemu/mod.rs`, so the identity side can share it. The crossing counter (`counters.rs`, feature `crossing-count`) counts each binding crossing here, where the slot is chosen.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use atl_engine_api::AtlLanes;

use super::jni::{self, Class, Refused};
use super::unsupported;
use crate::engine::bindings::JNI_STUBS;
use crate::engine::bindings::qemu::THUNKS;
use crate::engine::identity::Down;

/// A trap from the engine, decoded from `atl_trap_kind`. `Boot` x0 = the auxv QEMU built for the boot executable (x1, its return stub, the engine keeps); `Call` and `Bind` carry their lanes (`atl_lanes`: x0..x7, v0..v7, eight stack words); `Report` x0.
#[derive(Clone, Copy, Debug)]
pub enum Trap {
    /// BOOT: the boot executable's auxv (x0).
    Boot(u64),
    /// `svc #CALL`: the guest's lanes.
    Call(AtlLanes),
    /// A pool thunk: its id (a JNI slot below `JNI_STUBS`, else a bound host function), and its lanes.
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

/// The host function and result class of each pool slot, read by the binding trap ([`call_binding`]). The QEMU placement sets a slot once (`place::bind_result`), so a slot is bound before any trap reads it.
pub(super) static TARGETS: [OnceLock<(usize, Class)>; THUNKS] = [const { OnceLock::new() }; THUNKS];

/// The default trap callback: a JNI trap calls the host object's table (`call_jni`), a binding trap its host target (`call_binding`), and each answers with x0 and v0 from the host result. Other kinds abort until a route is built; BOOT is the start, answered by `qemu::run` or by the gate's own callback.
pub fn on_trap(trap: Trap) -> Answer {
    match trap {
        Trap::Bind { id, lanes } if (id as usize) < JNI_STUBS => call_jni(id, &lanes),
        Trap::Bind { id, lanes } => call_binding(id, &lanes),
        other => unsupported(&format!("qemu crossing: {other:?} is not routed yet")),
    }
}

/// Added to a trap's id before its slot is read: 0 in every build; the `qe_id` break (`ATL_NB_BREAK`) makes it 1, so each binding calls its neighbour's host function.
#[cfg(not(atl_nb_qe_id))]
const ID_SKEW: usize = 0;
#[cfg(atl_nb_qe_id)]
const ID_SKEW: usize = 1;

/// Calls the host function of the slot `id + ID_SKEW` with the guest's lanes ([`call_host`], answered by the slot's result class). Every argument lane is forwarded, fp included. The counter sees the slot that ran, so a skewed dispatch shows up as a wrong count.
pub(super) fn call_binding(id: u32, lanes: &AtlLanes) -> Answer {
    let Some(slot) = usize::try_from(id)
        .ok()
        .map(|i| i + ID_SKEW)
        .filter(|&s| s < THUNKS)
    else {
        unsupported("qemu crossing: binding id past the pool")
    };
    let Some(&(target, ret)) = TARGETS[slot].get() else {
        unsupported("qemu crossing: binding trap for an unbound slot")
    };
    #[cfg(feature = "crossing-count")]
    crate::counters::count_binding(slot, target);
    call_host(target, lanes, ret)
}

/// A guest JNI call (pool id `id` < `JNI_STUBS`, Q6-2): x0 is the guest's JNIEnv or JavaVM, and the call runs on the host object it stands for. That object's own table slot `id` runs with x0 replaced by the host object and the other lanes forwarded; its result is the answer. The descriptor (`jni::lane_of`, from the guest object's table) gives the result class; a refused call aborts by name ([`refuse`]).
/// Deliberate breaks: `jni_swap` exchanges x3 and x4 of CallStaticIntMethod only; `jni_guest_env` passes the guest's env through unmapped.
fn call_jni(id: u32, lanes: &AtlLanes) -> Answer {
    let guest = lanes.x[0] as usize;
    let lane = jni::lane_of(guest, id as usize).unwrap_or_else(|r| refuse(r));
    // SAFETY: `guest` is a guest env or vm: `lane_of` found its table, which only `guest_env` / `guest_vm` objects carry.
    let host = unsafe { jni::host_of(guest) };
    // SAFETY: `id` is below the length of the table `lane_of` read, which jni.rs builds from the host object's table, so it is a slot of `host`'s own table.
    let target = unsafe { jni::host_slot(host, id as usize) };
    let mut lanes = *lanes;
    #[cfg(atl_nb_jni_swap)]
    if id as usize == jni::CALL_STATIC_INT_METHOD {
        lanes.x.swap(3, 4);
    }
    #[cfg(atl_nb_jni_guest_env)]
    let arg0 = guest as u64;
    #[cfg(not(atl_nb_jni_guest_env))]
    let arg0 = host as u64;
    lanes.x[0] = arg0;
    call_host(target, &lanes, lane.ret)
}

/// The host call's image, SysV x86_64: integer words x0..x7 then the eight stack words (SysV takes the sixth and later integer words from the stack, in order), and fp lanes v0..v7 in xmm0..7. Both ABIs assign integer and fp arguments to registers by class in declaration order, so the guest's lanes are the callee's register image as they stand; the descriptor's parameter classes need no walk. Eight fp words always pass, so the call sets %al = 8: a variadic callee (glibc printf) saves its xmm arguments only when %al is nonzero. 8 is the ABI's upper bound on the vector registers used; the descriptor cannot count a variadic call's fp arguments, so the exact count is not built.
/// Not represented: fp arguments past eight, or an integer and an fp argument both overflowing the registers (R-1). x86_64 host only (R-3).
#[cfg(not(atl_nb_al_zero))]
type HostWords = unsafe extern "C" fn(u64, ...) -> u64;
/// The same image, for a result in xmm0.
#[cfg(not(atl_nb_al_zero))]
type HostFp = unsafe extern "C" fn(u64, ...) -> f64;

/// Calls `target` with the guest lanes `lanes` and answers its result by `ret`: a double or float from xmm0 (its bits in v0), a void as x0 = 0, anything else from x0.
/// Breaks: `v_drop` zeroes the fp lanes; `drop_stack` moves the stack words down one; `ret_int` reads an fp result from x0; `al_zero` makes the call with %al = 0 ([`call_al0`]).
fn call_host(target: usize, lanes: &AtlLanes, ret: Class) -> Answer {
    let [x0, x1, x2, x3, x4, x5, x6, x7] = lanes.x;
    #[cfg(not(atl_nb_v_drop))]
    let [v0, v1, v2, v3, v4, v5, v6, v7] = lanes.v.map(f64::from_bits);
    #[cfg(atl_nb_v_drop)]
    let [v0, v1, v2, v3, v4, v5, v6, v7] = [0.0; 8];
    #[cfg(not(atl_nb_drop_stack))]
    let stack = lanes.stack;
    #[cfg(atl_nb_drop_stack)]
    let stack = {
        let mut s = lanes.stack;
        s.copy_within(1.., 0);
        s[7] = 0;
        s
    };
    #[cfg(not(atl_nb_al_zero))]
    let [s0, s1, s2, s3, s4, s5, s6, s7] = stack;
    match fp_result(ret) {
        Class::Fp => {
            #[cfg(not(atl_nb_al_zero))]
            let r = {
                // SAFETY: `target` is the host C function a binding or JNI entry names, and its result is a double or float (the descriptor's or the table's `ret`); the call passes the guest words in the image above, which such a callee reads by its own prototype.
                let f = unsafe { std::mem::transmute::<usize, HostFp>(target) };
                // SAFETY: as above; a callee with fewer parameters ignores the rest.
                unsafe {
                    f(
                        x0, x1, x2, x3, x4, x5, x6, x7, v0, v1, v2, v3, v4, v5, v6, v7, s0, s1, s2,
                        s3, s4, s5, s6, s7,
                    )
                }
            };
            #[cfg(atl_nb_al_zero)]
            let r = call_al0(
                target,
                [x0, x1, x2, x3, x4, x5, x6, x7],
                [v0, v1, v2, v3, v4, v5, v6, v7],
                stack,
            )
            .1;
            Answer {
                x0: 0,
                v0: r.to_bits(),
            }
        }
        _ => {
            #[cfg(not(atl_nb_al_zero))]
            let r = {
                // SAFETY: `target` is a C-ABI host function of this trap (see above); the same image as the fp arm, with an integer or pointer result.
                let f = unsafe { std::mem::transmute::<usize, HostWords>(target) };
                // SAFETY: as above; a callee with fewer parameters ignores the rest.
                unsafe {
                    f(
                        x0, x1, x2, x3, x4, x5, x6, x7, v0, v1, v2, v3, v4, v5, v6, v7, s0, s1, s2,
                        s3, s4, s5, s6, s7,
                    )
                }
            };
            #[cfg(atl_nb_al_zero)]
            let r = call_al0(
                target,
                [x0, x1, x2, x3, x4, x5, x6, x7],
                [v0, v1, v2, v3, v4, v5, v6, v7],
                stack,
            )
            .0;
            Answer {
                x0: if ret == Class::Void { 0 } else { r },
                v0: 0,
            }
        }
    }
}

/// Break `al_zero` (`ATL_NB_BREAK=al_zero`): [`call_host`]'s image, called from asm with %al = 0 where the normal call sets 8. A variadic callee saves its xmm arguments only when %al is nonzero (SysV), so a double vararg reads garbage. Returns rax and xmm0. The ten stack words (x6, x7, then s0..s7) are pushed so the callee finds the first at the lowest address.
#[cfg(atl_nb_al_zero)]
fn call_al0(target: usize, x: [u64; 8], v: [f64; 8], s: [u64; 8]) -> (u64, f64) {
    let stack = [x[6], x[7], s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]];
    let p = stack.as_ptr();
    let (int, fp): (u64, f64);
    // SAFETY: `target` is a C host function of this trap, called with `call_host`'s image: the register words in their registers, the stack words pushed in reverse (ten pushes keep rsp 16-byte aligned at the call), rsp restored after the call. `stack` outlives the asm block. %al is zero, the one register this break changes.
    unsafe {
        std::arch::asm!(
            "push qword ptr [{p} + 72]",
            "push qword ptr [{p} + 64]",
            "push qword ptr [{p} + 56]",
            "push qword ptr [{p} + 48]",
            "push qword ptr [{p} + 40]",
            "push qword ptr [{p} + 32]",
            "push qword ptr [{p} + 24]",
            "push qword ptr [{p} + 16]",
            "push qword ptr [{p} + 8]",
            "push qword ptr [{p}]",
            "call {f}",
            "add rsp, 80",
            p = in(reg) p,
            f = in(reg) target,
            in("rdi") x[0],
            in("rsi") x[1],
            in("rdx") x[2],
            in("rcx") x[3],
            in("r8") x[4],
            in("r9") x[5],
            inout("xmm0") v[0] => fp,
            in("xmm1") v[1],
            in("xmm2") v[2],
            in("xmm3") v[3],
            in("xmm4") v[4],
            in("xmm5") v[5],
            in("xmm6") v[6],
            in("xmm7") v[7],
            inout("rax") 0_u64 => int,
            clobber_abi("C"),
        );
    }
    (int, fp)
}

/// The result class the call is made with: an fp result in x0 is the break `ret_int`.
#[cfg(not(atl_nb_ret_int))]
fn fp_result(ret: Class) -> Class {
    ret
}
/// Break `ret_int`: an fp result is read from x0, as an integer's is.
#[cfg(atl_nb_ret_int)]
fn fp_result(_ret: Class) -> Class {
    Class::Int
}

/// Refusals by kind, in [`refuse`]'s order: a forged id, a reserved slot, a `va_list`. A refusal aborts, so each count reaches the abort message only.
static REFUSED: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

/// The named abort of a refused guest JNI call (owner decision D-4): the message says which call and why, with the count of that kind.
fn refuse(r: Refused) -> ! {
    let (kind, why) = match r {
        Refused::Forged { id, table, len } => (
            0,
            format!("JNI id {id} past the {table} table ({len} entries)"),
        ),
        Refused::Reserved { id } => (
            1,
            format!("JNI slot {id} is reserved (jni.h reserved*), no function"),
        ),
        Refused::VaList { name } => (2, format!("JNI {name} takes a va_list (a V variant, D-4)")),
    };
    let n = REFUSED[kind].fetch_add(1, Ordering::Relaxed) + 1;
    unsupported(&format!(
        "qemu crossing: refused {why} (refusals of this kind: {n})"
    ))
}

/// Host->guest entry (Q6-4, x86.rs `down_entries_asm`): the shell of down slot `slot` calls it with its frame's lanes.
///
/// # Safety
/// `host` is the shell's frame, an `AtlLanes` live for the call and not otherwise accessed until it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_nb_down_dispatch(slot: usize, host: *mut AtlLanes) {
    // SAFETY: the shell's frame, per the contract above.
    down_body(slot, unsafe { &mut *host });
}

/// The host->guest body of down slot `slot` (Q6-4): `host` is the shell's frame with the host call's lanes; the guest call's lanes are placed by the slot's classes (`place`), and the guest's answer goes back in `host.x[0]` and `host.v[0]`.
fn down_body(slot: usize, host: &mut AtlLanes) {
    let Some(d) = super::qemu::down_slot(slot) else {
        unsupported("qemu crossing: down shell without a binding")
    };
    let Some(mut guest) = place(&d.params, host) else {
        refuse_down(d.target)
    };
    // the first argument of a JNI method, or of JNI_OnLoad, is the host's env or vm: the guest's own object stands for it (jni.rs), and it is the first integer, in x0
    match d.kind {
        Down::Env => guest.x[0] = jni::guest_env(guest.x[0] as usize) as u64,
        Down::Vm => guest.x[0] = jni::guest_vm(guest.x[0] as usize) as u64,
        Down::Plain => {}
    }
    super::qemu::call_down(d.target, &mut guest);
    host.x[0] = guest.x[0];
    host.v[0] = guest.v[0];
}

/// The lane classes of a down call's arguments (Q6-4). A JNI method's env and class (or `this`) come first, a `@CriticalNative` method's none; JNI_OnLoad and JNI_OnUnload, which ART calls with no shorty, take `(JavaVM*, void*)`. The shorty is `ArtMethod::GetShorty`: its first byte is the result, each further byte one parameter.
pub(in crate::engine) fn down_params(
    kind: Down,
    shorty: Option<&[u8]>,
) -> Result<Vec<Class>, String> {
    let (implicit, s): (&[Class], &[u8]) = match (kind, shorty) {
        (Down::Vm, None) => return Ok(vec![Class::Int, Class::Int]),
        (Down::Env, Some(s)) => (&[Class::Int, Class::Int], s),
        (Down::Plain, Some(s)) => (&[], s),
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
        params.push(match c {
            b'F' | b'D' => Class::Fp,
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'L' => Class::Int,
            other => {
                return Err(format!(
                    "shorty parameter {:?}: no such type",
                    other as char
                ));
            }
        });
    }
    Ok(params)
}

/// The guest call's lanes for a host call whose arguments have the classes `params` (Q6-4). Both ABIs take an integer from the integer registers (six on SysV, eight on AAPCS64) and a float or double from the fp registers (eight each), in declaration order; the rest go to the stack, one word each, in declaration order. Each side reads its words from its own image: `host` is the SysV image (x[0..6], v[0..8], the eight stack words), the result the AAPCS64 one. `None` when the host call needs more than eight stack words (R-1). The guest side never overflows the image: AAPCS64 has at least as many registers per class, so its stack words are a subset of the host's.
fn place(params: &[Class], host: &AtlLanes) -> Option<AtlLanes> {
    let mut guest = AtlLanes::default();
    let (mut host_x, mut host_v, mut host_stack) = (0, 0, 0);
    let (mut guest_x, mut guest_v, mut guest_stack) = (0, 0, 0);
    for &class in params {
        let word = match class {
            Class::Int if host_x < 6 => {
                host_x += 1;
                host.x[host_x - 1]
            }
            Class::Fp if host_v < 8 => {
                host_v += 1;
                host.v[host_v - 1]
            }
            _ => {
                let word = *host.stack.get(host_stack)?;
                host_stack += 1;
                word
            }
        };
        match class {
            Class::Int if guest_x < 8 => {
                guest.x[guest_x] = word;
                guest_x += 1;
            }
            Class::Fp if guest_v < 8 => {
                guest.v[guest_v] = word;
                guest_v += 1;
            }
            _ => {
                guest.stack[guest_stack] = word;
                guest_stack += 1;
            }
        }
    }
    Some(guest)
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
mod down_tests {
    use super::*;

    /// Nine integers: SysV takes six in %rdi..%r9 and three from the stack; AAPCS64 takes eight in x0..x7 and one from its stack. Expected values follow from those two rules.
    #[test]
    fn nine_integers_split_across_both_banks() {
        let host = AtlLanes {
            x: [10, 11, 12, 13, 14, 15, 0, 0],
            stack: [100, 101, 102, 0, 0, 0, 0, 0],
            ..Default::default()
        };
        let guest = place(&[Class::Int; 9], &host).expect("three stack words fit");
        assert_eq!(guest.x, [10, 11, 12, 13, 14, 15, 100, 101]);
        assert_eq!(guest.stack[0], 102);
    }

    /// Eight doubles fill %xmm0..7 on both sides; a ninth double is the first stack word on both; an integer after them is %rdi on SysV and x0 on AAPCS64.
    #[test]
    fn ninth_double_is_the_first_stack_word() {
        let host = AtlLanes {
            x: [77, 0, 0, 0, 0, 0, 0, 0],
            v: [1, 2, 3, 4, 5, 6, 7, 8],
            stack: [9, 0, 0, 0, 0, 0, 0, 0],
        };
        let mut params = vec![Class::Fp; 9];
        params.push(Class::Int);
        let guest = place(&params, &host).expect("one stack word");
        assert_eq!(
            (guest.v, guest.stack[0], guest.x[0]),
            ([1, 2, 3, 4, 5, 6, 7, 8], 9, 77)
        );
    }

    /// The seventh integer is a stack word on SysV but a register on AAPCS64: the stack word crosses into x6, and the fp argument after it is xmm0 on SysV and v0 on AAPCS64.
    #[test]
    fn a_stack_integer_lands_in_a_guest_register() {
        let host = AtlLanes {
            x: [1, 2, 3, 4, 5, 6, 0, 0],
            v: [8, 0, 0, 0, 0, 0, 0, 0],
            stack: [7, 0, 0, 0, 0, 0, 0, 0],
        };
        let mut params = vec![Class::Int; 7];
        params.push(Class::Fp);
        let guest = place(&params, &host).expect("one stack word");
        assert_eq!(guest.x, [1, 2, 3, 4, 5, 6, 7, 0]);
        assert_eq!((guest.v[0], guest.stack), (8, [0; 8]));
    }

    /// Fourteen integers need eight stack words on SysV (six registers): the image holds them. Fifteen need nine: refused (R-1).
    #[test]
    fn nine_host_stack_words_are_refused() {
        assert!(place(&[Class::Int; 14], &AtlLanes::default()).is_some());
        assert!(place(&[Class::Int; 15], &AtlLanes::default()).is_none());
    }

    /// The shorty `DIDJ` (result double; int, double, long) under a JNI method: env and class first, then the three. JNI_OnLoad (no shorty) is `(JavaVM*, void*)`; a shorty without a method's env is refused.
    #[test]
    fn shorty_gives_the_classes() {
        assert_eq!(
            down_params(Down::Env, Some(&b"DIDJ"[..])),
            Ok(vec![
                Class::Int,
                Class::Int,
                Class::Int,
                Class::Fp,
                Class::Int
            ])
        );
        assert_eq!(
            down_params(Down::Plain, Some(&b"VF"[..])),
            Ok(vec![Class::Fp])
        );
        assert_eq!(
            down_params(Down::Vm, None),
            Ok(vec![Class::Int, Class::Int])
        );
        assert!(down_params(Down::Env, None).is_err());
        assert!(down_params(Down::Vm, Some(&b"V"[..])).is_err());
        assert!(down_params(Down::Env, Some(&b"QI"[..])).is_err());
    }
}
