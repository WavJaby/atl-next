// SPDX-License-Identifier: MIT
//! Spike gate g1 (bridge-1 route N): the identity crossing leaves the host thread intact, and the guest linker initialises through it.
//! Build env: tests/common/mod.rs; stage 2 prints SKIP without a native-bridge build.
//! Deliberate break: `RUSTFLAGS=--cfg=atl_nb_no_fs_restore` drops the crossing's FS restore -> the probes of stage 0 and stage 2 both fail (every stage runs; exit 1).
//!
//! Stages:
//! - 0, always: the crossing installs a guest thread pointer for the call and restores the host's after; host probe after it.
//! - 2, with a native-bridge build: `__linker_init` loads NBS `app_process64` and its closure and returns; host probe; initialised = `__loader_dlopen("libc.so")` non-NULL and guest `getpid` = host pid (bionic caches it in the guest TCB).
//!   Then the guest-dlsym proof: linker64's `dlsym` (crate atl-guest-dlsym, what Rust's panic_abort calls for `android_set_abort_message`) answers the linker's own function and counts the hit, a name it does not answer fails the same check (its break), and calling the answer leaves the text in bionic's abort-message mapping.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a harness-less gate: its verdict lines are its output"
)]

mod common;

use std::ffi::{CStr, c_char};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

use atl_native_bridge::engine::{Guest, HostThread, arch, vdso};
use common::{RTLD_NOW, need, probe};

const ABORT_TEXT: &CStr = c"atl g1 abort probe";
/// Words of the stand-in guest TCB of stage 0: covers glibc's `tcbhead_t` reads (`self` at 0x10) as zeros.
const FAKE_TCB_WORDS: usize = 64;

/// Stage 0's "guest": answers the thread pointer it runs on; inline asm only, no TLS.
extern "C" fn read_fs(_: usize, _: usize, _: usize) -> usize {
    arch::fs_base()
}

fn crossing() -> Result<(), String> {
    let self0 = common::self0();
    let host = arch::fs_base();
    let tcb = Box::leak(vec![0usize; FAKE_TCB_WORDS].into_boxed_slice()).as_mut_ptr() as usize;
    let ht = HostThread::leak();
    // SAFETY: a leaked FAKE_TCB_WORDS-word stand-in TCB (bionic's: 11 words); the record is leaked.
    unsafe { arch::init_tcb(tcb, &raw mut *ht) };
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

/// bionic android_set_abort_message.cpp `fill_abort_message_magic`: the two words a crash reporter finds the message by.
const ABORT_MAGIC: [u64; 2] = [0xb18e40886ac388f0, 0xc6dfba755a1de0b5];

/// The text of bionic's abort-message mapping (magic_abort_msg_t: magic1, magic2, abort_msg_t.size, then the text), found by its magic as debuggerd does: the mapping is named `[anon:abort message]` only where the kernel has `CONFIG_ANON_VMA_NAME` (WSL2's 6.6 kernel does not).
fn abort_message() -> Result<String, String> {
    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| e.to_string())?;
    // magic_abort_msg_t: two u64 magics + abort_msg_t.size
    const TEXT_OFFSET: usize = 24;
    for line in maps.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // private anonymous read-write mappings (bionic mmaps it MAP_ANONYMOUS | MAP_PRIVATE): no path, or a name only
        if f.len() < 5 || f[1] != "rw-p" || f.get(5).is_some_and(|p| p.starts_with('/')) {
            continue;
        }
        let Some((Ok(start), Ok(end))) = f[0]
            .split_once('-')
            .map(|(s, e)| (usize::from_str_radix(s, 16), usize::from_str_radix(e, 16)))
        else {
            continue;
        };
        if end - start <= TEXT_OFFSET {
            continue;
        }
        // SAFETY: the start of a readable mapping of this process.
        let head = unsafe { std::slice::from_raw_parts(start as *const u64, 2) };
        if head == ABORT_MAGIC {
            // SAFETY: bionic strcpy'd a NUL-terminated text at TEXT_OFFSET.
            return Ok(
                unsafe { CStr::from_ptr((start + TEXT_OFFSET) as *const c_char) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Err("no mapping holds bionic's abort-message magic".into())
}

fn linker_init(env: &common::Nb) -> Result<(), String> {
    let self0 = common::self0();
    let mut g = common::start(env)?;
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

/// Stage 2, or None when no native-bridge build is named.
fn stage2() -> Option<Result<(), String>> {
    let Some(env) = common::nb_env() else {
        println!("g1: stage 2 SKIP (ATL_NB / ATL_NB_LIBCXX unset: no native-bridge build)");
        return None;
    };
    Some(linker_init(&env))
}

fn main() -> ExitCode {
    // every stage runs (a failed probe puts the host FS back), so the break shows in each stage's probe
    common::verdict("g1", [Some(crossing()), stage2()])
}
