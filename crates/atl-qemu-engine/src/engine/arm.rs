// SPDX-License-Identifier: MIT
//! arm-linux-user (arm32 guests, phase 4): the guest registers, AAPCS32 guest calls and the command line of the engine `.so`, core registers only.
//! Lanes (engine ABI 4.0): r0..r3 = `regs[0..4]` (zero-extended), the stack arguments = `stack[0..16]`, one 32-bit word each at sp + 4i; answer r0 / r1 in `ret` (a 64-bit result's pair). No fp lane: Android armeabi-v7a is softfp, FP values travel in core registers.

use atl_engine_api::{AtlLanes, LANE_STACK};

use super::{Env, Guest, lib, page, ret_stub};

/// argv[0] of the engine `.so`'s QEMU command line.
#[cfg(feature = "engine-so")]
pub(super) const ARGV0: &str = "qemu-arm";

/// The guest CPU QEMU emulates (`-cpu`): none, QEMU's default for qemu-arm (`max`), the CPU the P1 spike gates ran on. Choosing the reference device class's model (behaviour decision 1: an arm32 app on an ARMv8 phone in AArch32 state) is open: phase 4 P8 measures what the first app reads.
#[cfg(feature = "engine-so")]
pub(crate) fn guest_cpu() -> Option<&'static str> {
    None
}

/// r0..r15, `uint32_t regs[16]`; r13 = sp, r14 = lr, r15 = pc.
const REGS: usize = crate::layout::ATL_OFFSET_REGS as usize;
/// `uint32_t thumb`: the CPSR T bit linux-user keeps apart (target/arm/cpu.h).
const THUMB: usize = crate::layout::ATL_OFFSET_THUMB as usize;
/// `uint32_t condexec_bits`: the IT block state, 0 outside one.
const CONDEXEC: usize = crate::layout::ATL_OFFSET_CONDEXEC as usize;
/// TPIDRURO (`cp15.tpidrro_el[0]`, uint64_t), the thread pointer linux-user's `cpu_set_tls` writes.
const TPIDRURO: usize = crate::layout::ATL_OFFSET_TPIDRURO as usize;
const _: () = assert!(REGS + 64 <= Env::SIZE && TPIDRURO + 8 <= Env::SIZE);
const _: () = assert!(THUMB + 4 <= Env::SIZE && CONDEXEC + 4 <= Env::SIZE);
const SP: usize = 13;
const LR: usize = 14;
const PC: usize = 15;
/// Bytes per stack word.
const WORD: u64 = 4;
/// AAPCS32 core argument registers r0..r3.
const ARG_REGS: usize = 4;
/// Stack slots of the lanes' window this target reads and writes: all of it (16 words = 64 bytes of stack arguments).
const STACK_SLOTS: usize = LANE_STACK;

impl Guest {
    fn reg32(&self, offset: usize) -> u32 {
        // SAFETY: env is the live CPUArchState (hook contract); offset is a u32 field from its clang layout.
        unsafe { self.env.add(offset).cast::<u32>().read() }
    }

    fn set_reg32(&self, offset: usize, v: u32) {
        // SAFETY: as in reg32.
        unsafe { self.env.add(offset).cast::<u32>().write(v) }
    }

    /// r<i> (0..=15).
    pub fn r(&self, i: usize) -> u32 {
        assert!(i < 16, "r{i}");
        self.reg32(REGS + 4 * i)
    }

    pub fn set_r(&self, i: usize, v: u32) {
        assert!(i < 16, "r{i}");
        self.set_reg32(REGS + 4 * i, v);
    }

    /// Integer argument register `i` of a trap (r0..r3), zero-extended.
    pub fn arg(&self, i: usize) -> u64 {
        assert!(i < ARG_REGS, "argument {i}");
        u64::from(self.r(i))
    }

    /// The trap's answer register, r0: the low 32 bits of `v`.
    pub fn set_ret(&self, v: u64) {
        self.set_r(0, v as u32);
    }

    pub fn sp(&self) -> u64 {
        u64::from(self.r(SP))
    }

    /// Sets sp; `v` must lie below 4 GiB (the guest's address space).
    pub fn set_sp(&self, v: u64) {
        self.set_r(SP, u32::try_from(v).expect("arm guest sp above 4 GiB"));
    }

    /// The integer argument registers a CALL trap carries: r0..r3, then zero (the stack arguments are [`Guest::stack_word`]).
    pub(crate) fn trap_args(&self) -> [u64; 8] {
        std::array::from_fn(|i| if i < ARG_REGS { self.arg(i) } else { 0 })
    }

    /// The engine ABI lanes of a CALL or binding trap whose argument registers are `args` (r0..r3 in `args[0..4]`): `regs[0..4]`, the [`STACK_SLOTS`] words at sp in `stack` (a word past the guest's mapped memory is 0), no fp lane (softfp).
    #[cfg_attr(
        not(feature = "engine-so"),
        allow(dead_code, reason = "the C ABI's trap only")
    )]
    pub(crate) fn trap_lanes(&self, args: &[u64; 8]) -> AtlLanes {
        let mut l = AtlLanes::default();
        l.regs[..ARG_REGS].copy_from_slice(&args[..ARG_REGS]);
        for i in 0..STACK_SLOTS {
            l.stack[i] = self.stack_word(i).unwrap_or(0);
        }
        l
    }

    /// Writes a trap's answer beyond r0 (`ret[1]` into r1: the high word of a 64-bit or softfp double result) and returns r0's (`ret[0]`), which the trap dispatch sets.
    #[cfg_attr(
        not(feature = "engine-so"),
        allow(dead_code, reason = "the C ABI's trap only")
    )]
    pub(crate) fn answer_trap(&self, l: &AtlLanes) -> u64 {
        self.set_r(1, l.ret[1] as u32);
        l.ret[0]
    }

    /// Whether the CPU executes Thumb code (CPSR.T).
    pub fn thumb(&self) -> bool {
        self.reg32(THUMB) != 0
    }

    /// Stack word `i` (0..[`STACK_SLOTS`]): the 32-bit word at sp + 4i, where a call's fifth and later argument words sit. `None` when its address is not mapped for the guest. Identity: read at its host address.
    pub fn stack_word(&self, i: usize) -> Option<u64> {
        assert!(i < STACK_SLOTS, "stack word {i}");
        let addr = self.sp() + WORD * i as u64;
        if addr > *lib().guest_addr_max as u64 || self.page_flags(addr) & page::VALID == 0 {
            return None;
        }
        // SAFETY: the page is mapped for the guest, so its host address is mapped (identity); an unaligned read is fine for any alignment.
        Some(u64::from(unsafe {
            (self.host_addr(addr) as *const u32).read_unaligned()
        }))
    }

    /// The address of the svc's successor (r15 after the trap).
    pub fn pc(&self) -> u64 {
        u64::from(self.r(PC))
    }

    /// The guest thread pointer, TPIDRURO (what the guest's `mrc p15, 0, rX, c13, c0, 3` reads).
    pub fn tp(&self) -> u64 {
        self.reg(TPIDRURO)
    }

    pub fn set_tp(&self, v: u64) {
        self.set_reg(TPIDRURO, v);
    }

    /// Calls guest function `f` (bit 0 set = Thumb, the interworking address the guest's own `blx` takes) with `lanes` on thread pointer `tp`: r0..r3 from `regs[0..4]`, the [`STACK_SLOTS`] words at the callee's sp from `stack` (low 32 bits each). Answers r0 / r1 in `ret`; returns the thread pointer the call returned with. Nested as on aarch64; the whole CPU state is saved and restored around the call.
    pub fn call_lanes(&self, tp: u64, f: u64, lanes: &mut AtlLanes) -> u64 {
        let ret = ret_stub();
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let saved = self.save();
        self.set_tp(tp);
        for i in 0..ARG_REGS {
            self.set_r(i, lanes.regs[i] as u32);
        }
        // AAPCS32: sp 8-aligned at a public interface; below the interrupted frame with a margin for a stray store
        let sp = (self.sp() - 256) & !7;
        for (i, w) in lanes.stack[..STACK_SLOTS].iter().enumerate() {
            // SAFETY: sp..sp+64 lies in the CPU's guest stack below the interrupted sp, where the callee's own frame goes too; the host address is mapped (identity).
            unsafe {
                (self.host_addr(sp + WORD * i as u64) as *mut u32).write_unaligned(*w as u32)
            };
        }
        self.set_sp(sp);
        // the return stub's own bit 0 picks its mode, as `bx lr` reads it
        self.set_r(LR, ret as u32);
        self.set_r(PC, (f & !1) as u32);
        self.set_reg32(THUMB, (f & 1) as u32);
        self.set_reg32(CONDEXEC, 0);
        self.run();
        lanes.ret = [u64::from(self.r(0)), u64::from(self.r(1))];
        let tp_out = self.tp();
        self.restore(&saved);
        tp_out
    }
}
