// SPDX-License-Identifier: MIT
//! bionic's TLS_SLOT_OPENGL_API on a glibc x86_64 thread (build.rs `PLATFORM_FORWARDS`): `%fs:0 + 4 * 8`, glibc's tcbhead_t `sysinfo`.
//! Holds if glibc leaves the word alone: zero on every new thread, per thread, and unchanged across glibc work a GL-using thread does (allocation, dlopen, thread creation, file I/O).
#![cfg(target_arch = "x86_64")]

/// bionic tls_defines.h x86_64 `TLS_SLOT_OPENGL_API`.
const TLS_SLOT_OPENGL_API: usize = 4;

/// The slot as AOSP's hooks.h reads it: `__get_tls()[TLS_SLOT_OPENGL_API]`, `__get_tls()` = `%fs:0` (bionic tls.h).
fn slot() -> *mut usize {
    let base: *mut usize;
    // SAFETY: reads the thread pointer's self word, which glibc and bionic both set on x86_64; no memory is written.
    unsafe { std::arch::asm!("mov {}, fs:0", out(reg) base) };
    base.wrapping_add(TLS_SLOT_OPENGL_API)
}

fn get() -> usize {
    // SAFETY: the slot lies in this thread's TCB, live for the thread's lifetime.
    unsafe { slot().read_volatile() }
}

fn set(v: usize) {
    // SAFETY: as `get`; the word is the one AOSP's setGlThreadSpecific writes.
    unsafe { slot().write_volatile(v) }
}

/// glibc dlfcn.h.
const RTLD_NOW: i32 = 2;
unsafe extern "C" {
    fn dlopen(file: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
    fn dlclose(handle: *mut std::ffi::c_void) -> i32;
}

/// glibc work between the write and the read: what a GL front's thread does around its calls.
fn glibc_work() {
    let v: Vec<u8> = vec![7; 1 << 22];
    assert_eq!(v.iter().map(|&b| usize::from(b)).sum::<usize>(), 7 << 22);
    let lib = std::ffi::CString::new("libz.so.1").unwrap();
    // SAFETY: dlopen/dlclose of zlib, which has no constructors with side effects on us.
    unsafe {
        let h = dlopen(lib.as_ptr(), RTLD_NOW);
        assert!(!h.is_null());
        dlclose(h);
    }
    let inner = std::thread::spawn(get).join().unwrap();
    assert_eq!(inner, 0, "a new thread's slot starts empty");
    let p = std::env::temp_dir().join(format!("atl-tls-slot-{}", std::process::id()));
    std::fs::write(&p, b"x").unwrap();
    std::fs::remove_file(&p).unwrap();
}

#[test]
fn opengl_api_slot_is_per_thread_and_left_alone_by_glibc() {
    let threads: Vec<_> = (1..=4usize)
        .map(|i| {
            std::thread::spawn(move || {
                assert_eq!(get(), 0, "thread {i}: slot not empty at start");
                let mine = 0x5100_0000 + i;
                set(mine);
                glibc_work();
                let after = get();
                set(0);
                after == mine
            })
        })
        .collect();
    for (i, t) in threads.into_iter().enumerate() {
        assert!(t.join().unwrap(), "thread {}: glibc changed the slot", i + 1);
    }
}

/// The probe responds: a word glibc does use (tcbhead_t `stack_guard`, slot 5) is non-zero, so a read of the wrong word would show.
#[test]
fn probe_reads_the_tcb() {
    // SAFETY: as `get`, one word further.
    let guard = unsafe { slot().add(1).read_volatile() };
    assert_ne!(guard, 0, "stack guard word reads zero: not glibc's TCB");
}
