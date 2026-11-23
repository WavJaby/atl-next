// SPDX-License-Identifier: MIT
//! The up call on a host whose variadic convention takes the guest's lanes as they stand (x86_64 SysV, the reference; AArch64 alike): one variadic call `f(x0..x7, v0..v7, s0..s7)`.
//! SysV: integer words x0..x7 then the eight stack words (SysV takes the sixth and later integer words from the stack, in order), fp lanes v0..v7 in xmm0..7. Both ABIs assign integer and fp arguments to registers by class in declaration order, so the guest's lanes are the callee's register image as they stand; the prototype's classes need no walk.
//! Eight fp words always pass, so the call sets %al = 8: a variadic callee (glibc printf) saves its xmm arguments only when %al is nonzero. 8 is the ABI's upper bound on the vector registers used; a variadic call's fp count is unknown, so the exact count is not built.
//! Not represented: fp arguments past eight, or more than 16 integers (R-1): a proxy row with such a prototype is a named trap from build time (atl_bridgegen::jni `lane_refusal`); a `bind` caller has no prototype and is not checked.

use atl_engine_api::AtlLanes;

use crate::engine::objects::Sig;

#[cfg(not(atl_nb_al_zero))]
type HostWords = unsafe extern "C" fn(u64, ...) -> u64;
/// The same image, for a result in xmm0.
#[cfg(not(atl_nb_al_zero))]
type HostFp = unsafe extern "C" fn(u64, ...) -> f64;

/// The first eight lanes of a bank: the image passes eight words per bank (AAPCS64's registers and window).
fn first8(bank: &[u64]) -> [u64; 8] {
    std::array::from_fn(|i| bank[i])
}

/// Calls `target` with `lanes` as the image above (x = `regs`, v = `fp`, s = `stack`, eight each); `fp`: its result is in xmm0. Answers (rax, xmm0 bits), the unread one 0. `sig.params` is not read: the image is the callee's as it stands.
/// Break `al_zero` makes the call with %al = 0 ([`call_al0`]).
pub(super) fn call(target: usize, lanes: &AtlLanes, fp: bool, _sig: Sig) -> (u64, u64) {
    let [x0, x1, x2, x3, x4, x5, x6, x7] = first8(&lanes.regs);
    let [v0, v1, v2, v3, v4, v5, v6, v7] = first8(&lanes.fp).map(f64::from_bits);
    #[cfg(atl_nb_al_zero)]
    {
        let (int, f) = call_al0(
            target,
            [x0, x1, x2, x3, x4, x5, x6, x7],
            [v0, v1, v2, v3, v4, v5, v6, v7],
            first8(&lanes.stack),
        );
        if fp { (0, f.to_bits()) } else { (int, 0) }
    }
    #[cfg(not(atl_nb_al_zero))]
    {
        let [s0, s1, s2, s3, s4, s5, s6, s7] = first8(&lanes.stack);
        if fp {
            // SAFETY: `target` is the host C function a binding or JNI entry names, and its result is a double or float (the descriptor's or the table's `ret`); the call passes the guest words in the image above, which such a callee reads by its own prototype.
            let f = unsafe { std::mem::transmute::<usize, HostFp>(target) };
            // SAFETY: as above; a callee with fewer parameters ignores the rest.
            let r = unsafe {
                f(
                    x0, x1, x2, x3, x4, x5, x6, x7, v0, v1, v2, v3, v4, v5, v6, v7, s0, s1, s2, s3,
                    s4, s5, s6, s7,
                )
            };
            (0, r.to_bits())
        } else {
            // SAFETY: `target` is a C-ABI host function of this trap (see above); the same image as the fp arm, with an integer or pointer result.
            let f = unsafe { std::mem::transmute::<usize, HostWords>(target) };
            // SAFETY: as above; a callee with fewer parameters ignores the rest.
            let r = unsafe {
                f(
                    x0, x1, x2, x3, x4, x5, x6, x7, v0, v1, v2, v3, v4, v5, v6, v7, s0, s1, s2, s3,
                    s4, s5, s6, s7,
                )
            };
            (r, 0)
        }
    }
}

/// Break `al_zero` (`ATL_NB_BREAK=al_zero`): [`call`]'s image, called from asm with %al = 0 where the normal call sets 8. A variadic callee saves its xmm arguments only when %al is nonzero (SysV), so a double vararg reads garbage. Returns rax and xmm0. The ten stack words (x6, x7, then s0..s7) are pushed so the callee finds the first at the lowest address.
#[cfg(atl_nb_al_zero)]
fn call_al0(target: usize, x: [u64; 8], v: [f64; 8], s: [u64; 8]) -> (u64, f64) {
    let stack = [x[6], x[7], s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]];
    let p = stack.as_ptr();
    let (int, fp): (u64, f64);
    // SAFETY: `target` is a C host function of this trap, called with `call`'s image: the register words in their registers, the stack words pushed in reverse (ten pushes keep rsp 16-byte aligned at the call), rsp restored after the call. `stack` outlives the asm block. %al is zero, the one register this break changes.
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
