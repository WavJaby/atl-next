// SPDX-License-Identifier: MIT
//! The engine C ABI as Rust types, mirroring `include/atl_engine.h` (the description). atl-qemu-engine's build script checks each layout and value here against that header through clang, so the two cannot drift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

/// Major version: a host refuses an engine with another major.
pub const ABI_MAJOR: u32 = 4;
/// Minor version: additions only, appended to a struct.
pub const ABI_MINOR: u32 = 2;

/// `ATL_LANE_REGS`: lanes of [`AtlLanes::regs`].
pub const LANE_REGS: usize = 16;
/// `ATL_LANE_FP`: lanes of [`AtlLanes::fp`].
pub const LANE_FP: usize = 16;
/// `ATL_LANE_STACK`: lanes of [`AtlLanes::stack`], the stack window.
pub const LANE_STACK: usize = 16;

/// `ATL_PROT_READ`.
pub const PROT_READ: u32 = 0x1;
/// `ATL_PROT_WRITE`.
pub const PROT_WRITE: u32 = 0x2;
/// `ATL_PROT_EXEC`.
pub const PROT_EXEC: u32 = 0x4;

/// `enum atl_trap_kind`: what reached the host from the guest.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    /// BOOT: `regs[0]` = auxv, `regs[1]` = return stub (the first two argument registers); the answer is the boot executable's exit status.
    Boot = 0,
    /// A host call ([`TrapExFn`]): lanes in; the answer is `ret` and `fp[0]`.
    Call = 1,
    /// A binding thunk ([`TrapExFn`]): `id` = slot; lanes in; the answer is `ret` and `fp[0]`.
    Bind = 2,
    /// A value reported for a gate: `regs[0]` = the first argument register; the answer is ignored.
    Report = 3,
}

/// Opaque engine handle (`atl_engine_handle`).
pub enum AtlEngineHandle {}

/// Opaque guest CPU of one attached host thread (`atl_cpu`).
pub enum AtlCpu {}

/// The trap callback of [`AtlHost`] for BOOT and REPORT: `kind` is a [`TrapKind`], `regs` points at eight u64.
pub type TrapFn =
    unsafe extern "C" fn(ctx: *mut c_void, kind: u32, id: u32, regs: *const u64) -> u64;

/// `atl_lanes`: the guest-neutral lanes of a call or trap; each bank holds one calling convention's slots in order, zero-extended, unused lanes 0 (which lanes a guest uses: header). The answer is `ret` and `fp[0]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlLanes {
    /// The integer argument registers (aarch64 x0..x7, arm r0..r3).
    pub regs: [u64; LANE_REGS],
    /// The low 64 bits of the fp argument registers (aarch64 v0..v7; arm softfp none); out: `fp[0]` = the fp result.
    pub fp: [u64; LANE_FP],
    /// The stack window: one guest stack slot per lane at the callee's sp (aarch64 8 bytes, arm 4).
    pub stack: [u64; LANE_STACK],
    /// Out: the integer result registers (aarch64 x0, x1; arm r0, r1).
    pub ret: [u64; 2],
}

/// `atl_stack`: the guest stack of an attach ([`AtlEngine::attach_ex`]). `size` and `guard` in (rounded up to pages; 0 = default: 1 MiB, one page); `base` and `len` out, the usable stack (sp starts at `base + len`; the guard lies right below `base`); `guard` out too, the guard actually made.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlStack {
    /// Requested stack bytes; 0 = default.
    pub size: u64,
    /// Requested guard bytes below the stack, 0 = default; out: the guard made.
    pub guard: u64,
    /// Lowest usable byte.
    pub base: u64,
    /// Usable bytes.
    pub len: u64,
}

/// The trap callback of [`AtlHost`] for CALL and BIND: `lanes` is read, then answered in `ret` and `fp[0]`.
pub type TrapExFn =
    unsafe extern "C" fn(ctx: *mut c_void, kind: u32, id: u32, lanes: *mut AtlLanes);

/// `atl_host`: what the host hands the engine in `create`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtlHost {
    /// ABI major the host speaks.
    pub major: u32,
    /// ABI minor the host speaks.
    pub minor: u32,
    /// Size of this struct as the host compiled it.
    pub size: u32,
    /// Passed back to `trap` and `trap_ex`.
    pub ctx: *mut c_void,
    /// The trap callback for BOOT and REPORT.
    pub trap: Option<TrapFn>,
    /// The trap callback for CALL and BIND.
    pub trap_ex: Option<TrapExFn>,
    /// 3.1: reports a guest fault before the engine's guest delivery (signal context; header).
    pub fault_report: Option<FaultReportFn>,
}

/// The fault-report callback of [`AtlHost`]: `info` / `ucontext` are the kernel's `siginfo_t *` / `ucontext_t *`. Signal context, async-signal-safe, must return (header).
pub type FaultReportFn =
    unsafe extern "C" fn(ctx: *mut c_void, sig: c_int, info: *mut c_void, ucontext: *mut c_void);

/// `atl_engine::create`: `boot_exe` is a NUL-terminated path; QEMU's flags are the engine's own.
pub type CreateFn = unsafe extern "C" fn(
    host: *const AtlHost,
    boot_exe: *const c_char,
    out: *mut *mut AtlEngineHandle,
) -> c_int;
/// `atl_engine::destroy`.
pub type DestroyFn = unsafe extern "C" fn(engine: *mut AtlEngineHandle);
/// `atl_engine::map`.
pub type MapFn =
    unsafe extern "C" fn(engine: *mut AtlEngineHandle, start: u64, len: u64, prot: u32) -> c_int;
/// `atl_engine::attach_ex`: `stack` in and out.
pub type AttachExFn = unsafe extern "C" fn(
    engine: *mut AtlEngineHandle,
    stack: *mut AtlStack,
    out: *mut *mut AtlCpu,
) -> c_int;
/// `atl_engine::detach`.
pub type DetachFn = unsafe extern "C" fn(engine: *mut AtlEngineHandle, cpu: *mut AtlCpu);
/// `atl_engine::call_ex`: `tp` in and out; `lanes` in and out (the answer is `ret` and `fp[0]`).
pub type CallExFn = unsafe extern "C" fn(
    engine: *mut AtlEngineHandle,
    cpu: *mut AtlCpu,
    tp: *mut u64,
    f: u64,
    lanes: *mut AtlLanes,
);
/// `atl_engine::fault`: `info` is the kernel's `siginfo_t *`, `ucontext` its `ucontext_t *`; true when the engine took the signal. Signal context; may not return (header).
pub type FaultFn =
    unsafe extern "C" fn(sig: c_int, info: *mut c_void, ucontext: *mut c_void) -> bool;
/// `atl_engine::end_call`: inside a CALL / BIND trap callback, makes the trap's return end the guest call it interrupted, with the trap's answer (header). 0, or `-EINVAL` outside a trap.
pub type EndCallFn = unsafe extern "C" fn(engine: *mut AtlEngineHandle) -> c_int;
/// `atl_engine::map_window` (4.1): `out` receives the window's address.
pub type MapWindowFn =
    unsafe extern "C" fn(engine: *mut AtlEngineHandle, len: u64, prot: u32, out: *mut u64) -> c_int;
/// `atl_engine::unmap_window` (4.2): gives back part or all of a [`MapWindowFn`] window.
pub type UnmapWindowFn =
    unsafe extern "C" fn(engine: *mut AtlEngineHandle, start: u64, len: u64) -> c_int;

/// `atl_engine`: the table the engine `.so` exports as `atl_engine`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtlEngine {
    /// ABI major the engine speaks.
    pub major: u32,
    /// ABI minor the engine speaks.
    pub minor: u32,
    /// Size of this struct as the engine compiled it.
    pub size: u32,
    /// Starts QEMU; see the header.
    pub create: Option<CreateFn>,
    /// Releases the engine handle.
    pub destroy: Option<DestroyFn>,
    /// Marks a guest range as mapped.
    pub map: Option<MapFn>,
    /// Gives the calling thread a guest CPU on a sized guest stack.
    pub attach_ex: Option<AttachExFn>,
    /// Releases a CPU from [`AtlEngine::attach_ex`].
    pub detach: Option<DetachFn>,
    /// Runs guest code with lanes on a CPU.
    pub call_ex: Option<CallExFn>,
    /// The engine's part of a host SIGSEGV / SIGBUS handler (a sigchain special handler).
    pub fault: Option<FaultFn>,
    /// Ends the guest call a trap interrupted when the trap returns (thread exit).
    pub end_call: Option<EndCallFn>,
    /// 4.1: maps a fresh window the guest can address (below 4 GiB for a 32-bit guest).
    pub map_window: Option<MapWindowFn>,
    /// 4.2: gives back a window (host unmap + guest page marks cleared).
    pub unmap_window: Option<UnmapWindowFn>,
}

/// Admits an engine table this host can drive, or refuses it by name. Refuses another major, a size below this host's table size (minor versions only append, so this also refuses an older minor), and a table missing any entry. The core loader and the gate both call it.
pub fn check_engine_table(table: &AtlEngine) -> Result<(), String> {
    if table.major != ABI_MAJOR {
        return Err(format!(
            "engine ABI {}.{} refused, host speaks {ABI_MAJOR}.{ABI_MINOR}",
            table.major, table.minor
        ));
    }
    let host_size = core::mem::size_of::<AtlEngine>();
    if (table.size as usize) < host_size {
        return Err(format!(
            "engine ABI {}.{} table size {} below the host's {host_size}",
            table.major, table.minor, table.size
        ));
    }
    // host and engine are built together, so no older-minor fallback: the host calls every entry unconditionally
    let missing: Vec<&str> = [
        ("create", table.create.is_none()),
        ("destroy", table.destroy.is_none()),
        ("map", table.map.is_none()),
        ("attach_ex", table.attach_ex.is_none()),
        ("detach", table.detach.is_none()),
        ("call_ex", table.call_ex.is_none()),
        ("fault", table.fault.is_none()),
        ("end_call", table.end_call.is_none()),
        ("map_window", table.map_window.is_none()),
        ("unmap_window", table.unmap_window.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, none)| none.then_some(name))
    .collect();
    // no minor comparison: minors only append entries, so an older minor's table is refused by the size check above
    if !missing.is_empty() {
        return Err(format!(
            "engine ABI {}.{} lacks [{}], host needs {ABI_MAJOR}.{ABI_MINOR}",
            table.major,
            table.minor,
            missing.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn create(
        _: *const AtlHost,
        _: *const c_char,
        _: *mut *mut AtlEngineHandle,
    ) -> c_int {
        0
    }
    extern "C" fn destroy(_: *mut AtlEngineHandle) {}
    extern "C" fn map(_: *mut AtlEngineHandle, _: u64, _: u64, _: u32) -> c_int {
        0
    }
    extern "C" fn attach_ex(
        _: *mut AtlEngineHandle,
        _: *mut AtlStack,
        _: *mut *mut AtlCpu,
    ) -> c_int {
        0
    }
    extern "C" fn detach(_: *mut AtlEngineHandle, _: *mut AtlCpu) {}
    extern "C" fn call_ex(
        _: *mut AtlEngineHandle,
        _: *mut AtlCpu,
        _: *mut u64,
        _: u64,
        _: *mut AtlLanes,
    ) {
    }
    extern "C" fn fault(_: c_int, _: *mut c_void, _: *mut c_void) -> bool {
        false
    }
    extern "C" fn end_call(_: *mut AtlEngineHandle) -> c_int {
        0
    }
    extern "C" fn map_window(_: *mut AtlEngineHandle, _: u64, _: u32, _: *mut u64) -> c_int {
        0
    }
    extern "C" fn unmap_window(_: *mut AtlEngineHandle, _: u64, _: u64) -> c_int {
        0
    }

    /// Every entry set, at `major.minor`.
    fn full(major: u32, minor: u32) -> AtlEngine {
        AtlEngine {
            major,
            minor,
            size: core::mem::size_of::<AtlEngine>() as u32,
            create: Some(create),
            destroy: Some(destroy),
            map: Some(map),
            attach_ex: Some(attach_ex),
            detach: Some(detach),
            call_ex: Some(call_ex),
            fault: Some(fault),
            end_call: Some(end_call),
            map_window: Some(map_window),
            unmap_window: Some(unmap_window),
        }
    }

    /// The host version as refusals name it: the ABI constants are the version spec (bumped by hand with the header), not output of `check_engine_table`, so the tests follow each bump.
    fn host() -> String {
        format!("{ABI_MAJOR}.{ABI_MINOR}")
    }

    #[test]
    fn admits_a_full_4_0_engine() {
        assert_eq!(check_engine_table(&full(4, 0)), Ok(()));
    }

    #[test]
    fn refuses_a_3_1_engine_naming_the_major() {
        // a complete 3.1 table (the last 3.x minor) is still another major: 4.0 reshaped `atl_lanes` (guest-neutral banks)
        assert_eq!(
            check_engine_table(&full(3, 1)),
            Err(format!("engine ABI 3.1 refused, host speaks {}", host()))
        );
    }

    #[test]
    fn refuses_a_4_0_engine_without_fault_by_name() {
        let table = AtlEngine {
            fault: None,
            ..full(4, 0)
        };
        assert_eq!(
            check_engine_table(&table),
            Err(format!(
                "engine ABI 4.0 lacks [fault], host needs {}",
                host()
            ))
        );
    }

    /// atl_lanes per the header: regs[16], fp[16], stack[16], ret[2], u64 each, in that order.
    #[test]
    fn lanes_layout() {
        use core::mem::{offset_of, size_of};
        assert_eq!(
            (
                offset_of!(AtlLanes, fp),
                offset_of!(AtlLanes, stack),
                offset_of!(AtlLanes, ret),
                size_of::<AtlLanes>()
            ),
            (16 * 8, 32 * 8, 48 * 8, 50 * 8)
        );
    }
}
