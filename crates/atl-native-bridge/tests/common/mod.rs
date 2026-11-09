// SPDX-License-Identifier: MIT
//! Shared by the spike gates g1-g4: the host probe, the native-bridge build env, guest start.
//! Env (stages needing them print SKIP without): `ATL_NB=<out>/nb/x86_64`, `ATL_NB_LIBCXX=<out>/upstream/llvm-project/x86_64/libcxx-shared/build/lib` (the guest platform `libc++.so`: no `nb:` goal, pass 2 of `xtask native --native-bridge x86_64` builds it there, xtask runtimes.rs `shared_libcxx`), g2 also `ATL_NB_TESTLIB=<dir of the NDK-built libatl-jni-test.so>:<NDK sysroot usr/lib/x86_64-linux-android>` (a path list).
#![allow(dead_code, reason = "each gate uses a subset")]

use std::path::{Path, PathBuf};

use atl_native_bridge::engine::{Config, Guest, arch, image::Image};

pub const RTLD_NOW: usize = 2;

/// Host-side probe. Errors are static: on a guest thread pointer an allocation would itself crash.
pub fn probe(self0: libc::pthread_t) -> Result<(), &'static str> {
    // SAFETY: pthread_self has no preconditions.
    if unsafe { libc::pthread_self() } != self0 {
        return Err("pthread_self changed: the host thread pointer was not restored");
    }
    // SAFETY: this thread's errno slot.
    unsafe { *libc::__errno_location() = 4242 };
    // SAFETY: as above.
    if unsafe { *libc::__errno_location() } != 4242 {
        return Err("errno did not round-trip");
    }
    // SAFETY: plain malloc/free of a fresh block.
    unsafe {
        let p = libc::malloc(4096) as *mut u8;
        if p.is_null() {
            return Err("malloc failed");
        }
        p.write_bytes(0xa5, 4096);
        libc::free(p.cast());
    }
    let mut buf = [0u8; 32];
    // SAFETY: buf is 32 bytes, the format takes one int.
    let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"g %d".as_ptr(), 7) };
    if n != 3 || &buf[..3] != b"g 7" {
        return Err("snprintf wrong");
    }
    // stdio's lock and buffer live behind glibc TLS too
    // SAFETY: format without conversions.
    if unsafe { libc::printf(c"probe: host printf ok\n".as_ptr()) } < 0 {
        return Err("printf failed");
    }
    Ok(())
}

/// This thread's pthread handle.
pub fn self0() -> libc::pthread_t {
    // SAFETY: no preconditions.
    unsafe { libc::pthread_self() }
}

pub fn need(img: &Image, name: &str) -> Result<usize, String> {
    img.symbol(name)
        .ok_or_else(|| format!("linker64 lacks {name}"))
}

/// The native-bridge build named by the env, or None (the stage skips).
pub struct Nb {
    pub nb: PathBuf,
    pub libcxx: PathBuf,
    /// What an APK's `lib/<abi>/` holds: the test lib dir and the NDK's `libc++_shared.so` dir (an NDK app ships its own; the guest's platform `libc++.so` has another soname).
    pub testlib: Vec<PathBuf>,
}

pub fn nb_env() -> Option<Nb> {
    let nb = std::env::var_os("ATL_NB")?;
    let libcxx = std::env::var_os("ATL_NB_LIBCXX")?;
    Some(Nb {
        nb: nb.into(),
        libcxx: libcxx.into(),
        testlib: std::env::var_os("ATL_NB_TESTLIB")
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default(),
    })
}

/// The bionic provider every ART process has loaded (`libatl_bionic`): the engine binds the guest's property and abort-message stubs to it by name (engine/vdso.rs `PROVIDER`). xtask builds it next to the native-bridge output: `<out>/nb/<arch>` -> `<out>/crates/atl-bionic/lib`.
fn load_provider(nb: &Path) -> Result<(), String> {
    let lib = nb
        .ancestors()
        .nth(2)
        .ok_or("ATL_NB is not <out>/nb/<arch>")?
        .join("crates/atl-bionic/lib/libatl_bionic.so.0.0.0");
    let path =
        std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen of a NUL-terminated path; its constructors are the provider's own.
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        return Err(format!("dlopen {}: failed", lib.display()));
    }
    Ok(())
}

/// Loads the provider and starts the guest (linker init through the crossing); the test lib dir, if any, joins the guest search path.
pub fn start(env: &Nb) -> Result<Guest, String> {
    let nb = &env.nb;
    let mut library_path = vec![
        nb.join("libnative_bridge_guest_libc"),
        nb.join("libm"),
        nb.join("libdl"),
        nb.join("ld-android"),
        env.libcxx.clone(),
    ];
    library_path.extend(env.testlib.clone());
    let cfg = Config {
        linker: nb.join("native_bridge_guest_linker/linker64"),
        vdso: nb.join("libnative_bridge_vdso/libnative_bridge_vdso.so"),
        exe: nb.join("native_bridge_guest_app_process/app_process64"),
        library_path,
    };
    load_provider(nb)?;
    Guest::start(&cfg)
}

/// The host FS is this thread's again (`host` read before the crossing) and the probe passes; puts it back on failure so the next stage runs.
pub fn host_intact(
    g: &Guest,
    self0: libc::pthread_t,
    host: usize,
    after: &str,
) -> Result<(), String> {
    let fs = arch::fs_base();
    if fs != host {
        g.force_host_fs();
        return Err(format!("after {after}: FS {fs:#x}, host {host:#x}"));
    }
    probe(self0).map_err(|e| format!("after {after}: {e}"))
}

/// Prints the failures, returns the exit code.
pub fn verdict(
    gate: &str,
    results: impl IntoIterator<Item = Option<Result<(), String>>>,
) -> std::process::ExitCode {
    let mut failed = false;
    for e in results.into_iter().flatten().filter_map(Result::err) {
        eprintln!("{gate}: FAILED: {e}");
        failed = true;
    }
    if failed {
        return std::process::ExitCode::FAILURE;
    }
    println!("{gate}: ok");
    std::process::ExitCode::SUCCESS
}
