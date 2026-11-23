// SPDX-License-Identifier: MIT
//! The AArch64 guest's `va_list` (Q6-7), read per a JNI shorty into the host's `jvalue[]`: a guest `...V` JNI call becomes the host's `...A` call, so no host `va_list` is built (dispatch.rs `call_object`). A guest `...` call's arguments are its lanes, read as the `va_list` its `va_start` would make ([`jvalues_lanes`]).
//! Layout (AAPCS64 B.3; LLVM AArch64ISelLowering.cpp `LowerAAPCS_VASTART`): `__stack`, `__gr_top`, `__vr_top`, `__gr_offs`, `__vr_offs`, 32 bytes, passed by reference. A register argument sits at `top + offs` (`offs` < 0 counts up by 8 for the integer save area, by 16 for the fp one, low 8 bytes = the double); once `offs` would pass 0 the class continues on `__stack`, 8-byte slots shared by both classes in argument order (berberis `GuestVAListParams`, the named-argument rule continued).
//! The ARM (AAPCS32) guest's `va_list` ([`jvalues32`]): AAPCS32 B.1 `typedef struct __va_list { void *__ap; } va_list`, passed by value, so the lane holds `__ap` itself. Variadic calls use the base standard (AAPCS32 6.4.1 / 5.5: no VFP registers), and va_start spills r0-r3 contiguous with the stack arguments, so the list is one run of 4-byte words: a 4-byte type takes one, an 8-byte type (J, D, and F after promotion to double) the next 8-aligned pair (C.3 / C.12: doubleword-aligned arguments; clang `ARMABIInfo::EmitVAArg` rounds `__ap` up to the type's alignment).
//! Types come from the shorty, as ART's own `CallXMethodV` and berberis `ConvertVAList` read them: Z B S C I promoted to an int slot, J and L a word, F promoted to double then narrowed, D a double. Guest addresses are host addresses (QEMU user mode, identity mapped).

use atl_engine_api::AtlLanes;

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
/// AAPCS64's argument registers: x0..x7, v0..v7 (each save area holds all eight).
const GR_COUNT: usize = 8;
const VR_COUNT: usize = 8;
/// A stack argument's slot: every shorty type is at most 8 bytes, rounded up to 8.
const STACK_SLOT: usize = 8;

/// The `jvalue[]` of the arguments `params` (a shorty without its result) read from the guest `va_list` at `va`; each word's low bytes are the union member jni.h names for its type. Err: a shorty character outside `ZBSCIJFDL`.
/// The guest's `va_list` is copied, not advanced: a V call takes it by value.
///
/// # Safety
/// `va` points to a live guest `va_list` whose remaining arguments are at least `params`.
pub(super) unsafe fn jvalues(va: usize, params: &[u8]) -> Result<Vec<u64>, Bad> {
    // SAFETY: caller contract.
    let mut v = unsafe { std::ptr::read_unaligned(va as *const VaList) };
    // SAFETY: caller contract; the guest's stack is not bounded here.
    unsafe { read(&mut v, usize::MAX, params) }
}

/// The `jvalue[]` of the arguments `params` of a `...` call, read from its lanes: `lanes` hold the call's registers and stack window as the callee's `va_start` saves them (AAPCS64 B.3). `fixed` counts the integer registers its named parameters take (JNI: env, object or class, methodID; one word each); the fp save area has no named slot. Stack arguments are read only inside the window, else [`Bad::Past`].
pub(super) fn jvalues_lanes(
    lanes: &AtlLanes,
    fixed: usize,
    params: &[u8],
) -> Result<Vec<u64>, Bad> {
    assert!(
        fixed <= GR_COUNT,
        "a `...` call's named parameters fit the integer registers"
    );
    let gr: [u64; GR_COUNT] = std::array::from_fn(|i| lanes.regs[i]);
    let vr: [u128; VR_COUNT] = std::array::from_fn(|i| u128::from(lanes.fp[i]));
    let stack = lanes.stack.as_ptr() as usize;
    let unnamed = (GR_COUNT - fixed) as i32;
    let mut v = VaList {
        stack,
        gr_top: gr.as_ptr() as usize + GR_COUNT * GR_SLOT as usize,
        vr_top: vr.as_ptr() as usize + VR_COUNT * VR_SLOT as usize,
        gr_offs: -(unnamed * GR_SLOT),
        vr_offs: -(VR_COUNT as i32 * VR_SLOT),
    };
    let end = stack + STACK_SLOT * lanes.stack.len();
    // SAFETY: `gr` and `vr` are the save areas `gr_top` and `vr_top` end, so every slot `gp` / `fp` reads at `top + offs` lies in them; `end` bounds the stack reads to `lanes.stack`.
    unsafe { read(&mut v, end, params) }
}

/// The walk shared by [`jvalues`] and [`jvalues_lanes`]: each argument of `params` read from `v` by its class, stack words only below `end`.
///
/// # Safety
/// As [`gp`] and [`fp`] for each argument of `params`.
unsafe fn read(v: &mut VaList, end: usize, params: &[u8]) -> Result<Vec<u64>, Bad> {
    let mut out = Vec::with_capacity(params.len());
    for &c in params {
        // SAFETY: each read takes the next argument of its class, which the caller's contract says exists.
        let w = unsafe {
            match c {
                b'Z' | b'B' => gp(v, end)? & 0xff,
                b'S' | b'C' => gp(v, end)? & 0xffff,
                b'I' => gp(v, end)? & 0xffff_ffff,
                b'J' | b'L' => gp(v, end)?,
                b'D' => fp(v, end)?,
                b'F' => u64::from((f64::from_bits(fp(v, end)?) as f32).to_bits()),
                _ => return Err(Bad::Shorty(c)),
            }
        };
        out.push(w);
    }
    Ok(out)
}

/// The next integer-class argument: the next integer save slot while one is left, else the next stack word.
///
/// # Safety
/// `v` is a live `va_list` (a guest's, or [`jvalues_lanes`]' copy) with an integer argument next; its stack words below `end` are readable.
unsafe fn gp(v: &mut VaList, end: usize) -> Result<u64, Bad> {
    let offs = v.gr_offs;
    if offs < 0 {
        v.gr_offs = offs + GR_SLOT;
        if v.gr_offs <= 0 {
            // SAFETY: caller contract: a register save slot of the list's frame.
            return Ok(unsafe { load(v.gr_top.wrapping_add_signed(offs as isize)) });
        }
    }
    // SAFETY: caller contract.
    unsafe { stack(v, end) }
}

/// The next fp-class argument's bits (a double).
/// Deliberate break `va_fp_gp` (qemu-jni gate, check V; unit tests here): read from the integer save area instead.
///
/// # Safety
/// As [`gp`], for an fp argument.
#[cfg(not(atl_nb_va_fp_gp))]
unsafe fn fp(v: &mut VaList, end: usize) -> Result<u64, Bad> {
    let offs = v.vr_offs;
    if offs < 0 {
        v.vr_offs = offs + VR_SLOT;
        if v.vr_offs <= 0 {
            // SAFETY: caller contract: the low 8 bytes of a v register save slot.
            return Ok(unsafe { load(v.vr_top.wrapping_add_signed(offs as isize)) });
        }
    }
    // SAFETY: caller contract.
    unsafe { stack(v, end) }
}

/// # Safety
/// As the non-break `fp`.
#[cfg(atl_nb_va_fp_gp)]
unsafe fn fp(v: &mut VaList, end: usize) -> Result<u64, Bad> {
    // SAFETY: caller contract.
    unsafe { gp(v, end) }
}

/// The next stack argument: the word at `v.stack`, if it lies below `end`.
///
/// # Safety
/// `v.stack` is the list's next stack argument, readable for 8 bytes whenever it lies below `end`.
unsafe fn stack(v: &mut VaList, end: usize) -> Result<u64, Bad> {
    let next = v
        .stack
        .checked_add(STACK_SLOT)
        .filter(|&n| n <= end)
        .ok_or(Bad::Past)?;
    // SAFETY: caller contract: the word lies below `end`.
    let w = unsafe { load(v.stack) };
    v.stack = next;
    Ok(w)
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

/// Why a list was not read ([`jvalues`], [`jvalues32`], [`jvalues_lanes`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Bad {
    /// a shorty character outside `ZBSCIJFDL`
    Shorty(u8),
    /// the arguments run past the readable end: a `...` call's lanes end at their stack window ([`image32`], [`jvalues_lanes`])
    Past,
}

/// [`jvalues`] for an ARM guest: `ap` is the `va_list`'s `__ap` (the lane's value, the struct being passed by value), `end` the first byte past the list's readable words (`usize::MAX` for a guest `va_list`, whose words are the guest's). Each value is widened into its `jvalue` word: I/S/B sign- or zero-free as jni.h's union stores the low bytes, L a 32-bit pointer zero-extended. Checked before any read: no word at or past `end` is read.
///
/// # Safety
/// `ap` is the live guest `__ap` whose remaining arguments are at least `params`, or a list readable up to `end`.
pub(super) unsafe fn jvalues32(ap: usize, end: usize, params: &[u8]) -> Result<Vec<u64>, Bad> {
    let mut ap = ap;
    let mut word = |wide: bool| -> Result<u64, Bad> {
        let align = if wide { 8 } else { SLOT32 };
        ap = ap.next_multiple_of(align);
        let next = ap + if wide { 8 } else { SLOT32 };
        if next > end {
            return Err(Bad::Past);
        }
        // SAFETY: caller contract: the next argument's word(s) of the guest's list, below `end`.
        let v = unsafe {
            if wide {
                load(ap)
            } else {
                u64::from(std::ptr::read_unaligned(ap as *const u32))
            }
        };
        ap = next;
        Ok(v)
    };
    params
        .iter()
        .map(|&c| {
            Ok(match c {
                b'Z' | b'B' => word(false)? & 0xff,
                b'S' | b'C' => word(false)? & 0xffff,
                b'I' | b'L' => word(false)?,
                b'J' | b'D' => word(true)?,
                b'F' => u64::from((f64::from_bits(word(true)?) as f32).to_bits()),
                _ => return Err(Bad::Shorty(c)),
            })
        })
        .collect()
}

/// An ARM guest's `jvalue` array at `at` for the arguments `params`, as the host's: jni.h's `jvalue` union is 8 bytes on ILP32 too (it holds `jlong`/`jdouble`, 8-aligned under AAPCS32), so the array strides alike, but storing `.l` writes only the low 4 bytes of an ILP32 entry and leaves the rest as they were: an `L` entry is cut to its low word (a 32-bit pointer zero-extended). Every other entry is copied whole; the host reads only its member's bytes.
///
/// # Safety
/// `at` is readable for `8 * params.len()` bytes.
pub(super) unsafe fn jvalues_a32(at: usize, params: &[u8]) -> Vec<u64> {
    params
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            // SAFETY: caller contract: entry i of the guest's array.
            let v = unsafe { load(at + 8 * i) };
            if c == b'L' && !cfg!(atl_nb_jvalue_l_raw) {
                v & 0xffff_ffff
            } else {
                v
            }
        })
        .collect()
}

/// An ARM guest's argument words `words` (r0-r3, then its stack window; each lane's low 32 bits) as one 8-aligned run of 4-byte words. That is the layout `va_start` reads (AAPCS32 B.1: r0-r3 spilled directly below the stack arguments, which start 8-aligned at a call, 5.2.1.2), so a `...` call's variadic arguments are read from it as from a `va_list` whose `__ap` is the word after the fixed parameters. Byte length = 4 x the word count.
pub(super) fn image32(words: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let w: Vec<u32> = words.into_iter().map(|x| x as u32).collect();
    w.chunks(2)
        .map(|p| u64::from(p[0]) | p.get(1).map_or(0, |&h| u64::from(h) << 32))
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
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, usize::MAX, b"IIJLLDF") }.unwrap();
        assert_eq!(
            v,
            [7, 9, j, 0x8000_1234, 0x5678, d, u64::from(1.5f32.to_bits())]
        );
        // an int then a long long: the pair skips word 1
        let pad = [3u64, j];
        // SAFETY: two 8-aligned u64: int 3 at 0, j at 8.
        let v = unsafe { jvalues32(pad.as_ptr() as usize, usize::MAX, b"IJ") };
        assert_eq!(v.unwrap(), [3, j]);
        // SAFETY: as above, a byte promoted to int.
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, usize::MAX, b"Z") };
        assert_eq!(v.unwrap(), [7]);
        // SAFETY: an unknown shorty character is refused before any read.
        let v = unsafe { jvalues32(aligned.as_ptr() as usize, usize::MAX, b"Q") };
        assert_eq!(v, Err(Bad::Shorty(b'Q')));
    }

    /// An ILP32 `jvalue[3]` for `(String, long, Object)` as the guest's compiler fills it after a memset of 0xA5 (as uninitialised stack): `.l` stores 4 bytes, `.j` 8. The host array has the references zero-extended and the long whole; red under break `jvalue_l_raw` (0xA5A5A5A5 in the high word).
    #[test]
    fn ilp32_jvalue_references_lose_the_stale_high_word() {
        let j = 0x1122_3344_5566_7788u64;
        let mut a = [0xA5A5_A5A5_A5A5_A5A5u64; 3];
        // SAFETY: writes the low 4 bytes of entries 0 and 2, as `v[i].l = p` does on ILP32 (little-endian).
        unsafe {
            (a.as_mut_ptr() as *mut u32).write(0x0000_2005);
            (a.as_mut_ptr().add(2) as *mut u32).write(0x8000_1001);
        }
        a[1] = j;
        // SAFETY: `a` holds three 8-byte entries.
        let v = unsafe { jvalues_a32(a.as_ptr() as usize, b"LJL") };
        assert_eq!(v, [0x2005, j, 0x8000_1001]);
    }

    /// A `...` call's lanes as its `va_list` (AAPCS32 base PCS, Parameter Passing): `CallStaticIntMethod(env, cls, mid, 2, 3)` puts env, cls, mid, 2 in r0-r3 and 3 in the first stack word, so from word 3 (after the three fixed words) the list reads 2, 3. `CallStaticVoidMethod(env, cls, mid, j)`: j needs an even register (C.3), r4 does not exist, so j goes to the stack at offset 0 (C.7, C.12), r3 unused -- the reader's 8-alignment from word 3 lands on word 4 = stack word 0. Arguments past the window are refused before a read.
    #[test]
    fn aapcs32_variadic_lanes_read_as_a_va_list() {
        let (env, cls, mid) = (0x1000u64, 0x2005, 0x41);
        let mut stack = [0u64; 16];
        stack[0] = 3;
        let img = image32([env, cls, mid, 2].into_iter().chain(stack));
        assert_eq!(img.len(), 10);
        let base = img.as_ptr() as usize;
        let end = base + 4 * 20;
        // SAFETY: `img` holds 20 words up to `end`.
        let v = unsafe { jvalues32(base + 4 * 3, end, b"II") };
        assert_eq!(v.unwrap(), [2, 3]);
        let j = 0x0102_0304_0506_0708u64;
        let mut stack = [0u64; 16];
        (stack[0], stack[1]) = (j & 0xffff_ffff, j >> 32);
        // r3 holds junk: an unused register is never read
        let img = image32([env, cls, mid, 0xdead].into_iter().chain(stack));
        let base = img.as_ptr() as usize;
        // SAFETY: as above.
        let v = unsafe { jvalues32(base + 4 * 3, base + 4 * 20, b"J") };
        assert_eq!(v.unwrap(), [j]);
        // nine longs need 72 bytes from word 4; the image has 64: refused
        // SAFETY: as above; nothing at or past `end` is read.
        let v = unsafe { jvalues32(base + 4 * 3, base + 4 * 20, b"JJJJJJJJJ") };
        assert_eq!(v, Err(Bad::Past));
    }

    /// The wideArgs call (`jni-test/atl_jni_test.c:277`): `CallStaticLongMethod(env, cls, mid, (jlong)0x100000000LL, 2.5, 7)` on `mixLong(JDI)J`. AAPCS64: the long in x3, the double in v0, the int in x4, after the three named words in x0..x2. The `jvalue[]` for the `...A` callee holds the three arguments in order. Red under break `va_fp_gp` (the double read from x4, the old crossing's misplacement).
    #[test]
    fn aapcs64_variadic_lanes_mixlong_fixture() {
        let mut l = AtlLanes::default();
        l.regs[..5].copy_from_slice(&[0x1000, 0x2005, 0x41, 0x1_0000_0000, 7]);
        l.fp[0] = 2.5f64.to_bits();
        assert_eq!(
            jvalues_lanes(&l, 3, b"JDI"),
            Ok(vec![0x1_0000_0000, 2.5f64.to_bits(), 7])
        );
    }

    /// The qemu-jni fixture's 19 mixed arguments as a `...` call (`CallStaticDoubleMethod(env, cls, V_MID, V_ARGS)`, check V). Named words x0..x2; x3..x7 take the first five integer-class arguments, v0..v7 the first eight fp ones; the rest are stack words in argument order: I 13, J, I 14, J 5, L, then D 5.875 (six words). F arrives promoted to double and reads back as float bits. Expected values are fixture.c's `V_ARGS`, written out here.
    #[test]
    fn aapcs64_variadic_lanes_mixed_fixture() {
        let mut l = AtlLanes::default();
        l.regs[..8].copy_from_slice(&[
            0x1000,
            0x2005,
            0x7a11,
            11,
            0x1_0000_0002,
            0xb01,
            (-12i32) as u32 as u64,
            (-3i64) as u64,
        ]);
        l.fp[..8]
            .copy_from_slice(&[1.25, 0.5, 2.5, 1.75, 3.75, 2.25, 4.5, 3.125].map(f64::to_bits));
        l.stack[..6].copy_from_slice(&[13, 0x7_0000_0004, 14, 5, 0xb02, 5.875f64.to_bits()]);
        assert_eq!(
            jvalues_lanes(&l, 3, b"IDJFLIDJFIDJFIDJFLD"),
            Ok(vec![
                11,
                1.25f64.to_bits(),
                0x1_0000_0002,
                u64::from(0.5f32.to_bits()),
                0xb01,
                0xffff_fff4,
                2.5f64.to_bits(),
                (-3i64) as u64,
                u64::from(1.75f32.to_bits()),
                13,
                3.75f64.to_bits(),
                0x7_0000_0004,
                u64::from(2.25f32.to_bits()),
                14,
                4.5f64.to_bits(),
                5,
                u64::from(3.125f32.to_bits()),
                0xb02,
                5.875f64.to_bits(),
            ])
        );
    }

    /// The stack window is `LANE_STACK` = 16 words: with x3..x7 taken by the first five longs, 21 longs read exactly the window; 22 are refused before the read past it. An unknown shorty character is refused too.
    #[test]
    fn aapcs64_variadic_lanes_window_bounds() {
        let l = AtlLanes::default();
        assert_eq!(jvalues_lanes(&l, 3, &[b'J'; 21]).map(|v| v.len()), Ok(21));
        assert_eq!(jvalues_lanes(&l, 3, &[b'J'; 22]), Err(Bad::Past));
        assert_eq!(jvalues_lanes(&l, 3, b"Q"), Err(Bad::Shorty(b'Q')));
    }
}
