// SPDX-License-Identifier: MIT
//! Q5-6 gate (QEMU bridge plan Q5-6), harness-less: guest threads under QEMU. The real arm64 guest linker starts the main executable (`xtask-harness/testdata/qemu-thread/fixture.c`, PIE, DT_NEEDED on libc.so, which resolves to the stub libc); the host calls its `qt_*` functions through the crossing.
//! The guest's threads are glibc threads (the stub libc's `pthread_create` is the host shim). Each attaches its own engine CPU on its first guest call, runs its routine, and at exit runs its guest key destructor through a crossing on that thread.
//! Env (xtask qemu_thread.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (the engine boot executable, crates/atl-qemu-boot), ATL_NB_THREADEXE (fixture.c, PIE), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images; `<nb>/libnative_bridge_guest_libc` among the guest library directories).
//! Expected values come from the property store and the fixture's C source, never from the engine: xtask seeds `atl.qt.p.<i>` and `atl.qt.expect` into the area (ATL_PROP_AREA), and the provider's foreach must count exactly `atl.qt.expect` of them; qt_join(x) = x + 1000 (fixture.c `QT_JOIN_OFFSET`); the destructor stores the value its thread's key held at exit.
//! Checks: (s) started, (p) foreach counts the seeded properties, (j) pthread_create's routine got its argument and its result reached pthread_join, (d) the key destructor ran at thread exit with the thread's value, (g) a guest thread's `pthread_getattr_np(pthread_self())` contains its sp and reports the size its attr asked for (fixture.c `QT_SMALL_STACK`), or 1 MiB without an attr, with a guard of one host page (the engine's default, bionic `PTHREAD_GUARD_SIZE`), (i) two host threads in concurrent down calls each keep a 64 KiB stack pattern intact while the other's is live, with sp in their own reported range and the two ranges disjoint, (k) guest setjmp / longjmp round trip from a deeper frame (fixture.c `QT_JMP_VALUE`, volatile local kept), (m) siglongjmp restores the mask sigsetjmp(b, 1) saved (SIGUSR1 unblocked again). Both run the guest libc's bionic setjmp.S: the NBS patch drops the arm64 setjmp stubs, so no host side exists. (x) a guest thread's pthread_exit, one frame below its routine, hands join its value (fixture.c `QT_EXIT_OFFSET`) and its key destructor runs with the thread's value. (r) getaddrinfo("localhost", "80", AF_INET stream, AI_CANONNAME) read in bionic's addrinfo layout (family, socktype, protocol, addrlen, port 80, 127.0.0.1, canonname), getnameinfo numeric back to "127.0.0.1" / "80", freeaddrinfo, gethostbyname("127.0.0.1"), gethostbyaddr(127.0.0.1) names it (engine/netdb.rs; fixture.c `qt_resolve`). (q) tr-first blocker 4: one guest handle per thread (engine/thread.rs `THREADS`): pthread_kill(handle, SIGUSR2) runs the handler on the target (its pthread_self equals the handle), the target's pthread_self equals pthread_create's handle, pthread_gettid_np(handle) is the target's gettid, join hands back the target's pthread_self, the creator's own handle differs (fixture.c `qt_kill`). (c) thread churn: created + joined guest threads give back every `Active::reserve` block (engine `unmap_window`, ABI 4.2; break `qt_keep_windows`) and leave no host heap or resident growth past `CHURN_BOUND` per thread (QEMU patch 0007, the ARM CPU finalizer; engine break `keep-cpu`).
//! Output: `qemu-thread: (x) ok` per check, `(x) FAIL <why>` otherwise, and one `qemu-thread: call-cost` line (not a check: the round trip of a no-op guest call on this thread). Exit 0 when all pass.
#![expect(
    clippy::disallowed_types,
    reason = "a test: check (g) runs as a child of this test binary"
)]
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "calls the guest's functions through Guest::call; loads the provider with dlopen"
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Instant;

use atl_native_bridge::engine::{Config, Guest, qemu};
use object::{Object, ObjectSymbol, read::elf::ElfFile64};

/// The fixture functions the host calls, in the order `Expected::values` holds them.
const SYMBOLS: [&str; 20] = [
    "qt_props_count",
    "qt_props_expected",
    "qt_join",
    "qt_key_run",
    "qt_last_value",
    "qt_nop",
    "qt_stack_run",
    "qt_small_stack",
    "qt_size_seen",
    "qt_guard_seen",
    "qt_fill_reset",
    "qt_stack_fill",
    "qt_fill_base_at",
    "qt_fill_size_at",
    "qt_jmp",
    "qt_sigjmp",
    "qt_exit_run",
    "qt_resolve",
    "qt_kill",
    "qt_prop_unset",
];
/// bionic `PTHREAD_STACK_SIZE_DEFAULT` order (1 MiB; R-10 plan D2): the guest stack of a thread created without an attr.
const DEFAULT_STACK: u64 = 1 << 20;
/// Concurrent down-call threads of check (i); fixture.c `qt_stack_fill` waits for exactly two.
const FILLERS: usize = 2;
/// The argument `qt_join` is given; its routine returns it plus `JOIN_OFFSET`.
const JOIN_ARG: u64 = 41;
/// fixture.c `QT_JOIN_OFFSET`: the routine's addend.
const JOIN_OFFSET: u64 = 1000;
/// The value `qt_key_run`'s thread sets its key to; the destructor must store it.
const KEY_VALUE: u64 = 0x7e57;
/// fixture.c `qt_jmp`: `QT_JMP_VALUE` (7) from the second setjmp return, plus 100 for its one first-return pass.
const JMP_WANT: u64 = 7 + 100;
/// The argument `qt_exit_run` is given: its thread's key value and, plus `EXIT_OFFSET`, its pthread_exit value.
const EXIT_ARG: u64 = 0x5eed;
/// fixture.c `QT_EXIT_OFFSET`: pthread_exit's addend.
const EXIT_OFFSET: u64 = 2000;
/// Round trips timed for the call-cost line.
const COST_CALLS: u32 = 1000;

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
            println!("qemu-thread: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-thread: ({letter}) FAIL {why}");
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

/// Loads the bionic provider every ART process loads (`libatl_bionic`, built next to the NB output at `<out>/crates/atl-bionic/lib`, `<nb>` = `<out>/nb/<arch>`). The stubs bind its property functions by name with RTLD_NOLOAD (bindings.rs `Host::Provider`), so it must be in the process before the guest starts.
fn load_provider(nb: &Path) -> Result<(), String> {
    let lib = nb
        .ancestors()
        .map(|a| a.join("crates/atl-bionic/lib/libatl_bionic.so.0.0.0"))
        .find(|p| p.is_file())
        .ok_or("no crates/atl-bionic/lib/libatl_bionic.so.0.0.0 above the NB directory (<out>/nb/<arch>, <out>/nb-graph-<arch>/nb/<arch>)")?;
    let path =
        std::ffi::CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: dlopen of a NUL-terminated path; the provider's constructors are its own.
    let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        return Err(format!("dlopen {}: failed", lib.display()));
    }
    Ok(())
}

/// BOOT's continuation: the started guest (or its error) against the expected values. The answer is the boot executable's exit status.
fn started(r: Result<Guest, String>) -> u64 {
    let Some(x) = EXPECTED.get() else {
        return 2;
    };
    let mut guest = match r {
        Ok(g) => g,
        Err(e) => {
            println!("qemu-thread: (s) FAIL {e}");
            return 1;
        }
    };
    println!("qemu-thread: (s) ok");
    let bias = (guest.exe_entry as u64).wrapping_sub(x.e_entry);
    let addr = |i: usize| bias.wrapping_add(x.values[i]) as usize;
    if std::env::var_os(STACK_CHILD).is_some() {
        let mut bad = false;
        for (c, r) in [
            ('g', stacks(&mut guest, addr(6), addr(7), addr(8), addr(9))),
            (
                'i',
                isolation(&mut guest, addr(10), addr(11), addr(12), addr(13)),
            ),
        ] {
            println!(
                "{STACK_LINE}{c} {}",
                r.as_ref().err().map_or("ok", String::as_str)
            );
            bad |= r.is_err();
        }
        return u64::from(bad);
    }
    let (props_count, props_expected, join, key_run, last_value, nop) =
        (addr(0), addr(1), addr(2), addr(3), addr(4), addr(5));
    let mut ok = true;

    // SAFETY: the fixture's functions, each with at most one integer argument (the executable's ELF symbols, `expected`); Guest::call attaches this thread's CPU.
    // qt_props_count runs the provider's foreach, whose callback calls back into the guest (read_callback).
    let count = unsafe { guest.call(props_count, 0, 0, 0) } as u64;
    // SAFETY: as above; the count the gate seeded, read through __system_property_get.
    let expected_count = unsafe { guest.call(props_expected, 0, 0, 0) } as u64;
    // SAFETY: as above; reads a name nobody set (its prop:miss line is the xtask side's check, qemu_thread.rs `UNSET`).
    let unset_len = unsafe { guest.call(addr(19), 0, 0, 0) } as u64;
    ok &= check(
        'p',
        if expected_count > 0 && count == expected_count && unset_len == 0 {
            Ok(())
        } else {
            Err(format!(
                "foreach counted {count} seeded properties, atl.qt.expect says {expected_count}; unset read {unset_len} bytes"
            ))
        },
    );

    // SAFETY: as above; qt_join creates one thread running qt_routine and joins it.
    let joined = unsafe { guest.call(join, JOIN_ARG as usize, 0, 0) } as u64;
    let want = JOIN_ARG + JOIN_OFFSET;
    ok &= check(
        'j',
        if joined == want {
            Ok(())
        } else {
            Err(format!("join returned {joined:#x}, expected {want:#x}"))
        },
    );

    // SAFETY: as above; qt_key_run creates a thread that sets a key, and joins it; qt_last_value reads what the destructor stored.
    let ran = unsafe { guest.call(key_run, KEY_VALUE as usize, 0, 0) } as u64;
    // SAFETY: as above.
    let last = unsafe { guest.call(last_value, 0, 0, 0) } as u64;
    ok &= check(
        'd',
        if ran == 0 && last == KEY_VALUE {
            Ok(())
        } else {
            Err(format!(
                "key_run returned {ran:#x}, destructor stored {last:#x}, expected {KEY_VALUE:#x}"
            ))
        },
    );

    // SAFETY: as above; guest bionic setjmp / longjmp inside the guest call.
    let jmp = unsafe { guest.call(addr(14), 0, 0, 0) } as u64;
    ok &= check(
        'k',
        if jmp == JMP_WANT {
            Ok(())
        } else {
            Err(format!("qt_jmp returned {jmp:#x}, expected {JMP_WANT:#x}"))
        },
    );
    // SAFETY: as above; sigsetjmp / siglongjmp and sigprocmask, guest code.
    let mask = unsafe { guest.call(addr(15), 0, 0, 0) } as i64;
    ok &= check(
        'm',
        if mask == 0 {
            Ok(())
        } else {
            Err(format!(
                "qt_sigjmp returned {mask} (0 restored, 1 SIGUSR1 still blocked, <0 a mask call failed)"
            ))
        },
    );

    // SAFETY: as above; qt_exit_run creates a thread that sets the key and calls pthread_exit, and joins it.
    let exited = unsafe { guest.call(addr(16), EXIT_ARG as usize, 0, 0) } as u64;
    // SAFETY: as above.
    let last = unsafe { guest.call(last_value, 0, 0, 0) } as u64;
    let want = EXIT_ARG + EXIT_OFFSET;
    ok &= check(
        'x',
        if exited == want && last == EXIT_ARG {
            Ok(())
        } else {
            Err(format!(
                "join reported {exited:#x} (expected {want:#x}), destructor stored {last:#x} (expected {EXIT_ARG:#x})"
            ))
        },
    );

    // SAFETY: as above; qt_resolve runs the resolver stubs, which cross to their host sides.
    let resolve = unsafe { guest.call(addr(17), 0, 0, 0) } as u64;
    ok &= check(
        'r',
        if resolve == 0 {
            Ok(())
        } else {
            Err(format!(
                "qt_resolve failed steps {resolve:#b} (bit 1 getaddrinfo, 2 family/socktype/protocol, 3 addrlen, 4 sockaddr, 5 canonname, 6 getnameinfo, 7 its text, 8 gethostbyname, 9 gethostbyaddr)"
            ))
        },
    );

    // SAFETY: as above; qt_kill creates a thread, signals it with pthread_kill, compares handles and joins it.
    let kill = unsafe { guest.call(addr(18), 0, 0, 0) } as u64;
    ok &= check(
        'q',
        if kill == 0 {
            Ok(())
        } else {
            Err(format!(
                "qt_kill failed steps {kill:#b} (bit 1 sigaction, 2 create, 3 routine never started, 4 pthread_kill rc, 5 handler never ran, 6 handler on another thread, 7 routine pthread_self != handle, 8 pthread_gettid_np, 9 join rc, 10 join value != handle, 11 creator equals the handle)"
            ))
        },
    );

    ok &= check('c', churn(&mut guest, join));

    // SAFETY: page size, no memory effects.
    let [g, i] = stack_child();
    ok &= check('g', g);
    ok &= check('i', i);

    let t = Instant::now();
    for _ in 0..COST_CALLS {
        // SAFETY: qt_nop takes no arguments and returns 0.
        unsafe { guest.call(nop, 0, 0, 0) };
    }
    let ns = t.elapsed().as_nanos() / u128::from(COST_CALLS);
    println!(
        "qemu-thread: call-cost {ns} ns per guest call round trip (qt_nop, {COST_CALLS} calls, this thread)"
    );
    u64::from(!ok)
}

/// Guest threads check (c) creates and joins after its warm-up (~1.5 s). Enough that a leak above [`CHURN_BOUND`] lifts the totals far past their noise: a 64 MiB malloc arena / 2000 = 32 KiB, and the leak QEMU's ARM CPU finalizer had (31.8 KB/thread) grew in-use heap by 64 MB here.
const CHURN: usize = 2_000;
/// Threads before (c)'s baseline: glibc's stack cache and QEMU's first-use allocations settle in them.
const CHURN_WARMUP: usize = 200;
/// Largest growth per joined thread check (c) accepts, of glibc's in-use heap (`mallinfo2().uordblks`) and of `VmRSS`, in bytes. Measured with the teardown complete: heap 0 net over 9000 threads, one window up to 529 B/thread (a ~1 MB transient), resident up to 624 B/thread; the smallest per-thread leak seen, the ARM CPU's cpreg lists, was 5.8 KB/thread (the whole ARM finalizer leak 31.8 KB). 2 KiB = 3x the noise and under half that leak.
const CHURN_BOUND: i128 = 2048;

/// glibc's in-use heap bytes, all arenas (`mallinfo2().uordblks`; struct mallinfo2, malloc.h: ten size_t fields, uordblks the eighth).
fn heap_in_use() -> usize {
    #[repr(C)]
    struct Mallinfo2 {
        fields: [usize; 10],
    }
    unsafe extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: glibc's query, no arguments; returns the struct by value.
    unsafe { mallinfo2() }.fields[7]
}

/// `VmRSS` and `VmSize` of this process in bytes (`/proc/self/status`, kB fields).
fn vm() -> Result<(u64, u64), String> {
    let s = std::fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .ok_or(format!("/proc/self/status: no {name}"))
    };
    Ok((field("VmRSS:")?, field("VmSize:")?))
}

/// Check (c), thread churn: [`CHURN`] guest threads created and joined (`qt_join`) give back every `Active::reserve` block (engine `unmap_window`): `RESERVED` returns to its start value; and leave nothing on the host heap: in-use heap and resident grow at most [`CHURN_BOUND`] per thread (break `keep-cpu`: the CPU teardown skipped). VmSize is printed, not judged: glibc may add an arena when threads overlap.
fn churn(guest: &mut Guest, join: usize) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let run = |guest: &mut Guest, n: usize| -> Result<(), String> {
        for k in 0..n {
            // SAFETY: qt_join creates one thread running qt_routine and joins it (as check j).
            let got = unsafe { guest.call(join, JOIN_ARG as usize, 0, 0) } as u64;
            if got != JOIN_ARG + JOIN_OFFSET {
                return Err(format!("thread {k}: join returned {got:#x}"));
            }
        }
        Ok(())
    };
    run(guest, CHURN_WARMUP)?;
    let reserved = atl_native_bridge::engine::RESERVED.load(Ordering::Relaxed);
    let heap = heap_in_use();
    let (rss, size) = vm()?;
    let t = Instant::now();
    run(guest, CHURN)?;
    let ms = t.elapsed().as_millis();
    let left = atl_native_bridge::engine::RESERVED.load(Ordering::Relaxed);
    let heap2 = heap_in_use();
    let (rss2, size2) = vm()?;
    let per = |a: u64, b: u64| (i128::from(b) - i128::from(a)) / CHURN as i128;
    let heap_per = per(heap as u64, heap2 as u64);
    let rss_per = per(rss, rss2);
    println!(
        "qemu-thread: churn {CHURN} threads in {ms} ms: reserved {reserved} -> {left}, heap {heap_per} B/thread, rss {rss_per} B/thread, vmsize {} B/thread (bound {CHURN_BOUND})",
        per(size, size2)
    );
    let mut why = Vec::new();
    if left != reserved {
        why.push(format!(
            "{} reserve blocks not given back",
            left as i128 - reserved as i128
        ));
    }
    if heap_per > CHURN_BOUND {
        why.push(format!("in-use heap grew {heap_per} B/thread"));
    }
    if rss_per > CHURN_BOUND {
        why.push(format!("resident grew {rss_per} B/thread"));
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "after {CHURN} joined threads: {} (bound {CHURN_BOUND} B/thread)",
            why.join(", ")
        ))
    }
}

/// Set on the child run of checks (g) and (i): it boots, runs only [`stacks`] and [`isolation`] and prints a [`STACK_LINE`] for each. A child, because a wrong `pthread_getattr_np` (glibc reading a bionic record as its own, break `qt_getattr_glibc`) may kill or hang the process: the parent turns either into FAIL lines.
const STACK_CHILD: &str = "ATL_QT_STACK_CHILD";
/// The child's verdict lines: this prefix, the check letter, a space, then `ok` or why not.
const STACK_LINE: &str = "qemu-thread-stack: ";
/// Bound on the child: a plain one boots and runs four threads in about a second; past this it hangs (glibc waiting on a lock word of a foreign struct) and is killed.
const STACK_CHILD_SECS: u64 = 30;
/// Poll interval while waiting for the child.
const STACK_POLL_MS: u64 = 50;

/// Checks (g) and (i) in a child run of this harness (same env + [`STACK_CHILD`]): each its verdict line, else how the child died or that it hung.
fn stack_child() -> [Result<(), String>; 2] {
    match stack_child_text() {
        Ok((text, status)) => ['g', 'i'].map(|c| {
            let tag = format!("{STACK_LINE}{c} ");
            match text.lines().find_map(|l| l.strip_prefix(tag.as_str())) {
                Some("ok") => Ok(()),
                Some(why) => Err(why.to_owned()),
                None => Err(format!("stack child printed no ({c}) verdict ({status})")),
            }
        }),
        Err(e) => [Err(e.clone()), Err(e)],
    }
}

/// Runs the child to its exit or the deadline (then kills it); its stdout and how it ended.
fn stack_child_text() -> Result<(String, String), String> {
    use std::io::Read;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .env(STACK_CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + std::time::Duration::from_secs(STACK_CHILD_SECS);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s.to_string();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break format!("hung past {STACK_CHILD_SECS} s, killed");
        }
        std::thread::sleep(std::time::Duration::from_millis(STACK_POLL_MS));
    };
    let mut text = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut text);
    }
    Ok((text, status))
}

/// Check (g)'s body: `qt_stack_run` with and without a `qt_small_stack` attr; each thread's sp inside its reported stack, size = the attr's (else [`DEFAULT_STACK`]), guard one host page.
fn stacks(
    guest: &mut Guest,
    run: usize,
    small: usize,
    size_seen: usize,
    guard_seen: usize,
) -> Result<(), String> {
    // SAFETY: page size, no memory effects.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    // SAFETY: fixture functions, as in `started`; qt_stack_run creates one thread and joins it.
    let small = unsafe { guest.call(small, 0, 0, 0) } as u64;
    for (arg, want) in [(1, small), (0, DEFAULT_STACK)] {
        // SAFETY: as above.
        let (rc, size, guard) = unsafe {
            (
                guest.call(run, arg, 0, 0) as i64,
                guest.call(size_seen, 0, 0, 0) as u64,
                guest.call(guard_seen, 0, 0, 0) as u64,
            )
        };
        if rc != 0 {
            return Err(format!(
                "attr {arg}: qt_stack_check step {rc} failed (1 getattr, 2 getstack, 3 sp outside)"
            ));
        }
        if size != want || guard != page {
            return Err(format!(
                "attr {arg}: reported size {size:#x} guard {guard:#x}, expected {want:#x} / {page:#x}"
            ));
        }
    }
    Ok(())
}

/// The guest, shared by the filler threads: `Guest::call` uses only the calling thread's own record.
struct Shared(*mut Guest);
// SAFETY: each thread's call runs on its own record and CPU (thread.rs `attach_current`); the Guest is not otherwise touched while they run.
unsafe impl Send for Shared {}
// SAFETY: as for Send.
unsafe impl Sync for Shared {}

/// Check (i): `FILLERS` host threads each make the down call `qt_stack_fill(slot)` at once; each must find its pattern intact and sp in its reported stack, and the reported stacks must be disjoint.
fn isolation(
    guest: &mut Guest,
    reset: usize,
    fill: usize,
    base_at: usize,
    size_at: usize,
) -> Result<(), String> {
    // SAFETY: fixture functions, as in `started`.
    unsafe { guest.call(reset, 0, 0, 0) };
    let shared = Shared(guest);
    let results: Vec<i64> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..FILLERS)
            .map(|slot| {
                let g = &shared;
                // SAFETY: as for Shared; qt_stack_fill takes its slot.
                s.spawn(move || unsafe { (*g.0).call(fill, slot, 0, 0) } as i64)
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().unwrap_or(i64::MIN))
            .collect()
    });
    let mut ranges = Vec::new();
    for (slot, r) in results.iter().enumerate() {
        if *r != 0 {
            return Err(format!(
                "filler {slot}: {r:#x} (low 32 bits: mismatched bytes; high: qt_stack_check step; -1: partner never came)"
            ));
        }
        // SAFETY: as above.
        let (b, n) = unsafe {
            (
                guest.call(base_at, slot, 0, 0) as u64,
                guest.call(size_at, slot, 0, 0) as u64,
            )
        };
        ranges.push((b, b + n));
    }
    let (a, b) = (ranges[0], ranges[1]);
    if a.0 < b.1 && b.0 < a.1 {
        return Err(format!("stacks overlap: {a:x?} and {b:x?}"));
    }
    Ok(())
}

fn main() -> ExitCode {
    let vars = [
        "ATL_ENGINE_SO",
        "ATL_NB_BOOT",
        "ATL_NB_THREADEXE",
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
            "qemu-thread: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_THREADEXE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    match expected(&exe) {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("qemu-thread: FAIL fixture: {e}");
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
        println!("qemu-thread: FAIL {e}");
        return ExitCode::from(2);
    }
    let r = qemu::run(qemu::Arch::Aarch64, &so, &boot, cfg, started);
    println!("qemu-thread: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
