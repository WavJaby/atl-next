// SPDX-License-Identifier: MIT
//! The AArch64 guest's `va_list` (Q6-7), read per a JNI shorty into the host's `jvalue[]`: a guest `...V` JNI call becomes the host's `...A` call, so no host `va_list` is built (dispatch.rs `call_jni`).
//! Layout (AAPCS64 B.3; LLVM AArch64ISelLowering.cpp `LowerAAPCS_VASTART`): `__stack`, `__gr_top`, `__vr_top`, `__gr_offs`, `__vr_offs`, 32 bytes, passed by reference. A register argument sits at `top + offs` (`offs` < 0 counts up by 8 for the integer save area, by 16 for the fp one, low 8 bytes = the double); once `offs` would pass 0 the class continues on `__stack`, 8-byte slots shared by both classes in argument order (berberis `GuestVAListParams`, the named-argument rule continued).
//! Types come from the shorty, as ART's own `CallXMethodV` and berberis `ConvertVAList` read them: Z B S C I promoted to an int slot, J and L a word, F promoted to double then narrowed, D a double. Guest addresses are host addresses (QEMU user mode, identity mapped).

/// AAPCS64 `va_list`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VaList {
    stack: usize,
    gr_top: usize,
    vr_top: usize,
    gr_offs: i32,
    vr_offs: i32,
}
const _: () = assert!(std::mem::size_of::<VaList>() == 32);
const _: () = assert!(
    std::mem::offset_of!(VaList, gr_offs) == 24 && std::mem::offset_of!(VaList, vr_offs) == 28
);

/// A register save area's slot sizes: x registers 8 bytes, v registers 16.
const GR_SLOT: i32 = 8;
const VR_SLOT: i32 = 16;
/// A stack argument's slot: every shorty type is at most 8 bytes, rounded up to 8.
const STACK_SLOT: usize = 8;

/// The `jvalue[]` of the arguments `params` (a shorty without its result) read from the guest `va_list` at `va`; each word's low bytes are the union member jni.h names for its type. Err: a shorty character outside `ZBSCIJFDL`.
/// The guest's `va_list` is copied, not advanced: a V call takes it by value.
///
/// # Safety
/// `va` points to a live guest `va_list` whose remaining arguments are at least `params`.
pub(super) unsafe fn jvalues(va: usize, params: &[u8]) -> Result<Vec<u64>, u8> {
    // SAFETY: caller contract.
    let mut v = unsafe { std::ptr::read_unaligned(va as *const VaList) };
    params
        .iter()
        .map(|&c| {
            // SAFETY: each read takes the next argument of its class, which the caller's contract says exists.
            Ok(unsafe {
                match c {
                    b'Z' | b'B' => gp(&mut v) & 0xff,
                    b'S' | b'C' => gp(&mut v) & 0xffff,
                    b'I' => gp(&mut v) & 0xffff_ffff,
                    b'J' | b'L' => gp(&mut v),
                    b'D' => fp(&mut v),
                    b'F' => u64::from((f64::from_bits(fp(&mut v)) as f32).to_bits()),
                    _ => return Err(c),
                }
            })
        })
        .collect()
}

/// The next integer-class argument.
///
/// # Safety
/// `v` is a live guest `va_list` with an integer argument next.
unsafe fn gp(v: &mut VaList) -> u64 {
    let offs = v.gr_offs;
    if offs < 0 {
        v.gr_offs = offs + GR_SLOT;
        if v.gr_offs <= 0 {
            // SAFETY: caller contract: a register save slot of the guest's frame.
            return unsafe { load(v.gr_top.wrapping_add_signed(offs as isize)) };
        }
    }
    // SAFETY: caller contract.
    unsafe { stack(v) }
}

/// The next fp-class argument's bits (a double).
/// Deliberate break `va_fp_gp` (qemu-jni gate, check V): read from the integer save area instead.
///
/// # Safety
/// `v` is a live guest `va_list` with an fp argument next.
#[cfg(not(atl_nb_va_fp_gp))]
unsafe fn fp(v: &mut VaList) -> u64 {
    let offs = v.vr_offs;
    if offs < 0 {
        v.vr_offs = offs + VR_SLOT;
        if v.vr_offs <= 0 {
            // SAFETY: caller contract: the low 8 bytes of a v register save slot.
            return unsafe { load(v.vr_top.wrapping_add_signed(offs as isize)) };
        }
    }
    // SAFETY: caller contract.
    unsafe { stack(v) }
}

/// # Safety
/// As the non-break `fp`.
#[cfg(atl_nb_va_fp_gp)]
unsafe fn fp(v: &mut VaList) -> u64 {
    // SAFETY: caller contract.
    unsafe { gp(v) }
}

/// # Safety
/// `v.stack` is the guest's next stack argument.
unsafe fn stack(v: &mut VaList) -> u64 {
    // SAFETY: caller contract.
    let w = unsafe { load(v.stack) };
    v.stack += STACK_SLOT;
    w
}

/// # Safety
/// `a` is readable for 8 bytes.
unsafe fn load(a: usize) -> u64 {
    // SAFETY: caller contract.
    unsafe { std::ptr::read_unaligned(a as *const u64) }
}
