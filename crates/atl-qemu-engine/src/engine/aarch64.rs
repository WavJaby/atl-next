// SPDX-License-Identifier: MIT
//! aarch64-linux-user: the guest registers, AAPCS64 guest calls and the command line of the engine `.so`.

use atl_engine_api::{AtlLanes, LANE_STACK};

use super::{Env, Guest, lib, page, ret_stub};

/// argv[0] of the engine `.so`'s QEMU command line.
#[cfg(feature = "engine-so")]
pub(super) const ARGV0: &str = "qemu-aarch64";

/// The guest CPU QEMU emulates (`-cpu`), two capability tiers picked by [`guest_cpu`]: what an app reads from HWCAP, /proc/cpuinfo and `mrs` follows the reference device class (behaviour decision 1), today's Android arm64 phone = ARMv9.2 Cortex-A720/X4 with SVE not exposed.
/// This QEMU has no A720/X model; cortex-a710 is the closest (vs the phone: + mte, - afp dgh ecv evtstrm), `sve=off` as the phone. QEMU's default `max` adds SVE/SME/MOPS/GCS/FP8 and more no phone exposes.
/// `midr` = QEMU's own `max` value (implementer 0 "software", part 'Q'): no claim to be Arm silicon an app might key errata or tuning on.
/// Tied to the pinned QEMU: a bump that adds a closer model, or a property this string names going away, changes it (gate (z) of tests/spike.rs pins the result of both tiers).
#[cfg(feature = "engine-so")]
const CPU_LSE2: &str = "cortex-a710,sve=off,midr=0x000f0510";
/// The tier of an engine without 16-byte atomics: cortex-a76 = an upstream model with no LSE2 (ID_AA64MMFR2.AT 0), so an aligned `ldp`/`stp` needs only 8-byte atomicity.
/// Under LSE2 such an engine, once a second guest thread exists, leaves the TB at each one to run it alone (`cpu_loop_exit_atomic`: stop the world); function prologues do this. Splitting it 8+8 while claiming LSE2 would tear silently.
/// Cost: ARMv8.2 features only (no BTI, PAuth, I8MM, BF16, ...), as an A76-class phone. No SVE in the model, so no `sve=off`.
#[cfg(feature = "engine-so")]
const CPU_BASE: &str = "cortex-a76,midr=0x000f0510";

#[cfg(feature = "engine-so")]
unsafe extern "C" {
    /// `HAVE_ATOMIC128_RW` of this engine's QEMU (src/host_atomic.c).
    fn atl_host_atomic128_rw() -> bool;
}

/// The `-cpu` value for this engine, None = QEMU's default. The predicate is QEMU's own, not a host feature list: the one `load_atom_16` / `store_atom_16` (accel/tcg/ldst_atomicity.c.inc) test before giving up on a parallel 16-byte access.
/// Valid before QEMU's `main`: a build constant, or QEMU's `cpuinfo`, which an initialiser fills.
#[cfg(feature = "engine-so")]
pub(crate) fn guest_cpu() -> Option<&'static str> {
    // break cpu-max (gate z): no `-cpu`, QEMU's default `max`
    if cfg!(atl_qe_cpu_max) {
        return None;
    }
    // SAFETY: no arguments, no preconditions; reads a constant or QEMU's initialised `cpuinfo`.
    let atomic16 = unsafe { atl_host_atomic128_rw() };
    // break cpu-tier (gate z): the predicate inverted
    Some(if atomic16 != cfg!(atl_qe_cpu_tier) {
        CPU_LSE2
    } else {
        CPU_BASE
    })
}

/// Byte offset of TPIDR_EL0 in `CPUArchState` (clang's `offsetof`, a `size_t` the generator types u64; an offset inside a 79 KB record fits any usize).
const TPIDR_EL0: usize = crate::layout::ATL_OFFSET_TPIDR_EL0 as usize;
const _: () = assert!(TPIDR_EL0 + 8 <= Env::SIZE);
/// `vfp.zregs` (v<i> is zregs[i]; its low 64 bits are `d[0]`): offsets and stride from clang (build.rs `TARGETS`).
const ZREGS: usize = crate::layout::ATL_OFFSET_ZREGS as usize;
const ZREG_SIZE: usize = crate::layout::ATL_SIZEOF_ZREG as usize;
const ZREG_D: usize = crate::layout::ATL_OFFSET_ZREG_D as usize;
const _: () = assert!(ZREGS + 32 * ZREG_SIZE <= Env::SIZE);
/// Stack slots of the lanes' window this target reads and writes: the whole bank, sixteen 8-byte words (as arm.rs). Eight refused Candy Crush Saga's 17-integer JNI method (nine AAPCS64 stack words past x0..x7).
const STACK_SLOTS: usize = LANE_STACK;

impl Guest {
    /// x<i> (0..=30), or sp for 31.
    pub fn x(&self, i: usize) -> u64 {
        assert!(i < 32, "x{i}");
        self.reg(Env::xregs + 8 * i)
    }

    pub fn set_x(&self, i: usize, v: u64) {
        assert!(i < 32, "x{i}");
        self.set_reg(Env::xregs + 8 * i, v);
    }

    /// Integer argument register `i` of a trap (x0..x7).
    pub fn arg(&self, i: usize) -> u64 {
        assert!(i < 8, "argument {i}");
        self.x(i)
    }

    /// The trap's answer register, x0.
    pub fn set_ret(&self, v: u64) {
        self.set_x(0, v);
    }

    /// The guest stack pointer.
    pub fn sp(&self) -> u64 {
        self.x(31)
    }

    pub fn set_sp(&self, v: u64) {
        self.set_x(31, v);
    }

    /// The integer argument registers a CALL or binding trap carries: x0..x7.
    pub(crate) fn trap_args(&self) -> [u64; 8] {
        std::array::from_fn(|i| self.x(i))
    }

    /// v<i> (0..=31): its low 64 bits, the lane a `double` argument or result uses.
    pub fn v(&self, i: usize) -> u64 {
        assert!(i < 32, "v{i}");
        self.reg(ZREGS + ZREG_SIZE * i + ZREG_D)
    }

    /// Sets v<i>'s low 64 bits and clears the high 64, as a write to d<i> does.
    pub fn set_v(&self, i: usize, v: u64) {
        assert!(i < 32, "v{i}");
        let at = ZREGS + ZREG_SIZE * i + ZREG_D;
        self.set_reg(at, v);
        self.set_reg(at + 8, 0);
    }

    /// The engine ABI lanes of a CALL or binding trap whose integer argument registers are `args` (x0..x7): v0..v7 into `fp[0..8]`, the [`STACK_SLOTS`] words at sp into `stack` (a word past the guest's mapped memory is 0).
    #[cfg_attr(
        not(feature = "engine-so"),
        allow(dead_code, reason = "the C ABI's trap only")
    )]
    pub(crate) fn trap_lanes(&self, args: &[u64; 8]) -> AtlLanes {
        let mut l = AtlLanes::default();
        l.regs[..8].copy_from_slice(args);
        for i in 0..8 {
            l.fp[i] = self.v(i);
        }
        for i in 0..STACK_SLOTS {
            l.stack[i] = self.stack_word(i).unwrap_or(0);
        }
        l
    }

    /// Writes a trap's answer beyond x0 (`fp[0]` into v0) and returns x0's (`ret[0]`), which the trap dispatch sets.
    #[cfg_attr(
        not(feature = "engine-so"),
        allow(dead_code, reason = "the C ABI's trap only")
    )]
    pub(crate) fn answer_trap(&self, l: &AtlLanes) -> u64 {
        self.set_v(0, l.fp[0]);
        l.ret[0]
    }

    /// Stack word `i` (0..[`STACK_SLOTS`]) at the guest's sp: where a call's stack arguments sit. `None` when the word's address is not mapped for the guest (QEMU bridge plan R-5: the trap reads them all, and past a shallow thread's stack top they are refused, not read). Identity: a mapped word is read at its host address.
    pub fn stack_word(&self, i: usize) -> Option<u64> {
        assert!(i < STACK_SLOTS, "stack word {i}");
        let addr = self.x(31) + 8 * i as u64;
        if addr > *lib().guest_addr_max as u64 || self.page_flags(addr) & page::VALID == 0 {
            return None;
        }
        // SAFETY: the page is mapped for the guest, so its host address is mapped (identity); an unaligned read is fine for any alignment.
        Some(unsafe { (self.host_addr(addr) as *const u64).read_unaligned() })
    }

    pub fn pc(&self) -> u64 {
        self.reg(Env::pc)
    }

    /// The guest thread pointer, TPIDR_EL0 (what the guest's `msr tpidr_el0` and linux-user's `cpu_set_tls` write).
    pub fn tp(&self) -> u64 {
        self.reg(TPIDR_EL0)
    }

    pub fn set_tp(&self, v: u64) {
        self.set_reg(TPIDR_EL0, v);
    }

    /// Calls guest function `f` with `lanes` on thread pointer `tp`: x0..x7 from `regs[0..8]`, v0..v7 (low 64 bits) from `fp[0..8]`, the [`STACK_SLOTS`] words at the callee's sp from `stack`. The answer is `ret` (x0, x1) and `fp[0]` (v0); returns the thread pointer the call returned with. Nested: may run from inside a trap callback, which may itself call again.
    /// The whole CPU state is saved and restored around the call (the thread pointer included), so the interrupted guest code resumes exactly where its trap left it.
    pub fn call_lanes(&self, tp: u64, f: u64, lanes: &mut AtlLanes) -> u64 {
        let ret = ret_stub();
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let saved = self.save();
        self.set_tp(tp);
        for (i, a) in lanes.regs[..8].iter().enumerate() {
            self.set_x(i, *a);
        }
        // break `call-v` (qemu-abi gate, check d): the FP lanes are not set, so v0..v7 keep the CPU's values
        if !cfg!(atl_qe_call_v) {
            for (i, v) in lanes.fp[..8].iter().enumerate() {
                self.set_v(i, *v);
            }
        }
        // AAPCS64: sp 16-aligned; below the interrupted frame (no red zone on aarch64, the margin is for a stray store)
        let sp = (self.x(31) - 256) & !15;
        // break `call-stack` (qemu-abi gate, check d): the stack words are not written, so the callee's stack arguments are what lies at sp
        if !cfg!(atl_qe_call_stack) {
            for (i, w) in lanes.stack[..STACK_SLOTS].iter().enumerate() {
                // SAFETY: sp..sp+128 (16 words) lies within the 256-byte margin in the CPU's guest stack below the interrupted sp, where the callee's own frame goes too; the host address is mapped (identity).
                unsafe { (self.host_addr(sp + 8 * i as u64) as *mut u64).write_unaligned(*w) };
            }
        }
        self.set_x(31, sp);
        self.set_x(30, ret);
        self.set_reg(Env::pc, f);
        self.run();
        lanes.ret = [self.x(0), self.x(1)];
        lanes.fp[0] = self.v(0);
        let tp_out = self.tp();
        self.restore(&saved);
        tp_out
    }
}
