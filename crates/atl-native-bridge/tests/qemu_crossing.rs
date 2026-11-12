// SPDX-License-Identifier: MIT
//! Q5-8 gate (QEMU bridge plan Q5-8), harness-less: the crossing counter under QEMU (feature `crossing-count`, counters.rs). The real arm64 guest linker starts the fixture (`xtask-harness/testdata/crossing/fixture.c`, PIE on libc.so); the host calls its `cx_*` functions through the crossing.
//! Expected values come from the fixture, never from the counter: N = `cx_calls()`, the sizes = `cx_size_at(i)` for `i < cx_size_count()`. A bin is a size's bit length (`ilog2 + 1`, 0 for size 0): the definition in counters.rs, re-derived here from the sizes alone.
//! Checks: (s) started; (r) the fixture's own results (copies, fills, compares and `time` all right, so the shims did the work); (c) during `cx_run` each bound host function ran exactly N crossings and no other did; (h) each counted kind's histogram gained exactly the bins of the sizes it was given; (j) `cx_jni` through the guest env and vm of fake host objects: each of its three JNI slots counted exactly N' = `cx_jni_calls()` (the shared `dispatch::count_jni` entry), no other slot, each fake ran N' times; (w) 2^16 further crossings (256 runs of 4N) dumped exactly once.
//! Env (xtask crossing.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (the engine boot executable, crates/atl-qemu-boot), ATL_NB_EXE (the fixture), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images; `<nb>/libnative_bridge_guest_libc` among the library directories).
//! Output: `crossing: (x) ok` or `crossing: (x) FAIL <why>` per check; exit 0 when all pass. The `qe_id` break (`ATL_NB_BREAK`) skews every binding to its neighbour's host function, so (c) and (r) must fail; `qe_jni_count` drops QEMU's JNI count, so (j) must fail.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "calls the guest's functions through Guest::call; loads the provider and glibc's time with dlopen/dlsym"
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use atl_native_bridge::counters::{self, BINS, Snapshot};
use atl_native_bridge::engine::jni::{guest_env, guest_vm};
use atl_native_bridge::engine::{Config, Guest, qemu};
use object::{Object, ObjectSymbol, read::elf::ElfFile64};

/// The fixture functions the host calls, in the order `Expected::values` holds them.
const SYMBOLS: [&str; 6] = [
    "cx_run",
    "cx_calls",
    "cx_size_count",
    "cx_size_at",
    "cx_jni",
    "cx_jni_calls",
];
/// jni.h slots `cx_jni` calls: `JNINativeInterface` GetVersion (after reserved0..3) and ExceptionCheck (233 entries, the last five NewDirectByteBuffer..GetObjectRefType follow it), `JNIInvokeInterface` DetachCurrentThread (reserved0..2, DestroyJavaVM, AttachCurrentThread, then it). Table sizes: 233 env entries, 8 vm entries.
const GET_VERSION: usize = 4;
const EXCEPTION_CHECK: usize = 228;
const DETACH_CURRENT_THREAD: usize = 5;
const ENV_SLOTS: usize = 233;
const VM_SLOTS: usize = 8;
/// The fake's GetVersion answer, as `cx_jni` checks it: jni.h `JNI_VERSION_1_6`.
const JNI_VERSION_1_6: i32 = 0x0001_0006;
/// Crossings in one 2^16 window: the 16-bit tick wraps exactly once in any window of this length.
const WINDOW: u64 = 1 << 16;
/// Bindings `cx_run` calls per iteration: memcpy, memset, memcmp, time (fixture.c).
const BINDINGS_PER_RUN: u64 = 4;

/// What the checks compare against: the executable's entry and the addresses of its functions, relative to its load bias.
struct Expected {
    e_entry: u64,
    /// Symbol values, in `SYMBOLS` order.
    values: Vec<u64>,
}
static EXPECTED: OnceLock<Expected> = OnceLock::new();

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("crossing: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("crossing: ({letter}) FAIL {why}");
            false
        }
    }
}

fn expected(exe: &Path) -> Result<Expected, String> {
    let bytes = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let elf = ElfFile64::<object::Endianness>::parse(&*bytes).map_err(|e| e.to_string())?;
    let mut values = Vec::new();
    for want in SYMBOLS {
        let sym = elf
            .symbols()
            .find(|s| s.name().is_ok_and(|n| n == want))
            .ok_or_else(|| format!("{}: no symbol {want}", exe.display()))?;
        values.push(sym.address());
    }
    Ok(Expected {
        e_entry: elf.entry(),
        values,
    })
}

/// Loads the bionic provider every ART process loads (`libatl_bionic`, built next to the NB output at `<out>/crates/atl-bionic/lib`, `<nb>` = `<out>/nb/<arch>`). The guest's libc init reads properties through it, so it must be in the process before the guest starts.
fn load_provider(nb: &Path) -> Result<(), String> {
    let lib = nb
        .ancestors()
        .nth(2)
        .ok_or("the NB directory is not <out>/nb/<arch>")?
        .join("crates/atl-bionic/lib/libatl_bionic.so.0.0.0");
    let path =
        std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen of a NUL-terminated path; the provider's constructors are its own.
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        return Err(format!("dlopen {}: failed", lib.display()));
    }
    Ok(())
}

/// glibc's `time`, the address the stub binds (bindings.rs `Host::Glibc`: dlsym on libc.so.6's own handle). Not the global scope: that holds the provider's bionic names.
fn glibc_time() -> Result<usize, String> {
    // SAFETY: a NUL-terminated name; RTLD_NOLOAD takes the handle of the libc this process already has, never closed.
    let h = unsafe { libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    if h.is_null() {
        return Err("libc.so.6 is not loaded".into());
    }
    // SAFETY: the handle is live and the name is NUL-terminated.
    let p = unsafe { libc::dlsym(h, c"time".as_ptr()) };
    if p.is_null() {
        return Err("glibc has no time".into());
    }
    Ok(p as usize)
}

/// Crossings each target took between two snapshots, for every target in `after`.
fn delta(before: &Snapshot, after: &Snapshot) -> Vec<(usize, u64)> {
    after
        .targets
        .iter()
        .map(|&(t, c)| {
            let b = before
                .targets
                .iter()
                .find(|(bt, _)| *bt == t)
                .map_or(0, |&(_, bc)| bc);
            (t, c.saturating_sub(b))
        })
        .collect()
}

fn calls_of(d: &[(usize, u64)], target: usize) -> u64 {
    d.iter().find(|(t, _)| *t == target).map_or(0, |&(_, c)| c)
}

/// Each named host function ran exactly `n` crossings, and no other host function ran.
fn check_calls(d: &[(usize, u64)], want: &[(usize, &str)], n: u64) -> Result<(), String> {
    for &(t, name) in want {
        let got = calls_of(d, t);
        if got != n {
            return Err(format!("{name} ran {got} crossings, the fixture makes {n}"));
        }
    }
    for &(t, c) in d {
        if c != 0 && !want.iter().any(|&(w, _)| w == t) {
            return Err(format!(
                "host target {t:#x} ran {c} crossings the fixture does not make"
            ));
        }
    }
    Ok(())
}

/// The fake host JNI functions `cx_jni` reaches: each counts its own calls, the record the per-slot counter is checked against.
static OWN_JNI: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
extern "C" fn get_version(_env: usize) -> i32 {
    OWN_JNI[0].fetch_add(1, Ordering::Relaxed);
    JNI_VERSION_1_6
}
extern "C" fn exception_check(_env: usize) -> u8 {
    OWN_JNI[1].fetch_add(1, Ordering::Relaxed);
    0
}
extern "C" fn detach_current_thread(_vm: usize) -> i32 {
    OWN_JNI[2].fetch_add(1, Ordering::Relaxed);
    0
}
/// Any other entry: the route reached a slot the fixture does not call.
extern "C" fn unimplemented() -> usize {
    println!("crossing: FAIL a JNI entry the fake does not implement was called");
    std::process::exit(2);
}

/// A fake host JNI object of `slots` entries (word 0 = its table), `set` filled in, the rest `unimplemented`. Leaked.
fn fake_object(slots: usize, set: &[(usize, usize)]) -> usize {
    let mut table = vec![unimplemented as *const () as usize; slots];
    for &(k, f) in set {
        table[k] = f;
    }
    let table: &'static [usize] = Box::leak(table.into_boxed_slice());
    Box::leak(Box::new(table.as_ptr() as usize)) as *const usize as usize
}

/// Each JNI slot in `want` gained exactly `n` between the snapshots, no other JNI slot gained any, and each fake function ran `n` times.
fn check_jni(before: &Snapshot, after: &Snapshot, want: &[usize], n: u64) -> Result<(), String> {
    let of = |s: &Snapshot, k: usize| s.jni.iter().find(|&&(x, _)| x == k).map_or(0, |&(_, c)| c);
    for &k in want {
        let got = of(after, k) - of(before, k);
        if got != n {
            return Err(format!(
                "JNI slot {k} counted {got} calls, the fixture makes {n}"
            ));
        }
    }
    for &(k, _) in &after.jni {
        let got = of(after, k) - of(before, k);
        if got != 0 && !want.contains(&k) {
            return Err(format!(
                "JNI slot {k} counted {got} calls the fixture does not make"
            ));
        }
    }
    for (i, own) in OWN_JNI.iter().enumerate() {
        let got = own.load(Ordering::Relaxed);
        if got != n {
            return Err(format!(
                "fake JNI function {i} ran {got} times, the fixture makes {n}"
            ));
        }
    }
    Ok(())
}

/// Bin counts that `n` calls of one kind give, call i with `sizes[i % sizes.len()]`.
fn expected_bins(sizes: &[u64], n: u64) -> [u64; BINS] {
    let mut bins = [0u64; BINS];
    for i in 0..n {
        let s = sizes[(i as usize) % sizes.len()];
        let bin = if s == 0 { 0 } else { s.ilog2() as usize + 1 };
        bins[bin] += 1;
    }
    bins
}

fn check_sizes(before: &Snapshot, after: &Snapshot, sizes: &[u64], n: u64) -> Result<(), String> {
    let want = expected_bins(sizes, n);
    for (kind, name) in ["memcpy", "memset", "memcmp"].into_iter().enumerate() {
        for (bin, &w) in want.iter().enumerate() {
            let got = after.sizes[kind][bin].saturating_sub(before.sizes[kind][bin]);
            if got != w {
                return Err(format!("{name} bin {bin}: {got} calls, the sizes give {w}"));
            }
        }
    }
    Ok(())
}

/// BOOT's continuation: the started guest (or its error) against the fixture. The answer is the boot executable's exit status.
fn started(r: Result<Guest, String>) -> u64 {
    let Some(x) = EXPECTED.get() else {
        return 2;
    };
    let mut guest = match r {
        Ok(g) => g,
        Err(e) => {
            println!("crossing: (s) FAIL {e}");
            return 1;
        }
    };
    println!("crossing: (s) ok");
    let bias = (guest.exe_entry as u64).wrapping_sub(x.e_entry);
    let addr = |i: usize| bias.wrapping_add(x.values[i]) as usize;
    let (run, calls, size_count, size_at) = (addr(0), addr(1), addr(2), addr(3));
    let time = match glibc_time() {
        Ok(t) => t,
        Err(e) => {
            println!("crossing: FAIL {e}");
            return 2;
        }
    };
    let want = [
        (time, "time"),
        (counters::memcpy as *const () as usize, "memcpy"),
        (counters::memset as *const () as usize, "memset"),
        (counters::memcmp as *const () as usize, "memcmp"),
    ];
    let mut ok = true;

    // SAFETY: the fixture's functions, none taking more than one integer argument (cx_size_at); Guest::call attaches this thread's CPU.
    let n = unsafe { guest.call(calls, 0, 0, 0) } as u64;
    // SAFETY: as above.
    let k = unsafe { guest.call(size_count, 0, 0, 0) };
    let mut sizes = Vec::with_capacity(k);
    for i in 0..k {
        // SAFETY: as above; the fixture's size table, read by index.
        sizes.push(unsafe { guest.call(size_at, i, 0, 0) } as u64);
    }

    let before = counters::snapshot();
    // SAFETY: as above; cx_run makes n calls of each binding and returns the number of wrong results.
    let bad = unsafe { guest.call(run, 0, 0, 0) };
    let after = counters::snapshot();
    let d = delta(&before, &after);
    ok &= check(
        'r',
        if bad == 0 {
            Ok(())
        } else {
            Err(format!("{bad} wrong results from the fixture's calls"))
        },
    );
    ok &= check('c', check_calls(&d, &want, n));
    ok &= check('h', check_sizes(&before, &after, &sizes, n));

    let (jni_run, jni_calls) = (addr(4), addr(5));
    let env = guest_env(fake_object(
        ENV_SLOTS,
        &[
            (GET_VERSION, get_version as *const () as usize),
            (EXCEPTION_CHECK, exception_check as *const () as usize),
        ],
    ));
    let vm = guest_vm(fake_object(
        VM_SLOTS,
        &[(
            DETACH_CURRENT_THREAD,
            detach_current_thread as *const () as usize,
        )],
    ));
    // SAFETY: as above.
    let jn = unsafe { guest.call(jni_calls, 0, 0, 0) } as u64;
    let before = counters::snapshot();
    // SAFETY: as above; cx_jni(env, vm) calls three JNI slots jn times each through the guest objects of the fakes and returns the number of wrong answers.
    let bad = unsafe { guest.call(jni_run, env, vm, 0) };
    let after = counters::snapshot();
    ok &= check(
        'j',
        if bad != 0 {
            Err(format!("{bad} wrong answers from the fixture's JNI calls"))
        } else {
            check_jni(
                &before,
                &after,
                &[GET_VERSION, EXCEPTION_CHECK, DETACH_CURRENT_THREAD],
                jn,
            )
        },
    );

    let per_run = BINDINGS_PER_RUN * n;
    let w = if per_run == 0 || WINDOW % per_run != 0 {
        Err(format!(
            "N = {n} does not divide the {WINDOW}-crossing window"
        ))
    } else {
        let w0 = counters::snapshot();
        let mut bad = 0;
        for _ in 0..WINDOW / per_run {
            // SAFETY: as above.
            bad += unsafe { guest.call(run, 0, 0, 0) };
        }
        let w1 = counters::snapshot();
        let crossings: u64 = delta(&w0, &w1).iter().map(|&(_, c)| c).sum();
        let dumps = w1.dumps - w0.dumps;
        if bad != 0 {
            Err(format!("{bad} wrong results in the window"))
        } else if crossings != WINDOW || dumps != 1 {
            Err(format!(
                "window made {crossings} crossings and {dumps} dumps, want {WINDOW} and 1"
            ))
        } else {
            Ok(())
        }
    };
    ok &= check('w', w);
    u64::from(!ok)
}

fn main() -> ExitCode {
    let vars = [
        "ATL_ENGINE_SO",
        "ATL_NB_BOOT",
        "ATL_NB_EXE",
        "ATL_NB_LINKER",
        "ATL_NB_VDSO",
    ]
    .map(|k| std::env::var_os(k).map(PathBuf::from));
    let [
        Some(so),
        Some(boot),
        Some(exe),
        Some(linker),
        Some(vdso_path),
    ] = vars
    else {
        println!(
            "crossing: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_EXE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    match expected(&exe) {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("crossing: FAIL fixture: {e}");
            return ExitCode::from(2);
        }
    }
    // the arm64 NB directory: the linker is at <nb>/native_bridge_guest_linker/linker64; the stub libc, libdl, ld-android and libm sit beside it
    let nb = linker
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let cfg = Config {
        linker,
        vdso: vdso_path,
        exe,
        library_path: vec![
            nb.join("libnative_bridge_guest_libc"),
            nb.join("libdl"),
            nb.join("ld-android"),
            nb.join("libm"),
        ],
    };
    if let Err(e) = load_provider(&nb) {
        println!("crossing: FAIL {e}");
        return ExitCode::from(2);
    }
    let r = qemu::run(&so, &boot, cfg, started);
    println!("crossing: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
