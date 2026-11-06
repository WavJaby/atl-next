// SPDX-License-Identifier: MIT
//! Host sides of the guest libc stubs whose host is the bionic-only API glibc lacks: the process's bionic provider (`libatl_bionic`, loaded in every ART process) exports each under its bionic name with bionic's signature, so the guest shares the host's one property store and abort message.
//! The names and signatures are generated from bionic's headers (build.rs `PROVIDER_APIS` -> `provider.rs`), the same headers atl-bionic implements them from: guest view == provider view, so the stub passes straight through to the provider (no shell),
//! except where the guest hands the provider a guest function -- `__system_property_foreach` and `__system_property_read_callback` -- which get own host sides: the provider calls a host trampoline that crosses to the guest callback (`arch::call_guest`), as pthread_create's start routine does (intercepts.rs).
//! Resolved by name at bind time (`RTLD_DEFAULT`): glibc defines none of these names, so the lookup cannot answer a glibc function of different ABI.

use std::ffi::{CString, c_char, c_void};

use super::{arch, thread};

// pass-through functions' aliases are unused: their stubs jump straight to the provider (guest view == provider view: one header, one arch, atl-bionic's implementation check)
#[allow(dead_code)]
mod api {
    include!(concat!(env!("OUT_DIR"), "/provider.rs"));
}
pub use api::NAMES;

/// The provider's `name`, untyped; None if no loaded object exports it.
fn lookup(name: &str) -> Option<usize> {
    let c = CString::new(name).ok()?;
    // SAFETY: dlsym of a NUL-terminated name in the global scope.
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// The host function of provider stub `name`; None for a name outside [`NAMES`] or one the provider lacks.
pub fn host_fn(name: &str) -> Option<usize> {
    if !NAMES.contains(&name) {
        return None;
    }
    let provider = lookup(name)?;
    Some(match name {
        "__system_property_foreach" => {
            PROVIDER_FOREACH.get_or_init(|| provider);
            foreach as *const () as usize
        }
        "__system_property_read_callback" => {
            PROVIDER_READ_CALLBACK.get_or_init(|| provider);
            read_callback as *const () as usize
        }
        _ => provider,
    })
}

static PROVIDER_FOREACH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
static PROVIDER_READ_CALLBACK: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// A guest callback and its cookie, for the trampoline the provider calls.
struct Guest {
    f: usize,
    cookie: usize,
}

/// `int __system_property_foreach(void (*)(const prop_info*, void*), void*)`.
extern "C" fn foreach(f: usize, cookie: *mut c_void) -> i32 {
    let Some(&p) = PROVIDER_FOREACH.get() else {
        return -1;
    };
    // SAFETY: the provider's export of this name, whose type is the header's (build.rs `provider`).
    let provider: api::__system_property_foreach = unsafe { std::mem::transmute(p) };
    let mut g = Guest {
        f,
        cookie: cookie as usize,
    };
    // SAFETY: `g` outlives the call; the provider calls `foreach_one` synchronously only.
    unsafe { provider(Some(foreach_one), (&raw mut g).cast()) }
}

unsafe extern "C" fn foreach_one(pi: *const api::prop_info, g: *mut c_void) {
    // SAFETY: the `Guest` `foreach` passed, alive for its call.
    let g = unsafe { &*g.cast::<Guest>() };
    let ht = thread::attach_current();
    // SAFETY: the guest's `void (*)(const prop_info*, void*)`, this thread's record.
    unsafe { arch::call_guest(&mut *ht, g.f, pi as usize, g.cookie, 0, 0) };
}

/// `void __system_property_read_callback(const prop_info*, void (*)(void*, const char*, const char*, uint32_t), void*)`.
extern "C" fn read_callback(pi: *const api::prop_info, f: usize, cookie: *mut c_void) {
    let Some(&p) = PROVIDER_READ_CALLBACK.get() else {
        return;
    };
    // SAFETY: the provider's export of this name, whose type is the header's (build.rs `provider`).
    let provider: api::__system_property_read_callback = unsafe { std::mem::transmute(p) };
    let mut g = Guest {
        f,
        cookie: cookie as usize,
    };
    // SAFETY: `pi` is the guest's, from the shared area; `g` outlives the call, made synchronously.
    unsafe { provider(pi, Some(read_one), (&raw mut g).cast()) }
}

unsafe extern "C" fn read_one(
    g: *mut c_void,
    name: *const c_char,
    value: *const c_char,
    serial: u32,
) {
    // SAFETY: the `Guest` `read_callback` passed, alive for its call.
    let g = unsafe { &*g.cast::<Guest>() };
    let ht = thread::attach_current();
    // SAFETY: the guest's `void (*)(void*, const char*, const char*, uint32_t)`, this thread's record.
    unsafe {
        arch::call_guest(
            &mut *ht,
            g.f,
            g.cookie,
            name as usize,
            value as usize,
            serial as usize,
        )
    };
}
