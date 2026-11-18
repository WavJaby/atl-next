// SPDX-License-Identifier: MIT
//! riscv64 down shells of the QEMU engine (Q6-4; interface: down.rs). Host calling convention: RISC-V psABI LP64D -- integer arguments in a0..a7, float/double in fa0..fa7, the rest on the stack from the caller's sp (no return address on the stack: `ra` holds it), results in a0 / fa0; s0 and ra callee-saved; t0, t1 free scratch.
//! The pool is assembled with `.option norvc`, so every shell is exactly `addi t1, zero, k` + `jal zero, entry` (4 + 4 bytes) whatever the target's C extension; `.option push` / `pop` restore the assembler state for the rest of the crate.

use std::fmt::Write;

use crate::down::{
    DownBackend, FP_AT, HOST_STACK_WORDS, HostAbi, LANES, RET_AT, STACK_AT, zeroed_offsets,
};

/// LP64D integer argument registers a0..a7, all stored into `regs`.
const INT_REGS: usize = 8;

/// Bytes of one down shell: two uncompressed instructions.
pub const DOWN_SHELL: usize = 8;
/// Slots a shell can name: `addi`'s immediate is 12-bit signed, so 0..2047.
pub const MAX_SLOTS: usize = 2048;
/// The entry's frame: the lanes, then saved s0 and ra; a multiple of 16 (psABI stack alignment).
const FRAME: usize = LANES + 16;
const _: () = assert!(FRAME.is_multiple_of(16));

/// The riscv64 backend (down.rs [`crate::down::backend`]).
pub const DOWN: DownBackend = DownBackend {
    shell: DOWN_SHELL,
    max_slots: MAX_SLOTS,
    att_syntax: false,
    abi: HostAbi {
        int_lanes: INT_REGS,
        fp_to_int: true,
        extend_int_ret: true,
        nan_box_f32: true,
    },
    entries_asm: down_entries_asm,
};

/// Assembly for the down shells: slot k is `addi t1, zero, k; jal zero, atl_nb_down_entry`. The entry builds the call's lanes on its frame (regs[0..8] = a0..a7, fp[0..8] = fa0..fa7 as stored by `fsd`, stack[0..8] = the eight words at the caller's sp, every other word 0), calls `atl_nb_down_dispatch(slot, lanes)`, and returns a0 = lanes.ret[0], fa0 = lanes.fp[0]. Uses t0, t1 and its frame; s0 (frame pointer = the caller's sp) and ra are saved and restored. `drop_stack` / `swap_v`: the breaks (down.rs [`DownBackend::entries_asm`]). Panics past [`MAX_SLOTS`].
pub fn down_entries_asm(slots: usize, drop_stack: bool, swap_v: bool) -> String {
    assert!(
        slots <= MAX_SLOTS,
        "{slots} down slots: addi names 0..{MAX_SLOTS}"
    );
    let mut s = String::from(
        ".text\n.option push\n.option norvc\n.p2align 3\n.hidden atl_nb_down_pool\n.globl atl_nb_down_pool\natl_nb_down_pool:\n",
    );
    for k in 0..slots {
        write!(
            s,
            "    addi t1, zero, {k}\n    jal zero, atl_nb_down_entry\n"
        )
        .expect("writing to a String");
    }
    write!(
        s,
        "atl_nb_down_entry:\n    addi sp, sp, -{FRAME}\n    sd ra, {}(sp)\n    sd s0, {LANES}(sp)\n    addi s0, sp, {FRAME}\n",
        LANES + 8
    )
    .expect("writing to a String");
    for i in 0..INT_REGS {
        writeln!(s, "    sd a{i}, {}(sp)", 8 * i).expect("writing to a String");
    }
    for at in zeroed_offsets(INT_REGS) {
        writeln!(s, "    sd zero, {at}(sp)").expect("writing to a String");
    }
    for i in 0..8 {
        let src = if swap_v && i < 2 { 1 - i } else { i };
        writeln!(s, "    fsd fa{src}, {}(sp)", FP_AT + 8 * i).expect("writing to a String");
    }
    // the caller's first stack argument is at its sp (= s0); `drop_stack` starts one word later
    let first = 8 * usize::from(drop_stack);
    for i in 0..HOST_STACK_WORDS {
        writeln!(
            s,
            "    ld t0, {}(s0)\n    sd t0, {}(sp)",
            first + 8 * i,
            STACK_AT + 8 * i
        )
        .expect("writing to a String");
    }
    write!(
        s,
        "    mv a0, t1\n    mv a1, sp\n    call atl_nb_down_dispatch\n    ld a0, {RET_AT}(sp)\n    fld fa0, {FP_AT}(sp)\n    ld s0, {LANES}(sp)\n    ld ra, {}(sp)\n    addi sp, sp, {FRAME}\n    ret\n.option pop\n",
        LANES + 8
    )
    .expect("writing to a String");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One slot, no break: the whole text, written out by hand from the psABI and the 4.0 `atl_lanes` layout (regs at 0, fp at 128, stack at 256, ret at 384, 400 bytes; frame 416 with s0 at 400, ra at 408). Expected text is a literal, never the emitter's output.
    #[test]
    fn one_slot_entry_is_the_psabi_sequence() {
        let want = "\
.text
.option push
.option norvc
.p2align 3
.hidden atl_nb_down_pool
.globl atl_nb_down_pool
atl_nb_down_pool:
    addi t1, zero, 0
    jal zero, atl_nb_down_entry
atl_nb_down_entry:
    addi sp, sp, -416
    sd ra, 408(sp)
    sd s0, 400(sp)
    addi s0, sp, 416
    sd a0, 0(sp)
    sd a1, 8(sp)
    sd a2, 16(sp)
    sd a3, 24(sp)
    sd a4, 32(sp)
    sd a5, 40(sp)
    sd a6, 48(sp)
    sd a7, 56(sp)
    sd zero, 64(sp)
    sd zero, 72(sp)
    sd zero, 80(sp)
    sd zero, 88(sp)
    sd zero, 96(sp)
    sd zero, 104(sp)
    sd zero, 112(sp)
    sd zero, 120(sp)
    sd zero, 192(sp)
    sd zero, 200(sp)
    sd zero, 208(sp)
    sd zero, 216(sp)
    sd zero, 224(sp)
    sd zero, 232(sp)
    sd zero, 240(sp)
    sd zero, 248(sp)
    sd zero, 320(sp)
    sd zero, 328(sp)
    sd zero, 336(sp)
    sd zero, 344(sp)
    sd zero, 352(sp)
    sd zero, 360(sp)
    sd zero, 368(sp)
    sd zero, 376(sp)
    sd zero, 384(sp)
    sd zero, 392(sp)
    fsd fa0, 128(sp)
    fsd fa1, 136(sp)
    fsd fa2, 144(sp)
    fsd fa3, 152(sp)
    fsd fa4, 160(sp)
    fsd fa5, 168(sp)
    fsd fa6, 176(sp)
    fsd fa7, 184(sp)
    ld t0, 0(s0)
    sd t0, 256(sp)
    ld t0, 8(s0)
    sd t0, 264(sp)
    ld t0, 16(s0)
    sd t0, 272(sp)
    ld t0, 24(s0)
    sd t0, 280(sp)
    ld t0, 32(s0)
    sd t0, 288(sp)
    ld t0, 40(s0)
    sd t0, 296(sp)
    ld t0, 48(s0)
    sd t0, 304(sp)
    ld t0, 56(s0)
    sd t0, 312(sp)
    mv a0, t1
    mv a1, sp
    call atl_nb_down_dispatch
    ld a0, 384(sp)
    fld fa0, 128(sp)
    ld s0, 400(sp)
    ld ra, 408(sp)
    addi sp, sp, 416
    ret
.option pop
";
        assert_eq!(down_entries_asm(1, false, false), want);
    }

    /// Slot k names k: the third shell is `addi t1, zero, 2`.
    #[test]
    fn slots_name_themselves() {
        let s = down_entries_asm(3, false, false);
        assert_eq!(s.matches("jal zero, atl_nb_down_entry").count(), 3);
        assert!(s.contains("    addi t1, zero, 2\n"));
    }

    /// Break `down_drop_stack`: the first stack word read is the caller's second. Break `down_swap_v`: fa1 lands in v[0], fa0 in v[1].
    #[test]
    fn breaks_move_one_lane_each() {
        let d = down_entries_asm(1, true, false);
        assert!(d.contains("    ld t0, 8(s0)\n    sd t0, 256(sp)\n"));
        let v = down_entries_asm(1, false, true);
        assert!(v.contains("    fsd fa1, 128(sp)\n    fsd fa0, 136(sp)\n"));
    }

    #[test]
    #[should_panic(expected = "addi names")]
    fn too_many_slots_panic() {
        down_entries_asm(MAX_SLOTS + 1, false, false);
    }
}
