// SPDX-License-Identifier: MIT
//! The engine C ABI as Rust types, mirroring `include/atl_engine.h` (the description). atl-qemu-engine's build script checks each layout and value here against that header through clang, so the two cannot drift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

/// Major version: a host refuses an engine with another major.
pub const ABI_MAJOR: u32 = 1;
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

/// Opaque engine handle (`atl_engine`).
pub enum AtlEngine {}

/// Opaque guest CPU of one attached host thread (`atl_cpu`).
pub enum AtlCpu {}

/// The trap callback of [`AtlHostV1`]: `kind` is a [`TrapKind`], `regs` points at eight u64.
pub type TrapFn =
    unsafe extern "C" fn(ctx: *mut c_void, kind: u32, id: u32, regs: *const u64) -> u64;

/// `atl_host_v1`: what the host hands the engine in `create`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtlHostV1 {
    /// ABI major the host speaks.
    pub major: u32,
    /// ABI minor the host speaks.
    pub minor: u32,
    /// Size of this struct as the host compiled it.
    pub size: u32,
    /// Passed back to `trap`.
    pub ctx: *mut c_void,
    /// The trap callback.
    pub trap: Option<TrapFn>,
}

/// `atl_engine_v1::create`.
pub type CreateFn = unsafe extern "C" fn(
    host: *const AtlHostV1,
    argc: c_int,
    argv: *const *mut c_char,
    out: *mut *mut AtlEngine,
) -> c_int;
/// `atl_engine_v1::destroy`.
pub type DestroyFn = unsafe extern "C" fn(engine: *mut AtlEngine);
/// `atl_engine_v1::map`.
pub type MapFn =
    unsafe extern "C" fn(engine: *mut AtlEngine, start: u64, len: u64, prot: u32) -> c_int;
/// `atl_engine_v1::attach`.
pub type AttachFn = unsafe extern "C" fn(engine: *mut AtlEngine, out: *mut *mut AtlCpu) -> c_int;
/// `atl_engine_v1::detach`.
pub type DetachFn = unsafe extern "C" fn(engine: *mut AtlEngine, cpu: *mut AtlCpu);
/// `atl_engine_v1::call`: `tp` in and out, `args` is four u64.
pub type CallFn = unsafe extern "C" fn(
    engine: *mut AtlEngine,
    cpu: *mut AtlCpu,
    tp: *mut u64,
    f: u64,
    args: *const u64,
) -> u64;

/// `atl_engine_v1`: the table the engine `.so` exports as `atl_engine_v1`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtlEngineV1 {
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
    /// Releases a CPU from [`AtlEngineV1::attach`].
    pub detach: Option<DetachFn>,
    /// Runs guest code on a CPU.
    pub call: Option<CallFn>,
}
