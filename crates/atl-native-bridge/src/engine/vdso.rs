// SPDX-License-Identifier: MIT
//! Host side of NBS `libnative_bridge_vdso.so` (soname `linux-vdso.so.1`), the guest's door to the host: the guest linker takes it as the kernel vdso (auxv AT_SYSINFO_EHDR, bionic linker_main.cpp `add_vdso`), and the guest libc imports `native_bridge_intercept_symbol` from it.
//! How an entry, a linker hook or a stub reaches its host function is the process engine's (`Active::patch_entry`, `patch_hook`, `patch_site`, `is_stub`): x86_64 rewrites `st_value` and movabs immediates, QEMU writes binding thunks in place. The vdso file stays unpatched.
//! `native_bridge_intercept_symbol` binds a guest libc stub (NBS `interceptable_functions.h`) by writing the thunk of its binding (bindings.rs, keyed (library, name)) into the stub. A stub the table lacks, or whose host symbol is absent, stays unbound: it jumps to 0 if called.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use super::active;
use super::image::Image;

/// Stubs bound to a host function / left unbound, since process start.
pub static BOUND: AtomicU32 = AtomicU32::new(0);
pub static UNBOUND: AtomicU32 = AtomicU32::new(0);
/// Names left unbound, for the startup report.
pub static UNBOUND_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

extern "C" fn intercept_symbol(addr: *mut c_void, library: *const c_char, symbol: *const c_char) {
    // SAFETY: NUL-terminated literals of the guest's stub tables (INIT_INTERCEPTABLE_STUB_FUNCTION).
    let (lib, name) = unsafe { (CStr::from_ptr(library), CStr::from_ptr(symbol)) };
    let name = name.to_string_lossy();
    let lib = lib.to_string_lossy();
    let addr = addr as usize;
    let engine = active();
    if !engine.is_stub(addr) {
        // the exported entry (`jmp name_stub`) reaches the host through the stub; a stub variable (NBS `DEFINE_INTERCEPTABLE_STUB_VARIABLE`, `uintptr_t name;`) takes the host variable's value (break `sles_var_skip`: left 0)
        if let Some(value) = super::bindings::variable(&lib, &name) {
            #[cfg(not(atl_nb_sles_var_skip))]
            // SAFETY: NBS INIT_INTERCEPTABLE_STUB_VARIABLE passes `&name`, the guest's own writable `uintptr_t`.
            unsafe {
                *(addr as *mut usize) = value;
            }
            #[cfg(atl_nb_sles_var_skip)]
            let _ = value;
            BOUND.fetch_add(1, Ordering::Relaxed);
        } else {
            // a function's exported entry, reported before its stub: the address the guest imports the row by (proc.rs `row_address`)
            super::proc::note_exported(&lib, &name, addr);
        }
        return;
    }
    // every stub resolves by (library, name) in the one binding table (bindings.rs), never by the global scope
    let host = super::bindings::target(&lib, &name).and_then(|t| t.callee());
    let host_addr = host.map(|(h, _)| h);
    // deliberate break of the app gate's glesPbuffer (`ATL_NB_BREAK=gl_direct`): glClear's stub jumps to the host front itself, skipping the crossing -> host code runs on the guest thread pointer
    #[cfg(atl_nb_gl_direct)]
    let bound = if name == "glClear" {
        host.map(|(h, _)| h)
    } else {
        host.and_then(|(h, ret)| engine.bind_result(h, ret))
    };
    #[cfg(not(atl_nb_gl_direct))]
    let bound = host.and_then(|(h, ret)| engine.bind_result(h, ret));
    match bound {
        Some(thunk) if engine.patch_site(addr, thunk) => {
            BOUND.fetch_add(1, Ordering::Relaxed);
            if let Some(h) = host_addr {
                super::proc::note_bound(&lib, &name, h);
            }
        }
        _ => {
            UNBOUND.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut v) = UNBOUND_NAMES.lock() {
                v.push(format!("{lib}:{name}"));
            }
        }
    }
}

use super::unsupported;

extern "C" fn post_init() {
    unsupported(
        "guest native_bridge_post_init: the guest main executable ran (g1 never jumps to it)",
    )
}

extern "C" fn call_guest() {
    unsupported("guest native_bridge_call_guest: not supported yet")
}

/// `native_bridge_trace(format, ...)`: guest debug trace, dropped.
extern "C" fn trace() {}

/// `__native_bridge_get_host_pthread()`: the bionic thread record the guest main thread reuses (NBS linker `__libc_init_main_thread_early`: `__init_tcb(temp_tcb, host_thread)`, then `TLS_SLOT_BIONIC_TLS = host_thread->bionic_tls`).
/// Built by `thread::current` (our host is glibc, so the record is ours, not host `pthread_self()` as berberis's bionic host answers).
pub static HOST_PTHREADS: AtomicU32 = AtomicU32::new(0);
extern "C" fn get_host_pthread() -> usize {
    HOST_PTHREADS.fetch_add(1, Ordering::Relaxed);
    super::thread::current()
}

/// `__native_bridge_config_static_tls(const NativeBridgeStaticTlsConfig*)`: how the host creates guest static TLS, for the main thread too (NBS: "the host will replace the guest main thread's static TLS"): `thread::configure`; `Guest::start` then installs it.
pub static STATIC_TLS_CONFIGS: AtomicU32 = AtomicU32::new(0);
extern "C" fn config_static_tls(config: *const c_void) {
    STATIC_TLS_CONFIGS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the linker passes its `NativeBridgeStaticTlsConfig` (NBS __libc_init_main_thread_final).
    unsafe { super::thread::configure(config.cast()) };
}

/// Routes the linker's two NB hooks (NBS linker `init_static_tls_*`: naked bodies in their own slot) to host thunks, as berberis `InitializeLinker` binds them; before `seal`.
pub fn bind_linker(linker: &Image) -> Result<(), String> {
    let mut hooks: Vec<(&str, usize)> = vec![(
        "__dl___native_bridge_get_host_pthread",
        get_host_pthread as *const () as usize,
    )];
    // the `qe_no_tls` break (`ATL_NB_BREAK`) leaves this placeholder unpatched: the linker's `bl` branches to 0
    #[cfg(not(atl_nb_qe_no_tls))]
    hooks.push((
        "__dl___native_bridge_config_static_tls",
        config_static_tls as *const () as usize,
    ));
    for (name, host) in hooks {
        let at = linker
            .symbol(name)
            .ok_or_else(|| format!("linker64 lacks {name}"))?;
        active()
            .patch_hook(at, host)
            .map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
}

/// Routes every entry of the mapped NB vdso to a host function (`Active::patch_entry`); before `seal`.
pub fn bind(vdso: &Image) -> Result<(), String> {
    for (name, value_at) in vdso.dynsym_values() {
        let host: usize = match name.as_str() {
            "native_bridge_intercept_symbol" => intercept_symbol as *const () as usize,
            "native_bridge_post_init" => post_init as *const () as usize,
            "native_bridge_call_guest" => call_guest as *const () as usize,
            "native_bridge_trace" => trace as *const () as usize,
            _ => return Err(format!("NB vdso exports {name}, which has no host side")),
        };
        active()
            .patch_entry(vdso, value_at, host)
            .map_err(|why| format!("{name}: {why}"))?;
    }
    Ok(())
}
