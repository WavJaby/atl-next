// SPDX-License-Identifier: MIT
//! The QEMU translator engine, spike shape (QEMU bridge plan Q2): QEMU's aarch64 linux-user core as a library in the host process, identity addresses (`-B 0`).
//!
//! Start: [`start`] spawns a boot helper thread that dlopens the spike library (QEMU's PIC objects linked `-shared`, `main` aliased `atl_qemu_user_main`) and runs QEMU's own `main` on a static guest ELF: QEMU's init, loader and `cpu_loop`, nothing replayed here.
//! Hand-over: the guest's bootstrap `svc #BOOT` (x0 = payload, x1 = its `svc #RET` stub) reaches P1's hook ([`trap`]), which runs the BOOT callback on the helper, hands its answer to `start`'s caller and parks the helper for the life of the process. QEMU's `main` never returns to a library caller, so the boot thread is spent; no QEMU change.
//! Guest calls: in a trap callback on the trapping CPU ([`Guest::call`]), or on any host thread through its own CPU ([`Booted::attach`], a `cpu_copy` of the boot CPU with linux-user's per-thread state, `attach`); `svc #CALL` and binding traps reach the one [`OnTrap`] callback on the thread that trapped.
//! Faults: [`signal`] puts its handler in front of QEMU's at BOOT (QEMU's `signal_init` overwrites earlier handlers and never chains them): guest faults go to QEMU's, host-only faults to the handler that was installed before `start`; with libsigchain linked, ART's special handlers run before it.
//! Guest threads: each registers its per-thread QEMU state (`helper_retaddr`) at its first trap (BOOT, or THREAD in [`trap`]) or at attach; [`signal`] classifies every fault on the thread that took it.
#![cfg_attr(
    not(feature = "qemu"),
    doc = "Built without feature `qemu`: no engine code (needs `ATL_QEMU_BUILD`, see Cargo.toml)."
)]

#[cfg(feature = "qemu")]
#[allow(
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all,
    reason = "generated: atl-bridgegen layout constants named after QEMU's records and fields"
)]
mod layout {
    include!(concat!(env!("OUT_DIR"), "/qemu_layout.rs"));
}

/// atl-bridgegen's aarch64 thunk constants (build.rs writes them for every build; the decode and the pool read one value).
#[allow(
    dead_code,
    reason = "generated: the thunk ABI's constants, not all of them used by the decode"
)]
mod thunk {
    include!(concat!(env!("OUT_DIR"), "/aarch64_constants.rs"));
}

#[cfg(feature = "qemu")]
mod attach;
#[cfg(feature = "qemu")]
mod engine;
#[cfg(feature = "qemu")]
pub mod signal;
#[cfg(feature = "qemu")]
pub mod trap;

#[cfg(feature = "qemu")]
pub use attach::Cpu;
#[cfg(feature = "engine-so")]
pub use engine::{Booted, Guest, OnTrap, Trap, page};
#[cfg(all(feature = "qemu", not(feature = "engine-so")))]
pub use engine::{Booted, Guest, OnTrap, Trap, page, start};

/// The engine C ABI's facts as clang reads them from `atl_engine.h` (build.rs): the Rust mirror in atl-engine-api must state the same values.
#[cfg(feature = "qemu")]
mod abi_layout {
    include!(concat!(env!("OUT_DIR"), "/engine_abi_layout.rs"));
}

#[cfg(feature = "qemu")]
mod abi_check {
    use core::mem::{offset_of, size_of};

    use atl_engine_api::{AtlEngineV1, AtlHostV1, PROT_EXEC, PROT_READ, PROT_WRITE, TrapKind};

    use super::abi_layout as l;

    // clang types plain int macros i32 and sizeof/offsetof u64: both sides go through u64 (every value here is non-negative)
    const _: () = assert!(atl_engine_api::ABI_MAJOR as u64 == l::ATL_ABI_MAJOR as u64);
    const _: () = assert!(atl_engine_api::ABI_MINOR as u64 == l::ATL_ABI_MINOR as u64);
    const _: () = assert!(PROT_READ as u64 == l::ATL_VALUE_PROT_READ as u64);
    const _: () = assert!(PROT_WRITE as u64 == l::ATL_VALUE_PROT_WRITE as u64);
    const _: () = assert!(PROT_EXEC as u64 == l::ATL_VALUE_PROT_EXEC as u64);
    const _: () = assert!(TrapKind::Boot as u64 == l::ATL_VALUE_TRAP_BOOT as u64);
    const _: () = assert!(TrapKind::Call as u64 == l::ATL_VALUE_TRAP_CALL as u64);
    const _: () = assert!(TrapKind::Bind as u64 == l::ATL_VALUE_TRAP_BIND as u64);
    const _: () = assert!(TrapKind::Report as u64 == l::ATL_VALUE_TRAP_REPORT as u64);
    const _: () = assert!(size_of::<AtlHostV1>() as u64 == l::ATL_SIZEOF_HOST_V1);
    const _: () = assert!(offset_of!(AtlHostV1, major) as u64 == l::ATL_OFFSET_HOST_MAJOR);
    const _: () = assert!(offset_of!(AtlHostV1, minor) as u64 == l::ATL_OFFSET_HOST_MINOR);
    const _: () = assert!(offset_of!(AtlHostV1, size) as u64 == l::ATL_OFFSET_HOST_SIZE);
    const _: () = assert!(offset_of!(AtlHostV1, ctx) as u64 == l::ATL_OFFSET_HOST_CTX);
    const _: () = assert!(offset_of!(AtlHostV1, trap) as u64 == l::ATL_OFFSET_HOST_TRAP);
    const _: () = assert!(size_of::<AtlEngineV1>() as u64 == l::ATL_SIZEOF_ENGINE_V1);
    const _: () = assert!(offset_of!(AtlEngineV1, major) as u64 == l::ATL_OFFSET_ENGINE_MAJOR);
    const _: () = assert!(offset_of!(AtlEngineV1, minor) as u64 == l::ATL_OFFSET_ENGINE_MINOR);
    const _: () = assert!(offset_of!(AtlEngineV1, size) as u64 == l::ATL_OFFSET_ENGINE_SIZE);
    const _: () = assert!(offset_of!(AtlEngineV1, create) as u64 == l::ATL_OFFSET_ENGINE_CREATE);
    const _: () = assert!(offset_of!(AtlEngineV1, destroy) as u64 == l::ATL_OFFSET_ENGINE_DESTROY);
    const _: () = assert!(offset_of!(AtlEngineV1, map) as u64 == l::ATL_OFFSET_ENGINE_MAP);
    const _: () = assert!(offset_of!(AtlEngineV1, attach) as u64 == l::ATL_OFFSET_ENGINE_ATTACH);
    const _: () = assert!(offset_of!(AtlEngineV1, detach) as u64 == l::ATL_OFFSET_ENGINE_DETACH);
    const _: () = assert!(offset_of!(AtlEngineV1, call) as u64 == l::ATL_OFFSET_ENGINE_CALL);
}

/// The C ABI (QEMU bridge plan Q5-1a), built into the engine `.so` only: the table `atl_engine_v1` and its functions, contract in `include/atl_engine.h`. One engine per process.
#[cfg(feature = "engine-so")]
mod capi {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::sync::OnceLock;

    use atl_engine_api::{
        ABI_MAJOR, ABI_MINOR, AtlCpu, AtlEngine, AtlEngineV1, AtlHostV1, PROT_EXEC, PROT_READ,
        PROT_WRITE, TrapFn, TrapKind,
    };

    use crate::engine::{set_pages, start_linked};
    use crate::{Booted, Cpu, Guest, Trap, page};

    /// The engine handle: this static's address, the only one there is.
    static ENGINE: u8 = 0;
    /// The host, set by create before QEMU runs (BOOT's callbacks need it).
    static HOST: OnceLock<HostRef> = OnceLock::new();
    /// The booted engine, set by create after BOOT; attach needs it.
    static BOOTED: OnceLock<Booted> = OnceLock::new();

    struct HostRef {
        ctx: *mut c_void,
        trap: TrapFn,
    }

    // SAFETY: the ABI's contract: the host keeps ctx and trap valid until the process ends, and only reads this struct after create.
    unsafe impl Send for HostRef {}
    // SAFETY: as for Send; the struct is never mutated after create.
    unsafe impl Sync for HostRef {}

    /// Writes one line to stderr: the ABI has no error text channel.
    fn note(msg: &str) {
        let line = format!("atl-engine: {msg}\n");
        // SAFETY: the buffer is valid for its length; a failed write has nowhere else to report.
        let _ = unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
    }

    /// A misuse the ABI cannot report by return value (a bad engine or CPU on a void or uint64 entry): fail closed.
    fn fail(msg: &str) -> ! {
        note(msg);
        std::process::abort()
    }

    fn is_engine(engine: *mut AtlEngine) -> bool {
        engine as usize == &raw const ENGINE as usize
    }

    /// The engine's `OnTrap` for the C host: BOOT, CALL, BIND and REPORT as the ABI's kinds, with the guest's x0..x7.
    fn host_trap(g: Guest, t: Trap) -> u64 {
        let host = HOST
            .get()
            .expect("atl-engine: create sets the host before BOOT");
        let (kind, id, regs) = match t {
            Trap::Boot(x0) => (TrapKind::Boot, 0, [x0, g.x(1), 0, 0, 0, 0, 0, 0]),
            Trap::Call(a) => (TrapKind::Call, 0, a),
            Trap::Bind { id, args } => (TrapKind::Bind, id, args),
            Trap::Report(x0) => (TrapKind::Report, 0, [x0, 0, 0, 0, 0, 0, 0, 0]),
        };
        // SAFETY: host.trap and host.ctx are the host's, valid by the ABI (HostRef); regs holds the eight u64 the call reads.
        unsafe { (host.trap)(host.ctx, kind as u32, id, regs.as_ptr()) }
    }

    unsafe extern "C" fn create(
        host: *const AtlHostV1,
        argc: c_int,
        argv: *const *mut c_char,
        out: *mut *mut AtlEngine,
    ) -> c_int {
        // SAFETY: the host passes its struct for the duration of the call (the ABI).
        let Some(h) = (unsafe { host.as_ref() }) else {
            return -libc::EINVAL;
        };
        if h.major != ABI_MAJOR
            || (h.size as usize) < core::mem::size_of::<AtlHostV1>()
            || argc < 1
            || argv.is_null()
            || out.is_null()
        {
            return -libc::EINVAL;
        }
        let Some(trap) = h.trap else {
            return -libc::EINVAL;
        };
        let mut owned: Vec<String> = Vec::new();
        for i in 0..argc as usize {
            // SAFETY: argv has argc entries (the ABI), each a NUL-terminated string.
            let p = unsafe { *argv.add(i) };
            if p.is_null() {
                return -libc::EINVAL;
            }
            // SAFETY: p is a NUL-terminated string (above).
            let Ok(s) = unsafe { CStr::from_ptr(p) }.to_str() else {
                return -libc::EINVAL;
            };
            owned.push(s.to_owned());
        }
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        if HOST.set(HostRef { ctx: h.ctx, trap }).is_err() {
            return -libc::EEXIST;
        }
        // SAFETY: out is non-null (checked); the handle is published before QEMU runs, so BOOT's callbacks reach the engine through it.
        unsafe { *out = (&raw const ENGINE).cast_mut().cast() };
        match start_linked(&args, host_trap) {
            Ok(booted) => {
                let _ = BOOTED.set(booted);
                0
            }
            Err(e) => {
                note(&e);
                -libc::EIO
            }
        }
    }

    unsafe extern "C" fn destroy(engine: *mut AtlEngine) {
        if !is_engine(engine) {
            fail("destroy: not this process's engine");
        }
        // the booted engine lives until the process ends: QEMU's main never returns, and its helper parks in BOOT
    }

    unsafe extern "C" fn map(engine: *mut AtlEngine, start: u64, len: u64, prot: u32) -> c_int {
        let known = PROT_READ | PROT_WRITE | PROT_EXEC;
        if !is_engine(engine) || BOOTED.get().is_none() || len == 0 || prot & !known != 0 {
            return -libc::EINVAL;
        }
        let (Ok(start), Ok(len)) = (usize::try_from(start), usize::try_from(len)) else {
            return -libc::EINVAL;
        };
        match set_pages(start, len, prot as i32 | page::VALID, 0) {
            Ok(()) => 0,
            Err(e) => {
                note(&e);
                -libc::EIO
            }
        }
    }

    unsafe extern "C" fn attach(engine: *mut AtlEngine, out: *mut *mut AtlCpu) -> c_int {
        let Some(booted) = BOOTED.get() else {
            return -libc::EINVAL;
        };
        if !is_engine(engine) || out.is_null() {
            return -libc::EINVAL;
        }
        match booted.attach() {
            Ok(cpu) => {
                // SAFETY: out is non-null (checked); detach frees this box, on the attaching thread.
                unsafe { *out = Box::into_raw(Box::new(cpu)).cast() };
                0
            }
            Err(e) => {
                note(&e);
                -libc::EIO
            }
        }
    }

    unsafe extern "C" fn detach(engine: *mut AtlEngine, cpu: *mut AtlCpu) {
        if !is_engine(engine) || cpu.is_null() {
            fail("detach: not this process's engine, or no CPU");
        }
        // SAFETY: cpu came from attach (a Box<Cpu>) and is detached once, on the thread that attached it (the ABI).
        drop(unsafe { Box::from_raw(cpu.cast::<Cpu>()) });
    }

    unsafe extern "C" fn call(
        engine: *mut AtlEngine,
        cpu: *mut AtlCpu,
        tp: *mut u64,
        f: u64,
        args: *const u64,
    ) -> u64 {
        if !is_engine(engine) || cpu.is_null() || tp.is_null() || args.is_null() {
            fail("call: not this process's engine, or no CPU, thread pointer or arguments");
        }
        // SAFETY: cpu came from attach and lives until detach, on this thread (the ABI).
        let cpu = unsafe { &*cpu.cast::<Cpu>() };
        // SAFETY: args points at four u64 and tp is writable (the ABI).
        let (a, t) = unsafe { (std::slice::from_raw_parts(args, 4), *tp) };
        let (x0, tp_out) = cpu.guest().call_on(t, f, a);
        // SAFETY: tp is writable (the ABI).
        unsafe { *tp = tp_out };
        x0
    }

    /// The table the engine `.so` exports (version script: `atl_engine_v1` is the only global symbol of its kind).
    #[cfg(atl_qe_abi_major_next)]
    const BREAK_MAJOR: u32 = 1;
    #[cfg(not(atl_qe_abi_major_next))]
    const BREAK_MAJOR: u32 = 0;

    #[unsafe(no_mangle)]
    #[allow(non_upper_case_globals, reason = "the exported ABI name")]
    pub static atl_engine_v1: AtlEngineV1 = AtlEngineV1 {
        major: ABI_MAJOR + BREAK_MAJOR,
        minor: ABI_MINOR,
        size: core::mem::size_of::<AtlEngineV1>() as u32,
        create: Some(create),
        destroy: Some(destroy),
        map: Some(map),
        attach: Some(attach),
        detach: Some(detach),
        call: Some(call),
    };
}
