// SPDX-License-Identifier: MIT
//! `atl-engine-cpuinfo <isa>`: runs guest ISA `isa`'s engine `.so` (atl-android-paths `engine_library`) on its cpuinfo probe (`engine_cpuinfo`) in this process. The probe copies the guest's `/proc/cpuinfo` to stdout and exits the process with 0, so `create` never returns on success.
//! Run by atld once per start inside the sandbox, where both paths are the installed ones; stdout is atld's cached guest cpuinfo, which the broker binds over translated apps' /proc/cpuinfo.
//! The engine answers the guest's open of /proc/cpuinfo with its own formatter (QEMU linux-user `open_cpuinfo`: one block per host online CPU, hwcaps of the engine's cpu model), so the file is exactly what guest code reads, on this host; one formatter, no copy of it here.
//! Exit status: the probe's (0, or 1 on a guest syscall error); 2 when the engine cannot be loaded or `create` fails (the reason on stderr).

use std::convert::Infallible;
use std::ffi::{CString, c_void};
use std::io::Write;
use std::process::ExitCode;

use atl_engine_api::{ABI_MAJOR, ABI_MINOR, AtlEngine, AtlHost, AtlLanes, check_engine_table};

/// The probe raises no trap; a trap means the engine ran something else.
unsafe extern "C" fn no_trap(_: *mut c_void, kind: u32, _: u32, _: *const u64) -> u64 {
    let _ = writeln!(
        std::io::stderr(),
        "atl-engine-cpuinfo: unexpected trap kind {kind}"
    );
    std::process::exit(2)
}

/// As [`no_trap`], for CALL and BIND.
unsafe extern "C" fn no_trap_ex(ctx: *mut c_void, kind: u32, id: u32, _: *mut AtlLanes) {
    // SAFETY: no_trap reads none of its pointers and never returns.
    unsafe { no_trap(ctx, kind, id, std::ptr::null()) };
}

fn run(isa: &str) -> Result<Infallible, String> {
    let library = atl_android_paths::engine_library(isa);
    let probe = CString::new(atl_android_paths::engine_cpuinfo(isa)).map_err(|e| e.to_string())?;
    // SAFETY: the engine's initialisers are QEMU's own (QOM type registration); the library is never closed (forgotten below), so its table and code outlive every use.
    let lib = unsafe {
        libloading::os::unix::Library::open(
            Some(&library),
            libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
        )
    }
    .map_err(|e| format!("dlopen {library}: {e}"))?;
    // SAFETY: atl_engine is the engine's table, a static of type AtlEngine (atl_engine.h).
    let table: AtlEngine = unsafe {
        **lib
            .get::<*const AtlEngine>(b"atl_engine\0")
            .map_err(|e| format!("{library}: no atl_engine: {e}"))?
    };
    std::mem::forget(lib);
    check_engine_table(&table).map_err(|e| format!("{library}: {e}"))?;
    let create = table
        .create
        .ok_or_else(|| format!("{library}: engine table has no create"))?;
    let host = AtlHost {
        major: ABI_MAJOR,
        minor: ABI_MINOR,
        size: std::mem::size_of::<AtlHost>() as u32,
        ctx: std::ptr::null_mut(),
        // the engine requires both callbacks
        trap: Some(no_trap),
        trap_ex: Some(no_trap_ex),
    };
    let mut engine = std::ptr::null_mut();
    // SAFETY: host and probe live for the call (the ABI); engine is an out parameter. The probe's exit_group ends the process inside this call.
    let rc = unsafe { create(&host, probe.as_ptr(), &mut engine) };
    Err(format!(
        "engine create returned {rc}: the probe {} ended without exiting the process",
        atl_android_paths::engine_cpuinfo(isa)
    ))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [isa] = args.as_slice() else {
        let _ = writeln!(std::io::stderr(), "usage: atl-engine-cpuinfo <isa>");
        return ExitCode::from(2);
    };
    // run returns only when the probe did not end the process
    let Err(why) = run(isa);
    let _ = writeln!(std::io::stderr(), "atl-engine-cpuinfo: {why}");
    ExitCode::from(2)
}
