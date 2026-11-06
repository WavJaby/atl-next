// SPDX-License-Identifier: MIT
//! Spike gate g1 (bridge-1 route N): the identity crossing leaves the host thread intact, and the guest linker initialises through it.
//! `harness = false`: runs on the process's main thread, no test-runner threads touching TLS while the crossing is under test.
//! Build: `ATL_G1_NB=<out>/nb/x86_64 ATL_G1_LIBCXX=<dir of the guest libc++.so> cargo test -p atl-native-bridge --test g1`; stage 2 prints SKIP without them.
//! Deliberate break: `RUSTFLAGS=--cfg=atl_g1_break` drops the crossing's FS restore -> stage 0's probe fails (exit 1).
//!
//! Stages:
//! - 0, always: the crossing installs a guest thread pointer for the call and restores the host's after; host probe after it.
//! - 2, with a native-bridge build: `__linker_init` loads NBS `app_process64` and its closure and returns; host probe; initialised = `__loader_dlopen("libc.so")` non-NULL and guest `getpid` = host pid (bionic caches it in the guest TCB).
//!   Then the guest-dlsym proof: linker64's `dlsym` (crate atl-guest-dlsym, what Rust's panic_abort calls for `android_set_abort_message`) answers the linker's own function and counts the hit, a name it does not answer fails the same check (its break), and calling the answer leaves the text in an `[anon:abort message]` mapping.
//!   Blocked: the linker's first host call, `__native_bridge_get_host_pthread`, needs a bionic `pthread_internal_t` the engine cannot build yet (engine/vdso.rs `get_host_pthread`); the process aborts there.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

use std::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

use atl_native_bridge::engine::{Config, Guest, HostThread, arch, vdso};

const RTLD_NOW: usize = 2;
const ABORT_TEXT: &CStr = c"atl g1 abort probe";
/// Words of the stand-in guest TCB of stage 0: covers glibc's `tcbhead_t` reads (`self` at 0x10) as zeros.
const FAKE_TCB_WORDS: usize = 64;

/// Host-side probe. Errors are static: on a guest thread pointer an allocation would itself crash.
fn probe(self0: libc::pthread_t) -> Result<(), &'static str> {
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
    let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"g1 %d".as_ptr(), 7) };
    if n != 4 || &buf[..4] != b"g1 7" {
        return Err("snprintf wrong");
    }
    // SAFETY: format without conversions.
    unsafe { libc::printf(c"g1: host printf ok\n".as_ptr()) };
    Ok(())
}

/// Stage 0's "guest": answers the thread pointer it runs on; inline asm only, no TLS.
extern "C" fn read_fs(_: usize, _: usize, _: usize) -> usize {
    arch::fs_base()
}

fn crossing() -> Result<(), String> {
    // SAFETY: no preconditions.
    let self0 = unsafe { libc::pthread_self() };
    let host = arch::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    ht.set_guest_fs(tcb);
    // SAFETY: read_fs takes three integers and touches no TLS.
    let inside = unsafe { arch::call_guest(ht, read_fs as *const () as usize, 0, 0, 0) };
    if let Err(e) = probe(self0) {
        // SAFETY: `host` is this thread's own thread pointer, read above.
        unsafe { arch::set_fs_base(host) };
        return Err(format!("stage 0: {e}"));
    }
    if inside != tcb {
        return Err(format!(
            "stage 0: guest ran on FS {inside:#x}, want {tcb:#x}"
        ));
    }
    if ht.host_fs() != host {
        return Err(format!(
            "stage 0: saved host FS {:#x}, want {host:#x}",
            ht.host_fs()
        ));
    }
    println!("g1: stage 0 ok: guest call on FS {inside:#x}, host FS {host:#x} restored");
    Ok(())
}

/// The guest-dlsym check: `got` is the linker's `want`, and the counter moved by one.
fn dlsym_ok(got: usize, want: usize, before: u32, after: u32) -> Result<(), String> {
    if got != want {
        return Err(format!("guest dlsym answered {got:#x}, want {want:#x}"));
    }
    if after != before + 1 {
        return Err(format!(
            "__dl_atl_guest_dlsym_hits {before} -> {after}, want +1"
        ));
    }
    Ok(())
}

fn need(img: &atl_native_bridge::engine::image::Image, name: &str) -> Result<usize, String> {
    img.symbol(name)
        .ok_or_else(|| format!("linker64 lacks {name}"))
}

/// The guest-dlsym proof on the initialised linker (atl-guest-dlsym reaches `strlen` through the GOT: it runs only after the linker relocated itself); returns the answered address.
fn dlsym_proof(g: &mut Guest) -> Result<usize, String> {
    let gdlsym = need(&g.linker, "__dl_dlsym")?;
    let want = need(&g.linker, "__dl_android_set_abort_message")?;
    // SAFETY: a u32 atomic in the linker's mapped .bss.
    let hits = unsafe { &*(need(&g.linker, "__dl_atl_guest_dlsym_hits")? as *const AtomicU32) };
    let before = hits.load(Ordering::Relaxed);
    // SAFETY: dlsym(void* handle, const char* symbol), RTLD_DEFAULT = NULL.
    let got = unsafe { g.call(gdlsym, 0, c"android_set_abort_message".as_ptr() as usize, 0) };
    dlsym_ok(got, want, before, hits.load(Ordering::Relaxed))?;
    println!(
        "g1: guest dlsym -> __dl_android_set_abort_message, hits {before} -> {}",
        hits.load(Ordering::Relaxed)
    );
    // its break: a name the guest dlsym does not answer fails the same check
    let before = hits.load(Ordering::Relaxed);
    // SAFETY: as above.
    let none = unsafe {
        g.call(
            gdlsym,
            0,
            c"android_set_abort_message_nonexistent".as_ptr() as usize,
            0,
        )
    };
    match dlsym_ok(none, want, before, hits.load(Ordering::Relaxed)) {
        Err(e) => println!("g1: dlsym break detected: {e}"),
        Ok(()) => return Err("dlsym break not detected".into()),
    }
    Ok(got)
}

/// The text of the `[anon:abort message]` mapping (bionic android_set_abort_message.cpp: magic1, magic2, size, then the text).
fn abort_message() -> Result<String, String> {
    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| e.to_string())?;
    let line = maps
        .lines()
        .find(|l| l.ends_with("[anon:abort message]"))
        .ok_or("no [anon:abort message] mapping")?;
    let start = usize::from_str_radix(line.split('-').next().unwrap_or(""), 16)
        .map_err(|e| e.to_string())?;
    // magic_abort_msg_t: two u64 magics + abort_msg_t.size
    const TEXT_OFFSET: usize = 24;
    // SAFETY: the mapping is readable and holds a NUL-terminated text at TEXT_OFFSET (strcpy in bionic).
    Ok(
        unsafe { CStr::from_ptr((start + TEXT_OFFSET) as *const c_char) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn linker_init(nb: &Path, libcxx: &Path) -> Result<(), String> {
    let cfg = Config {
        linker: nb.join("native_bridge_guest_linker/linker64"),
        vdso: nb.join("libnative_bridge_vdso/libnative_bridge_vdso.so"),
        exe: nb.join("native_bridge_guest_app_process/app_process64"),
        library_path: vec![
            nb.join("libnative_bridge_guest_libc"),
            nb.join("libm"),
            nb.join("libdl"),
            nb.join("ld-android"),
            libcxx.to_path_buf(),
        ],
    };
    // SAFETY: no preconditions.
    let self0 = unsafe { libc::pthread_self() };
    let mut g = Guest::start(&cfg)?;
    if let Err(e) = probe(self0) {
        g.force_host_fs();
        return Err(format!("after __linker_init: {e}"));
    }
    println!(
        "g1: linker initialised, exe entry {:#x}, guest FS {:#x}; stubs bound {} unbound {}",
        g.exe_entry,
        g.thread().guest_fs(),
        vdso::BOUND.load(Ordering::Relaxed),
        vdso::UNBOUND.load(Ordering::Relaxed)
    );
    if g.thread().guest_fs() == g.thread().host_fs() || g.thread().guest_fs() == 0 {
        return Err("the guest linker installed no thread pointer of its own".into());
    }
    let dlopen = need(&g.linker, "__dl___loader_dlopen")?;
    let dlsym = need(&g.linker, "__dl___loader_dlsym")?;
    // SAFETY: __loader_dlopen(const char*, int, const void* caller), caller inside the main executable -> its (default) namespace.
    let h = unsafe { g.call(dlopen, c"libc.so".as_ptr() as usize, RTLD_NOW, g.exe_entry) };
    probe(self0).map_err(|e| format!("after __loader_dlopen: {e}"))?;
    if h == 0 {
        return Err("guest __loader_dlopen(libc.so) = NULL".into());
    }
    // SAFETY: __loader_dlsym(void*, const char*, const void* caller).
    let getpid = unsafe { g.call(dlsym, h, c"getpid".as_ptr() as usize, g.exe_entry) };
    if getpid == 0 {
        return Err("guest dlsym(libc.so, getpid) = NULL".into());
    }
    // SAFETY: bionic getpid(void).
    let pid = unsafe { g.call(getpid, 0, 0, 0) };
    probe(self0).map_err(|e| format!("after guest getpid: {e}"))?;
    if pid != std::process::id() as usize {
        return Err(format!("guest getpid {pid} != host {}", std::process::id()));
    }
    let set = dlsym_proof(&mut g)?;
    // SAFETY: android_set_abort_message(const char*), on the initialised guest thread.
    unsafe { g.call(set, ABORT_TEXT.as_ptr() as usize, 0, 0) };
    probe(self0).map_err(|e| format!("after android_set_abort_message: {e}"))?;
    let text = abort_message()?;
    if text.as_bytes() != ABORT_TEXT.to_bytes() {
        return Err(format!("abort message {text:?}"));
    }
    println!(
        "g1: stage 2 ok: dlopen(libc.so) = {h:#x}, guest getpid = host pid, abort message {text:?}"
    );
    Ok(())
}

fn run() -> Result<(), String> {
    crossing()?;
    let (Some(nb), Some(libcxx)) = (
        std::env::var_os("ATL_G1_NB"),
        std::env::var_os("ATL_G1_LIBCXX"),
    ) else {
        println!("g1: stage 2 SKIP (ATL_G1_NB / ATL_G1_LIBCXX unset: no native-bridge build)");
        return Ok(());
    };
    linker_init(&PathBuf::from(nb), &PathBuf::from(libcxx))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("g1: ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("g1: FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}
