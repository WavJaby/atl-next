// SPDX-License-Identifier: MIT
//! Q5-6 gate (QEMU bridge plan Q5-6), harness-less: guest threads under QEMU. The real arm64 guest linker starts the main executable (`xtask/testdata/qemu-thread/fixture.c`, PIE, DT_NEEDED on libc.so, which resolves to the stub libc); the host calls its `qt_*` functions through the crossing.
//! The guest's threads are glibc threads (the stub libc's `pthread_create` is the host shim). Each attaches its own engine CPU on its first guest call, runs its routine, and at exit runs its guest key destructor through a crossing on that thread.
//! Env (xtask qemu_thread.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (the engine boot executable, crates/atl-qemu-boot), ATL_NB_THREADEXE (fixture.c, PIE), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images; `<nb>/libnative_bridge_guest_libc` among the guest library directories).
//! Expected values come from the property store and the fixture's C source, never from the engine: xtask seeds `atl.qt.p.<i>` and `atl.qt.expect` into the area (ATL_PROP_AREA), and the provider's foreach must count exactly `atl.qt.expect` of them; qt_join(x) = x + 1000 (fixture.c `QT_JOIN_OFFSET`); the destructor stores the value its thread's key held at exit.
//! Checks: (s) started, (p) foreach counts the seeded properties, (j) pthread_create's routine got its argument and its result reached pthread_join, (d) the key destructor ran at thread exit with the thread's value.
//! Output: `qemu-thread: (x) ok` per check, `(x) FAIL <why>` otherwise, and one `qemu-thread: call-cost` line (not a check: the round trip of a no-op guest call on this thread). Exit 0 when all pass.
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
const SYMBOLS: [&str; 6] = [
    "qt_props_count",
    "qt_props_expected",
    "qt_join",
    "qt_key_run",
    "qt_last_value",
    "qt_nop",
];
/// The argument `qt_join` is given; its routine returns it plus `JOIN_OFFSET`.
const JOIN_ARG: u64 = 41;
/// fixture.c `QT_JOIN_OFFSET`: the routine's addend.
const JOIN_OFFSET: u64 = 1000;
/// The value `qt_key_run`'s thread sets its key to; the destructor must store it.
const KEY_VALUE: u64 = 0x7e57;
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
    let (props_count, props_expected, join, key_run, last_value, nop) =
        (addr(0), addr(1), addr(2), addr(3), addr(4), addr(5));
    let mut ok = true;

    // SAFETY: the fixture's functions, each with at most one integer argument (the executable's ELF symbols, `expected`); Guest::call attaches this thread's CPU.
    // qt_props_count runs the provider's foreach, whose callback calls back into the guest (read_callback).
    let count = unsafe { guest.call(props_count, 0, 0, 0) } as u64;
    // SAFETY: as above; the count the gate seeded, read through __system_property_get.
    let expected_count = unsafe { guest.call(props_expected, 0, 0, 0) } as u64;
    ok &= check(
        'p',
        if expected_count > 0 && count == expected_count {
            Ok(())
        } else {
            Err(format!(
                "foreach counted {count} seeded properties, atl.qt.expect says {expected_count}"
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
    let r = qemu::run(&so, &boot, cfg, started);
    println!("qemu-thread: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
