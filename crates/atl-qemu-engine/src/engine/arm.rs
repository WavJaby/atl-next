// SPDX-License-Identifier: MIT
//! arm-linux-user (arm32 guests, phase 4 P1): the guest registers and AAPCS32 guest calls, core registers only.
//! Lanes in this spike: r0..r3 = `x[0..4]` (low 32 bits), the stack arguments = `stack[i]`'s low 32 bits as consecutive words at sp; answer r0 in `x[0]`, r1 in `x[1]` (a 64-bit result's pair). No FP lane: Android armeabi-v7a is softfp, FP values travel in core registers. The guest-neutral lanes are engine ABI 4.0 (phase 4 P3).

use atl_engine_api::AtlLanes;

use super::{Env, Guest, lib, page, ret_stub};

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
        assert!(i < 4, "argument {i}");
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
        std::array::from_fn(|i| if i < 4 { self.arg(i) } else { 0 })
    }

    /// Whether the CPU executes Thumb code (CPSR.T).
    pub fn thumb(&self) -> bool {
        self.reg32(THUMB) != 0
    }

    /// Stack word `i` (0..8): the 32-bit word at sp + 4i, where a call's fifth and later integer arguments sit. `None` when its address is not mapped for the guest. Identity: read at its host address.
    pub fn stack_word(&self, i: usize) -> Option<u64> {
        assert!(i < 8, "stack word {i}");
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

    /// Calls guest function `f` (bit 0 set = Thumb, the interworking address the guest's own `blx` takes) with `lanes` on thread pointer `tp`: r0..r3 from `x[0..4]`, eight stack words at the callee's sp. Answers r0 / r1 in `x[0]` / `x[1]`; returns the thread pointer the call returned with. Nested as on aarch64; the whole CPU state is saved and restored around the call.
    pub fn call_lanes(&self, tp: u64, f: u64, lanes: &mut AtlLanes) -> u64 {
        let ret = ret_stub();
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let saved = self.save();
        self.set_tp(tp);
        for i in 0..4 {
            self.set_r(i, lanes.x[i] as u32);
        }
        // AAPCS32: sp 8-aligned at a public interface; below the interrupted frame with a margin for a stray store
        let sp = (self.sp() - 256) & !7;
        for (i, w) in lanes.stack.iter().enumerate() {
            // SAFETY: sp..sp+32 lies in the CPU's guest stack below the interrupted sp, where the callee's own frame goes too; the host address is mapped (identity).
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
        lanes.x[0] = u64::from(self.r(0));
        lanes.x[1] = u64::from(self.r(1));
        let tp_out = self.tp();
        self.restore(&saved);
        tp_out
    }
}
