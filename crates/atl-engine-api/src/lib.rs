// SPDX-License-Identifier: MIT
//! The engine C ABI as Rust types, mirroring `include/atl_engine.h` (the description). atl-qemu-engine's build script checks each layout and value here against that header through clang, so the two cannot drift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

/// Major version: a host refuses an engine with another major.
pub const ABI_MAJOR: u32 = 3;
/// Minor version: additions only, appended to a struct.
pub const ABI_MINOR: u32 = 0;

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
/// `atl_engine::attach_ex`: `stack` in and out.
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
/// `atl_engine::fault`: `info` is the kernel's `siginfo_t *`, `ucontext` its `ucontext_t *`; true when the engine took the signal. Signal context; may not return (header).
pub type FaultFn =
    unsafe extern "C" fn(sig: c_int, info: *mut c_void, ucontext: *mut c_void) -> bool;
/// `atl_engine::end_call`: inside a CALL / BIND trap callback, makes the trap's return end the guest call it interrupted, with the trap's answer (header). 0, or `-EINVAL` outside a trap.
pub type EndCallFn = unsafe extern "C" fn(engine: *mut AtlEngineHandle) -> c_int;

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
}

/// Admits an engine table this host can drive, or refuses it by name. Refuses another major, a size below this host's table size (minor versions only append), a minor below this host's, and a table missing any entry. The core loader and the gate both call it.
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
    ]
    .into_iter()
    .filter_map(|(name, none)| none.then_some(name))
    .collect();
    if table.minor < ABI_MINOR || !missing.is_empty() {
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
        }
    }

    // "3.0" in the expected texts is the spec, typed here: ABI_MAJOR/ABI_MINOR are the code under test

    #[test]
    fn admits_a_full_3_0_engine() {
        assert_eq!(check_engine_table(&full(3, 0)), Ok(()));
    }

    #[test]
    fn refuses_a_2_3_engine_naming_the_major() {
        // a complete 2.3 table (the last 2.x minor) is still another major: 3.0 dropped `attach`
        assert_eq!(
            check_engine_table(&full(2, 3)),
            Err("engine ABI 2.3 refused, host speaks 3.0".to_owned())
        );
    }

    #[test]
    fn refuses_a_3_0_engine_without_fault_by_name() {
        let table = AtlEngine {
            fault: None,
            ..full(3, 0)
        };
        assert_eq!(
            check_engine_table(&table),
            Err("engine ABI 3.0 lacks [fault], host needs 3.0".to_owned())
        );
    }
}
