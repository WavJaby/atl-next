// SPDX-License-Identifier: MIT
//! The engine C ABI as Rust types, mirroring `include/atl_engine.h` (the description). atl-qemu-engine's build script checks each layout and value here against that header through clang, so the two cannot drift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

/// Major version: a host refuses an engine with another major.
pub const ABI_MAJOR: u32 = 2;
/// Minor version: additions only, appended to a struct.
pub const ABI_MINOR: u32 = 1;

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
    /// BOOT: `regs[0]` = auxv, `regs[1]` = return stub; the answer is the boot executable's exit status.
    Boot = 0,
    /// A host call: `regs[0..8]` = x0..x7; the answer is x0.
    Call = 1,
    /// A binding thunk: `id` = slot; `regs[0..8]` = x0..x7; the answer is x0.
    Bind = 2,
    /// A value reported for a gate: `regs[0]` = x0; the answer is ignored.
    Report = 3,
}

/// Opaque engine handle (`atl_engine_handle`).
pub enum AtlEngineHandle {}

/// Opaque guest CPU of one attached host thread (`atl_cpu`).
pub enum AtlCpu {}

/// The trap callback of [`AtlHost`] for BOOT and REPORT: `kind` is a [`TrapKind`], `regs` points at eight u64.
pub type TrapFn =
    unsafe extern "C" fn(ctx: *mut c_void, kind: u32, id: u32, regs: *const u64) -> u64;

/// `atl_lanes`: the lanes of a host call or trap beyond the integer registers. `x` is x0..x7 and `v` the low 64 bits of v0..v7 (both in); `stack` the eight words at the guest's sp (in). The answer is `x[0]` and `v[0]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlLanes {
    /// x0..x7.
    pub x: [u64; 8],
    /// The low 64 bits of v0..v7.
    pub v: [u64; 8],
    /// The eight words at sp.
    pub stack: [u64; 8],
}

/// `atl_stack`: the guest stack of an attach ([`AtlEngine::attach_ex`]). `size` and `guard` in (rounded up to pages; 0 = default: 1 MiB, one page); `base` and `len` out, the usable stack (sp starts at `base + len`; the guard lies right below `base`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlStack {
    /// Requested stack bytes; 0 = default.
    pub size: u64,
    /// Requested guard bytes below the stack; 0 = default.
    pub guard: u64,
    /// Lowest usable byte.
    pub base: u64,
    /// Usable bytes.
    pub len: u64,
}

/// The trap callback of [`AtlHost`] for CALL and BIND: `lanes` is read, then answered in `x[0]` and `v[0]`.
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
}

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
/// `atl_engine::attach`.
pub type AttachFn =
    unsafe extern "C" fn(engine: *mut AtlEngineHandle, out: *mut *mut AtlCpu) -> c_int;
/// `atl_engine::attach_ex` (2.1): `stack` in and out.
pub type AttachExFn = unsafe extern "C" fn(
    engine: *mut AtlEngineHandle,
    stack: *mut AtlStack,
    out: *mut *mut AtlCpu,
) -> c_int;
/// `atl_engine::detach`.
pub type DetachFn = unsafe extern "C" fn(engine: *mut AtlEngineHandle, cpu: *mut AtlCpu);
/// `atl_engine::call_ex`: `tp` in and out; `lanes` in and out (the answer is `x[0]` and `v[0]`).
pub type CallExFn = unsafe extern "C" fn(
    engine: *mut AtlEngineHandle,
    cpu: *mut AtlCpu,
    tp: *mut u64,
    f: u64,
    lanes: *mut AtlLanes,
);

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
    /// Gives the calling thread a guest CPU.
    pub attach: Option<AttachFn>,
    /// Releases a CPU from [`AtlEngine::attach`].
    pub detach: Option<DetachFn>,
    /// Runs guest code with lanes on a CPU.
    pub call_ex: Option<CallExFn>,
    /// 2.1: gives the calling thread a guest CPU on a sized guest stack.
    pub attach_ex: Option<AttachExFn>,
}

/// Admits an engine table this host can drive, or refuses it by name. Refuses another major, a size below this host's table size (minor versions only append), and a minor below 1 or a table without `attach_ex`. The core loader and the gate both call it.
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
    // host and engine are built together, so no older-minor fallback: the host calls attach_ex (2.1) unconditionally
    if table.minor < 1 || table.attach_ex.is_none() {
        return Err(format!(
            "engine ABI {}.{} has no attach_ex, host needs 2.1",
            table.major, table.minor
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_major_1_engine_by_name() {
        let table = AtlEngine {
            major: 1,
            minor: 1,
            size: core::mem::size_of::<AtlEngine>() as u32,
            create: None,
            destroy: None,
            map: None,
            attach: None,
            detach: None,
            call_ex: None,
            attach_ex: None,
        };
        // "2.1" is the spec, typed here: ABI_MAJOR/ABI_MINOR are the code under test
        assert_eq!(
            check_engine_table(&table),
            Err("engine ABI 1.1 refused, host speaks 2.1".to_owned())
        );
    }

    #[test]
    fn refuses_a_2_0_engine_without_attach_ex_by_name() {
        let table = AtlEngine {
            major: 2,
            minor: 0,
            size: core::mem::size_of::<AtlEngine>() as u32,
            create: None,
            destroy: None,
            map: None,
            attach: None,
            detach: None,
            call_ex: None,
            attach_ex: None,
        };
        assert_eq!(
            check_engine_table(&table),
            Err("engine ABI 2.0 has no attach_ex, host needs 2.1".to_owned())
        );
    }
}
