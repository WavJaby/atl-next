// SPDX-License-Identifier: MIT
//! The QEMU translator engine, spike shape (QEMU bridge plan Q2): QEMU's aarch64 linux-user core as a library in the host process, identity addresses (`-B 0`).
//!
//! Start: [`start`] spawns a boot helper thread that dlopens the spike library (QEMU's PIC objects linked `-shared`, `main` aliased `atl_qemu_user_main`) and runs QEMU's own `main` on a static guest ELF: QEMU's init, loader and `cpu_loop`, nothing replayed here.
//! Hand-over: the guest's bootstrap `svc #BOOT` (x0 = payload, x1 = its `svc #RET` stub) reaches P1's hook ([`trap`]), which runs the BOOT callback on the helper, hands its answer to `start`'s caller and parks the helper for the life of the process. QEMU's `main` never returns to a library caller, so the boot thread is spent; no QEMU change.
//! Guest calls: in a trap callback on the trapping CPU ([`Guest::call`]), or on any host thread through its own CPU ([`Booted::attach`], a `cpu_copy` of the boot CPU with linux-user's per-thread state, `attach`); `svc #CALL` and binding traps reach the one [`OnTrap`] callback on the thread that trapped.
//! Faults: [`signal::special`] is a sigchain special handler (ART registers it through NativeBridge `getSignalHandler`, the spike harness as ART does): guest faults go to QEMU's handler; everything else passes on to the process's user action, which BOOT restores to the one from before QEMU (`signal_init` overwrites it and never chains).
//! Guest threads: each registers its per-thread QEMU state (`helper_retaddr`) at attach, the BOOT thread at BOOT, a raw guest clone at its THREAD trap ([`trap`]); [`signal`] classifies every fault on the thread that took it.
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

    use atl_engine_api::{
        AtlEngine, AtlHost, AtlLanes, AtlStack, PROT_EXEC, PROT_READ, PROT_WRITE, TrapKind,
    };

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
    const _: () = assert!(size_of::<AtlHost>() as u64 == l::ATL_SIZEOF_HOST);
    const _: () = assert!(offset_of!(AtlHost, major) as u64 == l::ATL_OFFSET_HOST_MAJOR);
    const _: () = assert!(offset_of!(AtlHost, minor) as u64 == l::ATL_OFFSET_HOST_MINOR);
    const _: () = assert!(offset_of!(AtlHost, size) as u64 == l::ATL_OFFSET_HOST_SIZE);
    const _: () = assert!(offset_of!(AtlHost, ctx) as u64 == l::ATL_OFFSET_HOST_CTX);
    const _: () = assert!(offset_of!(AtlHost, trap) as u64 == l::ATL_OFFSET_HOST_TRAP);
    const _: () = assert!(offset_of!(AtlHost, trap_ex) as u64 == l::ATL_OFFSET_HOST_TRAP_EX);
    const _: () = assert!(size_of::<AtlLanes>() as u64 == l::ATL_SIZEOF_LANES);
    const _: () = assert!(offset_of!(AtlLanes, x) as u64 == l::ATL_OFFSET_LANES_X);
    const _: () = assert!(offset_of!(AtlLanes, v) as u64 == l::ATL_OFFSET_LANES_V);
    const _: () = assert!(offset_of!(AtlLanes, stack) as u64 == l::ATL_OFFSET_LANES_STACK);
    const _: () = assert!(size_of::<AtlEngine>() as u64 == l::ATL_SIZEOF_ENGINE);
    const _: () = assert!(offset_of!(AtlEngine, major) as u64 == l::ATL_OFFSET_ENGINE_MAJOR);
    const _: () = assert!(offset_of!(AtlEngine, minor) as u64 == l::ATL_OFFSET_ENGINE_MINOR);
    const _: () = assert!(offset_of!(AtlEngine, size) as u64 == l::ATL_OFFSET_ENGINE_SIZE);
    const _: () = assert!(offset_of!(AtlEngine, create) as u64 == l::ATL_OFFSET_ENGINE_CREATE);
    const _: () = assert!(offset_of!(AtlEngine, destroy) as u64 == l::ATL_OFFSET_ENGINE_DESTROY);
    const _: () = assert!(offset_of!(AtlEngine, map) as u64 == l::ATL_OFFSET_ENGINE_MAP);
    const _: () = assert!(offset_of!(AtlEngine, detach) as u64 == l::ATL_OFFSET_ENGINE_DETACH);
    const _: () = assert!(offset_of!(AtlEngine, call_ex) as u64 == l::ATL_OFFSET_ENGINE_CALL_EX);
    const _: () =
        assert!(offset_of!(AtlEngine, attach_ex) as u64 == l::ATL_OFFSET_ENGINE_ATTACH_EX);
    const _: () = assert!(offset_of!(AtlEngine, fault) as u64 == l::ATL_OFFSET_ENGINE_FAULT);
    const _: () = assert!(offset_of!(AtlEngine, end_call) as u64 == l::ATL_OFFSET_ENGINE_END_CALL);
    const _: () = assert!(size_of::<AtlStack>() as u64 == l::ATL_SIZEOF_STACK);
    const _: () = assert!(offset_of!(AtlStack, size) as u64 == l::ATL_OFFSET_STACK_SIZE);
    const _: () = assert!(offset_of!(AtlStack, guard) as u64 == l::ATL_OFFSET_STACK_GUARD);
    const _: () = assert!(offset_of!(AtlStack, base) as u64 == l::ATL_OFFSET_STACK_BASE);
    const _: () = assert!(offset_of!(AtlStack, len) as u64 == l::ATL_OFFSET_STACK_LEN);
}

/// The C ABI (QEMU bridge plan Q5-1a), built into the engine `.so` only: the table `atl_engine` and its functions, contract in `include/atl_engine.h`. One engine per process.
#[cfg(feature = "engine-so")]
mod capi {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::sync::OnceLock;

    use atl_engine_api::{
        ABI_MAJOR, ABI_MINOR, AtlCpu, AtlEngine, AtlEngineHandle, AtlHost, AtlLanes, AtlStack,
        PROT_EXEC, PROT_READ, PROT_WRITE, TrapExFn, TrapFn, TrapKind,
    };

    use crate::engine::{boot_env, set_pages, start_linked};
    use crate::{Cpu, Guest, Trap, page};

    /// The engine handle: this static's address, the only one there is.
    static ENGINE: u8 = 0;
    /// The host, set by create before QEMU runs (BOOT's callbacks need it).
    static HOST: OnceLock<HostRef> = OnceLock::new();

    struct HostRef {
        ctx: *mut c_void,
        trap: TrapFn,
        trap_ex: TrapExFn,
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

    fn is_engine(engine: *mut AtlEngineHandle) -> bool {
        engine as usize == &raw const ENGINE as usize
    }

    /// BOOT and REPORT through the trap callback: eight registers in, x0 out.
    fn host_word(host: &HostRef, kind: TrapKind, regs: [u64; 8]) -> u64 {
        // SAFETY: host.trap and host.ctx are the host's, valid by the ABI (HostRef); regs holds the eight u64 the call reads.
        unsafe { (host.trap)(host.ctx, kind as u32, 0, regs.as_ptr()) }
    }

    /// The engine's `OnTrap` for the C host. BOOT and REPORT go through `trap`; CALL and BIND through `trap_ex` with the guest's x0..x7, v0..v7 and eight stack words (a word past the guest's mapped memory is 0). The host's v0 answer is written to the guest here; its x0 is the trap's result (set by the trap dispatch).
    fn host_trap(g: Guest, t: Trap) -> u64 {
        let host = HOST
            .get()
            .expect("atl-engine: create sets the host before BOOT");
        let (kind, id, x) = match t {
            Trap::Boot(x0) => {
                return host_word(host, TrapKind::Boot, [x0, g.x(1), 0, 0, 0, 0, 0, 0]);
            }
            Trap::Report(x0) => {
                return host_word(host, TrapKind::Report, [x0, 0, 0, 0, 0, 0, 0, 0]);
            }
            Trap::Call(a) => (TrapKind::Call, 0, a),
            Trap::Bind { id, args } => (TrapKind::Bind, id, args),
        };
        let mut lanes = AtlLanes {
            x,
            v: std::array::from_fn(|i| g.v(i)),
            stack: std::array::from_fn(|i| g.stack_word(i).unwrap_or(0)),
        };
        // SAFETY: host.trap_ex and host.ctx are the host's, valid by the ABI (HostRef); lanes is a live AtlLanes that the host reads and answers.
        unsafe { (host.trap_ex)(host.ctx, kind as u32, id, &mut lanes) };
        g.set_v(0, lanes.v[0]);
        lanes.x[0]
    }

    unsafe extern "C" fn create(
        host: *const AtlHost,
        boot_exe: *const c_char,
        out: *mut *mut AtlEngineHandle,
    ) -> c_int {
        // SAFETY: the host passes its struct for the duration of the call (the ABI).
        let Some(h) = (unsafe { host.as_ref() }) else {
            return -libc::EINVAL;
        };
        if h.major != ABI_MAJOR
            || (h.size as usize) < core::mem::size_of::<AtlHost>()
            || boot_exe.is_null()
            || out.is_null()
        {
            return -libc::EINVAL;
        }
        let (Some(trap), Some(trap_ex)) = (h.trap, h.trap_ex) else {
            return -libc::EINVAL;
        };
        // SAFETY: boot_exe is a NUL-terminated string (the ABI; checked non-null above).
        let Ok(exe) = unsafe { CStr::from_ptr(boot_exe) }.to_str() else {
            return -libc::EINVAL;
        };
        if HOST
            .set(HostRef {
                ctx: h.ctx,
                trap,
                trap_ex,
            })
            .is_err()
        {
            return -libc::EEXIST;
        }
        // SAFETY: out is non-null (checked); the handle is published before QEMU runs, so BOOT's callbacks reach the engine through it.
        unsafe { *out = (&raw const ENGINE).cast_mut().cast() };
        match start_linked(exe, host_trap) {
            Ok(_) => 0,
            Err(e) => {
                note(&e);
                -libc::EIO
            }
        }
    }

    unsafe extern "C" fn destroy(engine: *mut AtlEngineHandle) {
        if !is_engine(engine) {
            fail("destroy: not this process's engine");
        }
        // the booted engine lives until the process ends: QEMU's main never returns, and its helper parks in BOOT
    }

    unsafe extern "C" fn map(
        engine: *mut AtlEngineHandle,
        start: u64,
        len: u64,
        prot: u32,
    ) -> c_int {
        let known = PROT_READ | PROT_WRITE | PROT_EXEC;
        if !is_engine(engine) || boot_env().is_none() || len == 0 || prot & !known != 0 {
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

    unsafe extern "C" fn attach_ex(
        engine: *mut AtlEngineHandle,
        stack: *mut AtlStack,
        out: *mut *mut AtlCpu,
    ) -> c_int {
        let Some(env) = boot_env() else {
            return -libc::EINVAL;
        };
        if !is_engine(engine) || out.is_null() || stack.is_null() {
            return -libc::EINVAL;
        }
        // SAFETY: stack is non-null and a live atl_stack (the ABI).
        let stack = unsafe { &mut *stack };
        let (Ok(size), Ok(guard)) = (usize::try_from(stack.size), usize::try_from(stack.guard))
        else {
            return -libc::EINVAL;
        };
        match crate::attach::attach(env, size, guard) {
            Ok(cpu) => {
                stack.base = cpu.stack_base() as u64;
                stack.len = (cpu.stack_top() - cpu.stack_base()) as u64;
                stack.guard = cpu.stack_guard() as u64;
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

    unsafe extern "C" fn detach(engine: *mut AtlEngineHandle, cpu: *mut AtlCpu) {
        if !is_engine(engine) || cpu.is_null() {
            fail("detach: not this process's engine, or no CPU");
        }
        // SAFETY: cpu came from attach (a Box<Cpu>) and is detached once, on the thread that attached it (the ABI).
        drop(unsafe { Box::from_raw(cpu.cast::<Cpu>()) });
    }

    unsafe extern "C" fn call_ex(
        engine: *mut AtlEngineHandle,
        cpu: *mut AtlCpu,
        tp: *mut u64,
        f: u64,
        lanes: *mut AtlLanes,
    ) {
        if !is_engine(engine) || cpu.is_null() || tp.is_null() || lanes.is_null() {
            fail("call_ex: not this process's engine, or no CPU, thread pointer or lanes");
        }
        // SAFETY: cpu came from attach and lives until detach, on this thread (the ABI).
        let cpu = unsafe { &*cpu.cast::<Cpu>() };
        // SAFETY: lanes is a live AtlLanes that the call reads and answers, and tp is writable (the ABI).
        let (lanes, t) = unsafe { (&mut *lanes, *tp) };
        let tp_out = cpu.guest().call_lanes(t, f, lanes);
        // SAFETY: tp is writable (the ABI).
        unsafe { *tp = tp_out };
    }

    /// The engine's part of a host SIGSEGV / SIGBUS special handler ([`crate::signal::special`]).
    unsafe extern "C" fn fault(sig: c_int, info: *mut c_void, uc: *mut c_void) -> bool {
        crate::signal::special(sig, info.cast(), uc)
    }

    /// 2.3: the innermost running trap of this thread ends its guest call on return ([`crate::trap::end_call`]).
    unsafe extern "C" fn end_call(engine: *mut AtlEngineHandle) -> c_int {
        if is_engine(engine) && crate::trap::end_call() {
            0
        } else {
            -libc::EINVAL
        }
    }

    /// The table the engine `.so` exports (version script: `atl_engine` is the only global symbol of its kind).
    #[cfg(atl_qe_abi_major_next)]
    const BREAK_MAJOR: u32 = 1;
    #[cfg(not(atl_qe_abi_major_next))]
    const BREAK_MAJOR: u32 = 0;

    #[unsafe(no_mangle)]
    #[allow(non_upper_case_globals, reason = "the exported ABI name")]
    pub static atl_engine: AtlEngine = AtlEngine {
        major: ABI_MAJOR + BREAK_MAJOR,
        minor: ABI_MINOR,
        size: core::mem::size_of::<AtlEngine>() as u32,
        create: Some(create),
        destroy: Some(destroy),
        map: Some(map),
        attach_ex: Some(attach_ex),
        detach: Some(detach),
        call_ex: Some(call_ex),
        fault: Some(fault),
        end_call: Some(end_call),
    };
}
