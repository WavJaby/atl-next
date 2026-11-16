// SPDX-License-Identifier: MIT
//! The up call on a riscv64 host (LP64D): the guest's AAPCS64 lanes are placed into the LP64D image by the prototype's classes ([`super::place`], AAPCS64 -> host rules: fp overflow into free integer registers, a float NaN-boxed in an fp register), then called from asm.
//! A variadic call cannot be the variadic image here: RISC-V passes variadic fp arguments in integer registers, so a callee's fa0.. would be garbage (glClearColor's four floats, measured on sifive).
//! Without a prototype (`Sig::params` None) the lanes cross as they stand: a0..a7 = x0..x7, fa0..fa7 = v0..v7, the stack words in order -- right for up to 16 integers and up to 8 doubles, not for an unboxed float.

use atl_engine_api::AtlLanes;

use super::{AAPCS64, HOST, HostRules, place, refuse_up};
use crate::engine::objects::Sig;

/// The host rules the up direction places into: the build's, except under break `up_no_nanbox`, which leaves a float unboxed (the callee then reads canonical NaN: the app gate's glesPbuffer reads 1024).
#[cfg(not(atl_nb_up_no_nanbox))]
const UP: HostRules = HOST;
#[cfg(atl_nb_up_no_nanbox)]
const UP: HostRules = HostRules {
    nan_box_f32: false,
    ..HOST
};

/// Calls `target` with the guest lanes `lanes` placed by `sig`. Answers (a0, fa0 bits): the asm reads both, the caller keeps the one `sig.ret` names.
pub(super) fn call(target: usize, lanes: &AtlLanes, _fp: bool, sig: Sig) -> (u64, u64) {
    let image = match sig.params {
        Some(params) => place(params, lanes, AAPCS64, UP).unwrap_or_else(|| refuse_up(target)),
        None => *lanes,
    };
    lp64d(target, &image)
}

/// The LP64D call of `target` with `image`: a0..a7 from x, fa0..fa7 from v (`fld`), the eight stack words stored at sp (64 bytes keep sp 16-aligned, psABI), `jalr`; sp restored after. Returns a0 and fa0's bits.
fn lp64d(target: usize, image: &AtlLanes) -> (u64, u64) {
    let (int, fp): (u64, f64);
    // SAFETY: `target` is the host C function a binding or JNI entry names; its arguments are the image `place` built by its prototype (or the identity image a prototype-less caller relies on). The stack words go below sp, which asm! hands over call-aligned; sp is restored after the call. t1 / t2 point at `image`'s fields, which outlive the block; every caller-saved register is declared clobbered (clobber_abi), ra too.
    unsafe {
        std::arch::asm!(
            "addi sp, sp, -64",
            "ld t0, 0(t2)",
            "sd t0, 0(sp)",
            "ld t0, 8(t2)",
            "sd t0, 8(sp)",
            "ld t0, 16(t2)",
            "sd t0, 16(sp)",
            "ld t0, 24(t2)",
            "sd t0, 24(sp)",
            "ld t0, 32(t2)",
            "sd t0, 32(sp)",
            "ld t0, 40(t2)",
            "sd t0, 40(sp)",
            "ld t0, 48(t2)",
            "sd t0, 48(sp)",
            "ld t0, 56(t2)",
            "sd t0, 56(sp)",
            "fld fa0, 0(t1)",
            "fld fa1, 8(t1)",
            "fld fa2, 16(t1)",
            "fld fa3, 24(t1)",
            "fld fa4, 32(t1)",
            "fld fa5, 40(t1)",
            "fld fa6, 48(t1)",
            "fld fa7, 56(t1)",
            "jalr ra, 0(t3)",
            "addi sp, sp, 64",
            in("t1") image.v.as_ptr(),
            in("t2") image.stack.as_ptr(),
            in("t3") target,
            inout("a0") image.x[0] => int,
            in("a1") image.x[1],
            in("a2") image.x[2],
            in("a3") image.x[3],
            in("a4") image.x[4],
            in("a5") image.x[5],
            in("a6") image.x[6],
            in("a7") image.x[7],
            lateout("fa0") fp,
            out("t0") _,
            out("ra") _,
            clobber_abi("C"),
        );
    }
    (int, fp.to_bits())
}
