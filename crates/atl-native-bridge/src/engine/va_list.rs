// SPDX-License-Identifier: MIT
//! The AArch64 guest's `va_list` (Q6-7), read per a JNI shorty into the host's `jvalue[]`: a guest `...V` JNI call becomes the host's `...A` call, so no host `va_list` is built (dispatch.rs `call_object`).
//! Layout (AAPCS64 B.3; LLVM AArch64ISelLowering.cpp `LowerAAPCS_VASTART`): `__stack`, `__gr_top`, `__vr_top`, `__gr_offs`, `__vr_offs`, 32 bytes, passed by reference. A register argument sits at `top + offs` (`offs` < 0 counts up by 8 for the integer save area, by 16 for the fp one, low 8 bytes = the double); once `offs` would pass 0 the class continues on `__stack`, 8-byte slots shared by both classes in argument order (berberis `GuestVAListParams`, the named-argument rule continued).
//! The ARM (AAPCS32) guest's `va_list` ([`jvalues32`]): AAPCS32 B.1 `typedef struct __va_list { void *__ap; } va_list`, passed by value, so the lane holds `__ap` itself. Variadic calls use the base standard (AAPCS32 6.4.1 / 5.5: no VFP registers), and va_start spills r0-r3 contiguous with the stack arguments, so the list is one run of 4-byte words: a 4-byte type takes one, an 8-byte type (J, D, and F after promotion to double) the next 8-aligned pair (C.3 / C.12: doubleword-aligned arguments; clang `ARMABIInfo::EmitVAArg` rounds `__ap` up to the type's alignment).
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

/// An AAPCS32 `va_list` slot: one 4-byte word; 8-byte types take an 8-aligned pair. Break `va32_slot8` (qemu main gate, check (k) variadic call; unit test below): 8-byte slots for every type, as an LP64 reader would.
#[cfg(not(atl_nb_va32_slot8))]
const SLOT32: usize = 4;
#[cfg(atl_nb_va32_slot8)]
const SLOT32: usize = 8;

/// [`jvalues`] for an ARM guest: `ap` is the `va_list`'s `__ap` (the lane's value, the struct being passed by value). Each value is widened into its `jvalue` word: I/S/B sign- or zero-free as jni.h's union stores the low bytes, L a 32-bit pointer zero-extended.
///
/// # Safety
/// `ap` is the live guest `__ap` whose remaining arguments are at least `params`.
pub(super) unsafe fn jvalues32(ap: usize, params: &[u8]) -> Result<Vec<u64>, u8> {
    let mut ap = ap;
    let mut word = |wide: bool| -> u64 {
        let align = if wide { 8 } else { SLOT32 };
        ap = ap.next_multiple_of(align);
        // SAFETY: caller contract: the next argument's word(s) of the guest's list.
        let v = unsafe {
            if wide {
                load(ap)
            } else {
                u64::from(std::ptr::read_unaligned(ap as *const u32))
            }
        };
        ap += if wide { 8 } else { SLOT32 };
        v
    };
    params
        .iter()
        .map(|&c| {
            Ok(match c {
                b'Z' | b'B' => word(false) & 0xff,
                b'S' | b'C' => word(false) & 0xffff,
                b'I' | b'L' => word(false),
                b'J' | b'D' => word(true),
                b'F' => u64::from((f64::from_bits(word(true)) as f32).to_bits()),
                _ => return Err(c),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AAPCS32 B.1 + C.3/C.12: the variadic part `(int 7, int 9, long long j, char* p, char* q, double 2.5, float 1.5f)` from an 8-aligned `__ap` is, by byte offset: 7 at 0, 9 at 4, j at 8, p at 16, q at 20, 2.5 at 24, 1.5 promoted to double at 32 (no padding: each pair already falls 8-aligned). Then `(int 3, long long j)`: 3 at 0, pad, j at 8. Laid out by hand from those rules, read back by shorty; red under `va32_slot8` (the second int read at 8).
    #[test]
    fn aapcs32_va_list_words_and_aligned_pairs() {
        let mut m = [0u32; 10];
        (m[0], m[1]) = (7, 9);
        let j = 0x1122_3344_5566_7788u64;
        (m[2], m[3]) = (j as u32, (j >> 32) as u32);
        // L: 32-bit guest pointers, one with the high bit set (zero-extended)
        (m[4], m[5]) = (0x8000_1234, 0x5678);
        let d = 2.5f64.to_bits();
        (m[6], m[7]) = (d as u32, (d >> 32) as u32);
        let f = 1.5f64.to_bits(); // F promoted to double
        (m[8], m[9]) = (f as u32, (f >> 32) as u32);
        // the list starts 8-aligned, as the guest's stack at a public interface is (AAPCS32 5.2.1.2)
        let mut aligned = [0u64; 6];
        // SAFETY: `aligned` holds the 40 bytes of `m`, 8-aligned.
        unsafe {
            std::ptr::copy_nonoverlapping(
                m.as_ptr().cast::<u8>(),
                aligned.as_mut_ptr().cast::<u8>(),
                40,
            )
        };
        // SAFETY: the list above, one argument per shorty character.
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, b"IIJLLDF") }.unwrap();
        assert_eq!(
            v,
            [7, 9, j, 0x8000_1234, 0x5678, d, u64::from(1.5f32.to_bits())]
        );
        // an int then a long long: the pair skips word 1
        let pad = [3u64, j];
        // SAFETY: two 8-aligned u64: int 3 at 0, j at 8.
        let v = unsafe { jvalues32(pad.as_ptr() as usize, b"IJ") };
        assert_eq!(v.unwrap(), [3, j]);
        // SAFETY: as above, a byte promoted to int.
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, b"Z") };
        assert_eq!(v.unwrap(), [7]);
        // SAFETY: an unknown shorty character is refused before any read.
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, b"Q") };
        assert_eq!(v, Err(b'Q'));
    }
}
