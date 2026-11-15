// SPDX-License-Identifier: MIT
//! Down shells of the QEMU engine (host->guest, Q6-4), one generator interface with a backend per host ISA ([`backend`]): x86_64 SysV (`x86.rs`), riscv64 LP64D (`riscv64.rs`).
//! A host call enters shell k of the pool, which jumps to the entry with k in a scratch register; the entry stores the host call's argument registers and eight caller stack words into an `atl_lanes` image on its own frame, calls `atl_nb_down_dispatch(k, lanes)` (atl-native-bridge engine/dispatch.rs), and returns lanes.x[0] in the integer result register and lanes.v[0] in the fp one.
//! What differs per host beyond the asm is [`HostAbi`]: how dispatch.rs reads the host image and shapes the answer. build.rs writes [`DownBackend::entries_asm`] (`down.s`, assembled by place.rs `global_asm!`) and [`pool_rust`] (`down_pool.rs`).

/// Bytes of a down call's lanes (`atl_lanes`, atl_engine.h): x[8] at 0, v[8] at [`V_AT`], stack[8] at [`STACK_AT`]. The entry's frame holds one.
pub const LANES: usize = 192;
/// Offset of `v` in the down call's lanes.
pub const V_AT: usize = 64;
/// Offset of `stack` in the down call's lanes.
pub const STACK_AT: usize = 128;
/// Words per bank of the lanes (x, v, stack).
pub const LANE_WORDS: usize = 8;

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
    /// bytes per shell: slot k sits at `atl_nb_down_pool + k * shell`
    pub shell: usize,
    /// the most slots its shell encoding can name
    pub max_slots: usize,
    /// its text is AT&T syntax: the engine assembles it with `options(att_syntax)` (atl-native-bridge build.rs cfg `atl_nb_down_att`), otherwise with default options
    pub att_syntax: bool,
    pub abi: HostAbi,
    /// the pool and entry as assembly text for the engine's `global_asm!`, with the breaks `down_drop_stack` (stack words start one word late) and `down_swap_v` (fp argument registers 0 and 1 exchanged on entry): `(slots, drop_stack, swap_v)`
    pub entries_asm: fn(usize, bool, bool) -> String,
}

/// The down-shell backend of host ISA `arch` (arches.toml key); None where no backend is written.
pub fn backend(arch: &str) -> Option<DownBackend> {
    match arch {
        "x86_64" => Some(crate::x86::DOWN),
        "riscv64" => Some(crate::riscv64::DOWN),
        _ => None,
    }
}

/// `down_pool.rs`: the pool's size and shell stride, and the host ABI rules dispatch.rs reads.
pub fn pool_rust(b: &DownBackend, slots: usize) -> String {
    let a = b.abi;
    format!(
        "pub const DOWN_SLOTS: usize = {slots};\n\
         pub const DOWN_SHELL: usize = {};\n\
         pub const HOST_INT_LANES: usize = {};\n\
         pub const HOST_FP_TO_INT: bool = {};\n\
         pub const HOST_EXTEND_INT_RET: bool = {};\n\
         pub const HOST_NAN_BOX_F32: bool = {};\n",
        b.shell, a.int_lanes, a.fp_to_int, a.extend_int_ret, a.nan_box_f32
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// atl_lanes = three banks of eight u64 words, in order x, v, stack (atl_engine.h `struct atl_lanes`).
    #[test]
    fn lanes_layout() {
        assert_eq!((V_AT, STACK_AT, LANES), (8 * 8, 16 * 8, 24 * 8));
    }

    #[test]
    fn backends_by_arch() {
        assert_eq!(backend("x86_64").map(|b| b.abi.int_lanes), Some(6));
        assert_eq!(backend("riscv64").map(|b| b.abi.int_lanes), Some(8));
        assert!(backend("arm64").is_none());
    }

    #[test]
    fn pool_rust_names_every_rule() {
        let s = pool_rust(&crate::riscv64::DOWN, 4);
        assert!(s.contains("DOWN_SLOTS: usize = 4;"));
        assert!(s.contains("DOWN_SHELL: usize = 8;"));
        assert!(s.contains("HOST_INT_LANES: usize = 8;"));
        assert!(s.contains("HOST_FP_TO_INT: bool = true;"));
    }
}
