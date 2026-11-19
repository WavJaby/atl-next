// SPDX-License-Identifier: MIT
//! Guest-main gate (phase4-32bit P4b), harness-less: the real guest linker starts `main_exe.c` (PIE on the stub libc); the host calls its `fx_run`, which does what bionic's `__libc_init` does after the start, `exit(main())`. main creates a thread of an explicit attr, joins it, then a thread that registers thread-exit destructors (`__cxa_thread_atexit_impl`, what a C++ `thread_local` compiles to), a key value and a cleanup handler and ends in `pthread_exit`, then one that pops a handler with execute and returns, registers two on the main thread, and returns. The guest's exit runs its `__cxa_finalize`, then NBS's `native_bridge_exit` stub, bound to the host's `exit`: this process ends there, and an `on_exit` handler reports.
//! Env (xtask qemu_linker.rs sets it; unset prints SKIP): ATL_NB_ARCH, ATL_ENGINE_SO, ATL_NB_BOOT, ATL_NB_MAINEXE, ATL_NB_LINKER, ATL_NB_VDSO.
//! Expected values come from main_exe.c, never from the engine: main returns `MAIN_STATUS` 0x2a when the thread round trip held, else the failing step 1..5 (6..9: the workers' exit work; 8 = not "BACK" at the pthread_exit worker's join, 9 = not "DEK" at the returning worker's; break `exit_skip_cleanup` = 8).
//! Checks: (s) started, (c) thread churn before main (tests/common/churn.rs: 2000 `fx_join_one` threads give back every reserve block, heap and resident growth <= 2048 B/thread), (n) the resolver round (`fx_dns`: getaddrinfo walk + free, getnameinfo, gethostbyname/addr, gai_strerror, every field in the guest's own layout) answers 0 twice and leaves the engine's window count where it was ([`resolver`]), (e) the guest's exit reached the host's (the handler ran), (v) the status is 0x2a, (d) `fx_events` at the host's exit is [`EVENTS`] (each destructor and cleanup handler once; bionic pthread_exit.cpp order: thread_local destructors in reverse, then cleanup handlers, then key destructors), (r) no host pointer refused to a 32-bit guest (`ILP32_REFUSED` = 0). Output `qemu-main: (x) ok` / `(x) FAIL <why>`; the process exits 0 when all pass.
//! A guest call to a trapped stub (break `main_exit_unbound`: no host side for `native_bridge_exit`; break `main_tls_dtor_unbound`: none for `native_bridge___cxa_thread_atexit_impl`) aborts naming it, so (e) never prints.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "calls the guest's fx_run through Guest::call and registers an on_exit handler"
)]

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use atl_native_bridge::engine::{Config, Guest, ILP32_REFUSED, RESERVED, qemu};
use object::{Object, ObjectSymbol};

#[path = "common/churn.rs"]
mod churn;
#[path = "common/provider.rs"]
mod provider;

/// main_exe.c `MAIN_STATUS`.
const MAIN_STATUS: i32 = 0x2a;
/// The churn's first worker argument (any value; main_exe.c `fx_join_one` answers arg + 1).
const JOIN_ARG: usize = 0x1000;

/// main_exe.c's ELF entry and its `fx_run`, `fx_join_one`, `fx_events` and `fx_dns` symbol values (the load bias is entry - e_entry).
static EXPECTED: OnceLock<(u64, u64, u64, u64, u64)> = OnceLock::new();
/// Check (n)'s result, read by the exit handler.
static DNS_OK: AtomicBool = AtomicBool::new(false);
/// The guest address of main_exe.c `fx_events` (symbol + load bias), set before fx_run; read by the exit handler.
static EVENTS_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// main_exe.c's thread-exit record at the host's exit: the pthread_exit worker's "BACK" and the returning worker's "DEK" (main checked both, status 8 / 9 otherwise), then the main thread's two destructors in reverse registration, once each: "nm".
const EVENTS: &[u8] = b"BACKDEKnm";
/// Check (c)'s result, read by the exit handler.
static CHURN_OK: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    /// glibc: runs at `exit` with its status, before the process ends.
    fn on_exit(f: extern "C" fn(i32, *mut c_void), arg: *mut c_void) -> i32;
}

fn guest_arch() -> qemu::Arch {
    std::env::var("ATL_NB_ARCH")
        .ok()
        .and_then(|a| qemu::Arch::of_isa(&a))
        .unwrap_or(qemu::Arch::Aarch64)
}

fn check(letter: char, result: Result<(), String>) -> bool {
    match &result {
        Ok(()) => println!("qemu-main: ({letter}) ok"),
        Err(why) => println!("qemu-main: ({letter}) FAIL {why}"),
    }
    let _ = std::io::stdout().flush();
    result.is_ok()
}

/// The host exit the guest's reached: the checks, then the process's own status (0 = all ok), replacing the guest's.
extern "C" fn at_exit(status: i32, _: *mut c_void) {
    let refused = ILP32_REFUSED.load(Ordering::Relaxed);
    println!("qemu-main: values status {status:#x} ilp32 refused {refused}");
    let mut ok = CHURN_OK.load(Ordering::Relaxed) && DNS_OK.load(Ordering::Relaxed);
    ok &= check('e', Ok(()));
    ok &= check(
        'v',
        if status == MAIN_STATUS {
            Ok(())
        } else {
            Err(format!(
                "exit status {status:#x}, expected {MAIN_STATUS:#x} (1..5: main_exe.c thread step that failed)"
            ))
        },
    );
    // glibc's exit runs the thread-exit destructors (__call_tls_dtors) before on_exit handlers, so the main thread's have run by now
    let at = EVENTS_AT.load(Ordering::Relaxed);
    // SAFETY: fx_events, a 16-byte guest array in this address space (guest addresses are host addresses), NUL-terminated by its spare byte.
    let events = unsafe { std::ffi::CStr::from_ptr(at as *const libc::c_char) }.to_bytes();
    ok &= check(
        'd',
        if events == EVENTS {
            Ok(())
        } else {
            Err(format!(
                "thread-exit destructors ran {:?}, expected {:?}",
                String::from_utf8_lossy(events),
                String::from_utf8_lossy(EVENTS)
            ))
        },
    );
    ok &= check(
        'r',
        if refused == 0 {
            Ok(())
        } else {
            Err(format!("{refused} host pointers refused"))
        },
    );
    // SAFETY: ends the process at once; nothing after the guest's exit needs to run.
    unsafe { libc::_exit(i32::from(!ok)) }
}

/// Check (n): main_exe.c `fx_dns` answers 0 twice, and the engine's window count after the second equals the count before it (the first makes the once-per-process gai_strerror copy and this thread's hostent block, which the second replaces in place). Break `dns_keep_list`: freeaddrinfo keeps its block, the count grows by the round's two lists.
fn resolver(guest: &mut Guest, fx_dns: usize) -> Result<(), String> {
    // SAFETY: fx_dns() takes no arguments and answers a failure bit set.
    let first = unsafe { guest.call(fx_dns, 0, 0, 0) };
    let before = RESERVED.load(Ordering::Relaxed);
    // SAFETY: as above.
    let second = unsafe { guest.call(fx_dns, 0, 0, 0) };
    let after = RESERVED.load(Ordering::Relaxed);
    println!("qemu-main: values dns {first:#x} {second:#x} windows {before} -> {after}");
    if first != 0 || second != 0 {
        return Err(format!(
            "fx_dns answered {first:#x} then {second:#x} (main_exe.c / resolver.h bits)"
        ));
    }
    if after != before {
        return Err(format!(
            "window count {before} -> {after} across one resolver round (a guest copy not given back)"
        ));
    }
    Ok(())
}

fn started(r: Result<Guest, String>) -> u64 {
    let Some(&(e_entry, fx_run, fx_join_one, fx_events, fx_dns)) = EXPECTED.get() else {
        return 2;
    };
    let mut guest = match r {
        Ok(g) => g,
        Err(e) => {
            check('s', Err(e));
            return 1;
        }
    };
    check('s', Ok(()));
    let bias = (guest.exe_entry as u64).wrapping_sub(e_entry);
    let run = bias.wrapping_add(fx_run) as usize;
    let join_one = bias.wrapping_add(fx_join_one) as usize;
    EVENTS_AT.store(bias.wrapping_add(fx_events) as usize, Ordering::Relaxed);
    // the churn before main: a failure here is printed, and the process status reports it (`CHURN_OK`)
    let c = check(
        'c',
        churn::churn(&mut guest, "qemu-main", |guest, k| {
            let arg = JOIN_ARG + k;
            // SAFETY: fx_join_one(arg) creates one guest thread and joins it.
            let got = unsafe { guest.call(join_one, arg, 0, 0) };
            if got == arg + 1 {
                Ok(())
            } else {
                Err(format!("thread {k}: fx_join_one({arg:#x}) = {got:#x}"))
            }
        }),
    );
    CHURN_OK.store(c, Ordering::Relaxed);
    let n = check(
        'n',
        resolver(&mut guest, bias.wrapping_add(fx_dns) as usize),
    );
    DNS_OK.store(n, Ordering::Relaxed);
    // SAFETY: registers a handler of the right signature; arg unused.
    unsafe { on_exit(at_exit, std::ptr::null_mut()) };
    // SAFETY: `run` is the executable's fx_run (no arguments, never returns: it exits).
    unsafe { guest.call(run, 0, 0, 0) };
    println!("qemu-main: (e) FAIL the guest's exit returned");
    1
}

fn main() -> ExitCode {
    let vars = [
        "ATL_ENGINE_SO",
        "ATL_NB_BOOT",
        "ATL_NB_MAINEXE",
        "ATL_NB_LINKER",
        "ATL_NB_VDSO",
    ]
    .map(|k| std::env::var_os(k).map(PathBuf::from));
    let [Some(so), Some(boot), Some(exe), Some(linker), Some(vdso)] = vars else {
        println!(
            "qemu-main: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_MAINEXE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    let expected = std::fs::read(&exe)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            let elf = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
            let sym = |want: &str| {
                elf.symbols()
                    .find(|s| s.name().is_ok_and(|n| n == want))
                    .map(|s| s.address())
                    .ok_or(format!("no symbol {want}"))
            };
            Ok((
                elf.entry(),
                sym("fx_run")?,
                sym("fx_join_one")?,
                sym("fx_events")?,
                sym("fx_dns")?,
            ))
        });
    match expected {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("qemu-main: FAIL fixture {}: {e}", exe.display());
            return ExitCode::from(2);
        }
    }
    // the NB directory: <nb>/native_bridge_guest_linker/linker{64,}; the stub libc beside it
    let nb = linker
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let cfg = Config {
        linker,
        vdso,
        exe,
        library_path: vec![
            nb.join("libnative_bridge_guest_libc"),
            nb.join("libdl"),
            nb.join("ld-android"),
            nb.join("libm"),
        ],
    };
    if let Err(e) = provider::load_provider(&nb) {
        println!("qemu-main: FAIL {e}");
        return ExitCode::from(2);
    }
    let r = qemu::run(guest_arch(), &so, &boot, cfg, started);
    println!("qemu-main: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
