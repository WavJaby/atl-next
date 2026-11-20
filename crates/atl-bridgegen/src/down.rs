// SPDX-License-Identifier: MIT
//! Down shells of the QEMU engine (host->guest, Q6-4), one generator interface with a backend per host ISA ([`backend`]): x86_64 SysV (`x86.rs`), riscv64 LP64D (`riscv64.rs`).
//! A down shell is runtime-copied bytes: a template assembled once (`atl_nb_down_shell`..`atl_nb_down_shell_end`, [`SHELL_LEN`] bytes) whose position-independent code loads two 8-byte literals stored after it -- the entry address ([`SHELL_ENTRY_AT`]) and the shell's `DownSlot` pointer ([`SHELL_SLOT_AT`]) -- and jumps to the entry with the slot pointer in a scratch register. The engine copies the template into its code chunks (atl-native-bridge engine/code_chunk.rs) and writes the two literals, so no shell count is fixed at build time (berberis' `CodePool` is unbounded too).
//! The entry stores the host call's argument registers and [`HOST_STACK_WORDS`] caller stack words into an `atl_lanes` image on its own frame (the rest zeroed), calls `atl_nb_down_dispatch(slot, lanes)` (atl-native-bridge engine/dispatch.rs), and returns lanes.ret[0] in the integer result register and lanes.fp[0] in the fp one.
//! What differs per host beyond the asm is [`HostAbi`]: how dispatch.rs reads the host image and shapes the answer. build.rs writes [`DownBackend::entries_asm`] (`down.s`, assembled by place.rs `global_asm!`) and [`abi_rust`] (`down_abi.rs`).

/// Bytes of the shell template: 16 bytes of code (x86_64 `movq` + `jmp *` = 13, int3-padded; riscv64 four uncompressed instructions), then the two literals.
pub const SHELL_LEN: usize = 32;
/// Offset of the entry-address literal in a shell.
pub const SHELL_ENTRY_AT: usize = 16;
/// Offset of the `DownSlot` pointer literal in a shell; the entry passes it as `atl_nb_down_dispatch`'s first argument.
pub const SHELL_SLOT_AT: usize = 24;

/// Bytes of a down call's lanes (`atl_lanes`, atl_engine.h 4.0): regs[16] at 0, fp[16] at [`FP_AT`], stack[16] at [`STACK_AT`], ret[2] at [`RET_AT`]. The entry's frame holds one; atl-native-bridge asserts these against the struct ([`pool_rust`] carries them).
pub const LANES: usize = 400;
/// Offset of `fp` in the down call's lanes.
pub const FP_AT: usize = 128;
/// Offset of `stack` in the down call's lanes.
pub const STACK_AT: usize = 256;
/// Offset of `ret` in the down call's lanes: the integer answer the entry returns (`ret[0]`).
pub const RET_AT: usize = 384;
/// Words per bank of the lanes (regs, fp, stack).
pub const BANK_WORDS: usize = 16;
/// Caller stack words a shell copies into the window: the whole bank (the host convention's side of the image; a placement needing more is refused, R-1). Eight refused Candy Crush Saga's 15-integer JNI method (SysV: nine words); the copy reads at most 128 bytes above the return address, inside the caller's frames. The guest's own window (aarch64 8) bounds the call separately.
pub const HOST_STACK_WORDS: usize = BANK_WORDS;

/// The host calling convention's rules dispatch.rs applies to the lanes image a shell built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostAbi {
    /// integer argument registers the shell stored into x[0..]: SysV 6 (%rdi..%r9), LP64D 8 (a0..a7)
    pub int_lanes: usize,
    /// a float or double past the fp argument registers takes a free integer register before the stack (RISC-V psABI: "passed according to the integer calling convention")
    pub fp_to_int: bool,
    /// an integer result narrower than 64 bits is widened by its type to 32 bits, then sign-extended to 64 (RISC-V psABI); AAPCS64 leaves those bits unspecified, so the answer is extended
    pub extend_int_ret: bool,
    /// a float result is NaN-boxed (upper 32 bits all ones; RISC-V F/D: a non-boxed value reads as the canonical NaN); AArch64 writes s0 with zero upper bits
    pub nan_box_f32: bool,
}

/// One host ISA's down-shell generator.
#[derive(Clone, Copy, Debug)]
pub struct DownBackend {
    /// its text is AT&T syntax: the engine assembles it with `options(att_syntax)` (atl-native-bridge build.rs cfg `atl_nb_down_att`), otherwise with default options
    pub att_syntax: bool,
    pub abi: HostAbi,
    /// the shell template and the entry as assembly text for the engine's `global_asm!`, with the breaks `down_drop_stack` (stack words start one word late) and `down_swap_v` (fp argument registers 0 and 1 exchanged on entry): `(drop_stack, swap_v)`
    pub entries_asm: fn(bool, bool) -> String,
}

/// The down-shell backend of host ISA `arch` (arches.toml key); None where no backend is written.
pub fn backend(arch: &str) -> Option<DownBackend> {
    match arch {
        "x86_64" => Some(crate::x86::DOWN),
        "riscv64" => Some(crate::riscv64::DOWN),
        _ => None,
    }
}

/// `down_abi.rs`: the shell template's layout, the host ABI rules dispatch.rs reads, and the lanes layout the shells' asm uses (asserted there against `AtlLanes`).
pub fn abi_rust(b: &DownBackend) -> String {
    let a = b.abi;
    format!(
        "pub const SHELL_LEN: usize = {SHELL_LEN};\n\
         pub const SHELL_ENTRY_AT: usize = {SHELL_ENTRY_AT};\n\
         pub const SHELL_SLOT_AT: usize = {SHELL_SLOT_AT};\n\
         pub const HOST_INT_LANES: usize = {};\n\
         pub const HOST_FP_TO_INT: bool = {};\n\
         pub const HOST_EXTEND_INT_RET: bool = {};\n\
         pub const HOST_NAN_BOX_F32: bool = {};\n\
         pub const HOST_STACK_WORDS: usize = {HOST_STACK_WORDS};\n\
         pub const LANES_REGS_AT: usize = 0;\n\
         pub const LANES_FP_AT: usize = {FP_AT};\n\
         pub const LANES_STACK_AT: usize = {STACK_AT};\n\
         pub const LANES_RET_AT: usize = {RET_AT};\n\
         pub const LANES_SIZE: usize = {LANES};\n",
        a.int_lanes, a.fp_to_int, a.extend_int_ret, a.nan_box_f32
    )
}

/// Assembly-independent part of a shell's frame fill: the frame offsets of the lanes a shell zeroes (every word it does not store), so the image dispatch.rs reads is fully defined. `int_stored` integer registers and [`HOST_STACK_WORDS`] stack words are stored; fp stores the first eight.
pub fn zeroed_offsets(int_stored: usize) -> Vec<usize> {
    let regs = (int_stored..BANK_WORDS).map(|i| 8 * i);
    let fp = (8..BANK_WORDS).map(|i| FP_AT + 8 * i);
    let stack = (HOST_STACK_WORDS..BANK_WORDS).map(|i| STACK_AT + 8 * i);
    let ret = (0..2).map(|i| RET_AT + 8 * i);
    regs.chain(fp).chain(stack).chain(ret).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// atl_lanes 4.0 = banks of sixteen u64 words in order regs, fp, stack, then ret[2] (atl_engine.h `struct atl_lanes`).
    #[test]
    fn lanes_layout() {
        assert_eq!(
            (FP_AT, STACK_AT, RET_AT, LANES),
            (16 * 8, 32 * 8, 48 * 8, 50 * 8)
        );
    }

    /// SysV stores six integer registers and the whole stack bank: the other ten registers, fp 8..16 and ret are zeroed = 10 + 8 + 2 words.
    #[test]
    fn zeroed_offsets_cover_the_unstored_words() {
        let z = zeroed_offsets(6);
        assert_eq!(z.len(), 20);
        assert_eq!((z[0], *z.last().expect("ret[1]")), (48, RET_AT + 8));
    }

    #[test]
    fn backends_by_arch() {
        assert_eq!(backend("x86_64").map(|b| b.abi.int_lanes), Some(6));
        assert_eq!(backend("riscv64").map(|b| b.abi.int_lanes), Some(8));
        assert!(backend("arm64").is_none());
    }

    #[test]
    fn abi_rust_names_every_rule() {
        let s = abi_rust(&crate::riscv64::DOWN);
        assert!(s.contains("SHELL_LEN: usize = 32;"));
        assert!(s.contains("SHELL_ENTRY_AT: usize = 16;"));
        assert!(s.contains("SHELL_SLOT_AT: usize = 24;"));
        assert!(s.contains("HOST_INT_LANES: usize = 8;"));
        assert!(s.contains("HOST_FP_TO_INT: bool = true;"));
    }
}
