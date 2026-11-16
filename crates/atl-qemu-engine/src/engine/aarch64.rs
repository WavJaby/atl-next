// SPDX-License-Identifier: MIT
//! aarch64-linux-user: the guest registers, AAPCS64 guest calls and the command line of the engine `.so`.

use atl_engine_api::AtlLanes;

use super::{Env, Guest, lib, page, ret_stub};

/// argv[0] of the engine `.so`'s QEMU command line.
#[cfg(feature = "engine-so")]
pub(super) const ARGV0: &str = "qemu-aarch64";

/// The guest CPU QEMU emulates (`-cpu`): what an app reads from HWCAP, /proc/cpuinfo and `mrs` follows the reference device class (behaviour decision 1), today's Android arm64 phone = ARMv9.2 Cortex-A720/X4 with SVE not exposed.
/// This QEMU has no A720/X model; cortex-a710 is the closest (vs the phone: + mte, - afp dgh ecv evtstrm), `sve=off` as the phone. QEMU's default `max` adds SVE/SME/MOPS/GCS/FP8 and more no phone exposes.
/// `midr` = QEMU's own `max` value (implementer 0 "software", part 'Q'): no claim to be Arm silicon an app might key errata or tuning on.
/// Tied to the pinned QEMU: a bump that adds a closer model, or a property this string names going away, changes it (gate (z) of tests/spike.rs pins the result).
#[cfg(all(feature = "engine-so", not(atl_qe_cpu_max)))]
pub(super) const GUEST_CPU: &[&str] = &["-cpu", "cortex-a710,sve=off,midr=0x000f0510"];
// break cpu-max (gate z): no `-cpu`, QEMU's default `max`
#[cfg(all(feature = "engine-so", atl_qe_cpu_max))]
pub(super) const GUEST_CPU: &[&str] = &[];

/// Byte offset of TPIDR_EL0 in `CPUArchState` (clang's `offsetof`, a `size_t` the generator types u64; an offset inside a 79 KB record fits any usize).
const TPIDR_EL0: usize = crate::layout::ATL_OFFSET_TPIDR_EL0 as usize;
const _: () = assert!(TPIDR_EL0 + 8 <= Env::SIZE);
/// `vfp.zregs` (v<i> is zregs[i]; its low 64 bits are `d[0]`): offsets and stride from clang (build.rs `TARGETS`).
const ZREGS: usize = crate::layout::ATL_OFFSET_ZREGS as usize;
const ZREG_SIZE: usize = crate::layout::ATL_SIZEOF_ZREG as usize;
const ZREG_D: usize = crate::layout::ATL_OFFSET_ZREG_D as usize;
const _: () = assert!(ZREGS + 32 * ZREG_SIZE <= Env::SIZE);

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

    /// Stack word `i` (0..8) at the guest's sp: where a call's stack arguments sit. `None` when the word's address is not mapped for the guest (QEMU bridge plan R-5: the trap reads all eight, and past a shallow thread's stack top they are refused, not read). Identity: a mapped word is read at its host address.
    pub fn stack_word(&self, i: usize) -> Option<u64> {
        assert!(i < 8, "stack word {i}");
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

    /// Calls guest function `f` with `lanes` on thread pointer `tp`: x0..x7, v0..v7 (low 64 bits) and the eight stack words at the callee's sp. The answer is `lanes.x[0]` and `lanes.v[0]`; returns the thread pointer the call returned with. Nested: may run from inside a trap callback, which may itself call again.
    /// The whole CPU state is saved and restored around the call (the thread pointer included), so the interrupted guest code resumes exactly where its trap left it.
    pub fn call_lanes(&self, tp: u64, f: u64, lanes: &mut AtlLanes) -> u64 {
        let ret = ret_stub();
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let saved = self.save();
        self.set_tp(tp);
        for (i, a) in lanes.x.iter().enumerate() {
            self.set_x(i, *a);
        }
        // break `call-v` (qemu-abi gate, check d): the FP lanes are not set, so v0..v7 keep the CPU's values
        if !cfg!(atl_qe_call_v) {
            for (i, v) in lanes.v.iter().enumerate() {
                self.set_v(i, *v);
            }
        }
        // AAPCS64: sp 16-aligned; below the interrupted frame (no red zone on aarch64, the margin is for a stray store)
        let sp = (self.x(31) - 256) & !15;
        // break `call-stack` (qemu-abi gate, check d): the stack words are not written, so the callee's stack arguments are what lies at sp
        if !cfg!(atl_qe_call_stack) {
            for (i, w) in lanes.stack.iter().enumerate() {
                // SAFETY: sp..sp+64 lies in the CPU's guest stack below the interrupted sp, where the callee's own frame goes too; the host address is mapped (identity).
                unsafe { (self.host_addr(sp + 8 * i as u64) as *mut u64).write_unaligned(*w) };
            }
        }
        self.set_x(31, sp);
        self.set_x(30, ret);
        self.set_reg(Env::pc, f);
        self.run();
        lanes.x[0] = self.x(0);
        lanes.v[0] = self.v(0);
        let tp_out = self.tp();
        self.restore(&saved);
        tp_out
    }
}
