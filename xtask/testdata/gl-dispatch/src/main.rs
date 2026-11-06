// SPDX-License-Identifier: MIT
//! GL dispatch probe: AOSP libEGL's `setGlThreadSpecific` stores a thread's hooks table, libGLESv2's `glGetError` (USE_SLOW_BINDING: `getGlThreadSpecific()` -> `__get_tls()`) must dispatch through the table its own thread stored.
//! Two threads, two fake tables (every entry one function returning the thread's marker), so the check needs no entry index; the size is gl_hooks_t's in pointers (xtask derives it from entries.in).
//!
//! `probe <libEGL> <libGLESv2> <table pointers> <mode> [calls]`, mode:
//! - `same`: both libraries in one link map -> one atl-bionic, one slot array. Prints `dispatch ok` and the per-call cost.
//! - `split`: libGLESv2 by `dlmopen(LM_ID_NEWLM)` -> its own atl-bionic copy, so its `__get_tls()` reads another array: the failure a per-DSO slot array would cause. Must fail.
//!
//! Output: `gl-dispatch: <verdict>` lines; exit 0 = every check held.

use std::ffi::{CString, c_void};
use std::sync::{Arc, Barrier};
use std::time::Instant;

const MARK_A: u32 = 0xA1;
const MARK_B: u32 = 0xB2;

extern "C" fn mark_a() -> u32 {
    MARK_A
}
extern "C" fn mark_b() -> u32 {
    MARK_B
}

type SetHooks = unsafe extern "C" fn(*const c_void);
type GetError = unsafe extern "C" fn() -> u32;

fn open(path: &str, split: bool) -> *mut c_void {
    let c = CString::new(path).unwrap();
    // SAFETY: a NUL-terminated path; the libraries' constructors run on load
    let h = unsafe {
        if split {
            libc::dlmopen(libc::LM_ID_NEWLM, c.as_ptr(), libc::RTLD_NOW)
        } else {
            libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL)
        }
    };
    if h.is_null() {
        // SAFETY: dlerror returns a NUL-terminated string after a failed dlopen
        let e = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) };
        eprintln!("gl-dispatch: FAIL dlopen {path}: {}", e.to_string_lossy());
        std::process::exit(2);
    }
    h
}

fn sym(h: *mut c_void, name: &str) -> usize {
    let c = CString::new(name).unwrap();
    // SAFETY: a live handle, a NUL-terminated name
    let p = unsafe { libc::dlsym(h, c.as_ptr()) };
    if p.is_null() {
        eprintln!("gl-dispatch: FAIL no symbol {name}");
        std::process::exit(2);
    }
    p as usize
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let [_, egl, gles, size, mode, ..] = &a[..] else {
        eprintln!("usage: probe <libEGL> <libGLESv2> <table pointers> <same|split> [calls]");
        std::process::exit(2);
    };
    let size: usize = size.parse().expect("table pointers");
    let calls: u64 = a.get(5).map_or(10_000_000, |c| c.parse().expect("calls"));
    let split = match mode.as_str() {
        "same" => false,
        "split" => true,
        _ => panic!("mode same|split"),
    };
    let egl = open(egl, false);
    let gles = open(gles, split);
    let set = sym(egl, "_ZN7android19setGlThreadSpecificEPKNS_10gl_hooks_tE");
    let get_error = sym(gles, "glGetError");
    let table = |f: extern "C" fn() -> u32| vec![f as usize; size];
    let (ta, tb) = (Arc::new(table(mark_a)), Arc::new(table(mark_b)));
    // SAFETY: the symbols' C signatures (hooks.h setGlThreadSpecific, GLES2 glGetError)
    let (set, get_error): (SetHooks, GetError) = unsafe { (std::mem::transmute(set), std::mem::transmute(get_error)) };
    let barrier = Arc::new(Barrier::new(2));
    let run = |t: Arc<Vec<usize>>, bar: Arc<Barrier>, first: bool| {
        move || {
            // B calls before storing anything: an empty slot dispatches nowhere (slow binding returns 0), whatever A stored
            let before = if first {
                // SAFETY: a table of gl_hooks_t's size whose every entry is a `u32 (void)` function, alive for the thread
                unsafe { set(t.as_ptr().cast()) };
                bar.wait();
                None
            } else {
                bar.wait();
                // SAFETY: GLES2 entry, no arguments
                let v = unsafe { get_error() };
                // SAFETY: as above
                unsafe { set(t.as_ptr().cast()) };
                Some(v)
            };
            bar.wait();
            // SAFETY: GLES2 entry, no arguments
            (before, unsafe { get_error() })
        }
    };
    let ha = std::thread::spawn(run(ta.clone(), barrier.clone(), true));
    let hb = std::thread::spawn(run(tb.clone(), barrier.clone(), false));
    let ((_, got_a), (before_b, got_b)) = (ha.join().unwrap(), hb.join().unwrap());
    let ok = got_a == MARK_A && got_b == MARK_B && before_b == Some(0);
    println!(
        "gl-dispatch: thread A {got_a:#x} (want {MARK_A:#x}), thread B {got_b:#x} (want {MARK_B:#x}), B before storing {:#x} (want 0)",
        before_b.unwrap_or(u32::MAX)
    );
    if !ok {
        println!("gl-dispatch: FAIL ({mode})");
        std::process::exit(1);
    }
    // per-call cost: glGetError through libGLESv2's slow binding vs the table's function called directly
    // SAFETY: as above, on this thread
    unsafe { set(ta.as_ptr().cast()) };
    let direct: extern "C" fn() -> u32 = mark_a;
    let time = |f: &dyn Fn() -> u32| {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..calls {
            acc = acc.wrapping_add(u64::from(std::hint::black_box(f())));
        }
        std::hint::black_box(acc);
        t.elapsed().as_nanos() as f64 / calls as f64
    };
    // SAFETY: GLES2 entry, no arguments, this thread's table stored
    let through = time(&|| unsafe { get_error() });
    let base = time(&|| std::hint::black_box(direct)());
    println!("gl-dispatch: cost {calls} calls: glGetError {through:.2} ns/call, direct {base:.2} ns/call, dispatch {:.2} ns/call", through - base);
    println!("gl-dispatch: ok ({mode})");
}
