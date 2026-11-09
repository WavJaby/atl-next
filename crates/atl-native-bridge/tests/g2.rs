// SPDX-License-Identifier: MIT
//! Spike gate g2 (bridge-1 route N): the guest linker `dlopen`s an NDK-built app library and its native functions run, the host thread intact after each call.
//! Library: `libatl-jni-test.so` built by NDK r29 for android-29 (native ELF TLS: `__thread` through the guest's `__tls_get_addr`), linked against guest `libc++_shared.so`; it dlopens `libatl-jni-sibling.so` itself.
//! Env: tests/common/mod.rs, plus `ATL_NB_TESTLIB`; SKIP without them. Its JNI functions are called with env = cls = NULL: the ones called use neither.
//! Checks per call: the return value, then FS == host and the host probe. Calls: `tlsCounter` x2 (1 then 2: one guest TLS slot kept across crossings), `errnoRoundTrip` (ENOENT through guest errno), `dlopenSibling` (guest dlopen + dlsym from guest code: 42), `cppThrowCatch` (libc++ throw + unwind through guest `dl_iterate_phdr`: 1).
//! Deliberate break: `ATL_NB_BREAK=no_fs_restore` -> the first call's FS check fails. Control, every run: a library the guest cannot find gives NULL (the dlopen check responds).
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::ffi::CStr;
use std::process::ExitCode;

use atl_native_bridge::engine::{Guest, arch};
use common::{RTLD_NOW, host_intact, need};

const LIB: &CStr = c"libatl-jni-test.so";
const PREFIX: &str = "Java_atl_bridge1_testlib_AtlJniTest_";

/// (function, expected return) in call order; values from apk/bridge1-testlib/src (atl_jni_test.c, atl_jni_cxx.cpp).
const CALLS: [(&str, usize); 5] = [
    ("tlsCounter", 1),
    ("tlsCounter", 2),
    // ENOENT
    ("errnoRoundTrip", 2),
    // atl_jni_sibling.c atl_sibling_answer
    ("dlopenSibling", 42),
    ("cppThrowCatch", 1),
];

fn sym(g: &mut Guest, dlsym: usize, h: usize, name: &str) -> Result<usize, String> {
    let c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: __loader_dlsym(void*, const char*, const void* caller).
    let f = unsafe { g.call(dlsym, h, c.as_ptr() as usize, g.exe_entry) };
    if f == 0 {
        return Err(format!("guest dlsym({name}) = NULL"));
    }
    Ok(f)
}

/// The guest linker's last error text.
fn dlerror(g: &mut Guest) -> String {
    let Ok(f) = need(&g.linker, "__dl___loader_dlerror") else {
        return "(no __loader_dlerror)".into();
    };
    // SAFETY: char* __loader_dlerror(void); NULL or a NUL-terminated text in the guest's TLS buffer.
    let p = unsafe { g.call(f, 0, 0, 0) };
    if p == 0 {
        return "(no error)".into();
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(p as *const std::ffi::c_char) }
        .to_string_lossy()
        .into_owned()
}

fn run(env: &common::Nb) -> Result<(), String> {
    let self0 = common::self0();
    let host = arch::fs_base();
    let mut g = common::start(env)?;
    host_intact(&g, self0, host, "__linker_init")?;
    let dlopen = need(&g.linker, "__dl___loader_dlopen")?;
    let dlsym = need(&g.linker, "__dl___loader_dlsym")?;
    // control: the dlopen check responds
    // SAFETY: __loader_dlopen(const char*, int, const void* caller).
    let none = unsafe {
        g.call(
            dlopen,
            c"libatl-no-such-lib.so".as_ptr() as usize,
            RTLD_NOW,
            g.exe_entry,
        )
    };
    host_intact(&g, self0, host, "dlopen control")?;
    if none != 0 {
        return Err("control: dlopen of a missing library answered non-NULL".into());
    }
    // SAFETY: as above; the caller address puts it in the main executable's (default) namespace, whose search path holds ATL_NB_TESTLIB.
    let h = unsafe { g.call(dlopen, LIB.as_ptr() as usize, RTLD_NOW, g.exe_entry) };
    host_intact(&g, self0, host, "dlopen")?;
    if h == 0 {
        return Err(format!("guest dlopen({LIB:?}) = NULL: {}", dlerror(&mut g)));
    }
    for (name, want) in CALLS {
        let f = sym(&mut g, dlsym, h, &format!("{PREFIX}{name}"))?;
        // SAFETY: jint fn(JNIEnv*, jclass); these bodies touch neither argument.
        let got = unsafe { g.call(f, 0, 0, 0) } as u32 as usize;
        host_intact(&g, self0, host, name)?;
        if got != want {
            return Err(format!("{name} = {got}, want {want}"));
        }
        println!("g2: {name} = {got}, FS host after");
    }
    // isNativeBridgeFunctionPointer's question (loader.rs `is_guest_code`: guest `__loader_dladdr` != 0): a guest function yes, a host one no.
    // The app gate cannot break it: ART trampolines every native of a bridged namespace without asking (jni_internal.cc RegisterNatives).
    let dladdr = need(&g.linker, "__dl___loader_dladdr")?;
    let guest_fn = sym(&mut g, dlsym, h, &format!("{PREFIX}tlsCounter"))?;
    let mut info = [0usize; 4];
    for (what, addr, want) in [
        ("guest", guest_fn, true),
        ("host", run as *const () as usize, false),
    ] {
        // SAFETY: int __loader_dladdr(const void*, Dl_info*); `info` is a Dl_info-sized buffer.
        let got = unsafe { g.call(dladdr, addr, info.as_mut_ptr() as usize, 0) } != 0;
        host_intact(&g, self0, host, "dladdr")?;
        if got != want {
            return Err(format!("dladdr({what} fn) = {got}, want {want}"));
        }
    }
    println!("g2: dladdr: guest fn yes, host fn no");
    println!(
        "g2: ok: dlopen({LIB:?}) = {h:#x}, {} calls, guest FS {:#x}",
        CALLS.len(),
        g.thread().guest_fs()
    );
    Ok(())
}

fn main() -> ExitCode {
    let stage = match common::nb_env() {
        Some(env) if !env.testlib.is_empty() => Some(run(&env)),
        _ => {
            println!("g2: SKIP (ATL_NB / ATL_NB_LIBCXX / ATL_NB_TESTLIB unset)");
            None
        }
    };
    let mut v = common::Verdict::new("g2");
    v.stage(stage);
    v.finish()
}
