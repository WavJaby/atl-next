// SPDX-License-Identifier: MIT
//! Q4-5 gate (QEMU bridge plan Q4-5), harness-less: the guest's dlopen path under QEMU. The real arm64 guest linker starts the main executable (`dl_exe.c`, PIE with DT_NEEDED on libc.so and libdl.so); `libc.so` resolves to the stub libc, whose constructor intercepts its stubs. After the start the host calls the executable's functions through the crossing: its `dlopen("libfix.so")` loads a guest library whose DT_NEEDED is `libdep.so`, and `fix_value()` runs through both.
//! Env (xtask qemu_linker.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (the engine boot executable, crates/atl-qemu-boot), ATL_NB_DLEXE (dl_exe.c, PIE), ATL_NB_DLLIBS (the directory holding libfix.so and libdep.so; break `skip_reloc` passes a copy of libdep.so with DT_ANDROID_RELRSZ zeroed, so its RELATIVE relocations are not applied), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images, `<nb>/libnative_bridge_guest_libc` among the guest library directories).
//! Expected values come from the fixtures' C source, never from the engine: libdep.c `table[1]` = 0x51a2 (dep_value, through the RELATIVE relocation of `self_addr`); libfix.c fix_ctor = dep_value() = 0x51a2 (run by the constructor after libdep's), fix_value() = dep_value() + 0x10 = 0x51b2. The function addresses come from the executable's ELF symbol table (`object`) plus the load bias (entry - e_entry).
//! Checks: (s) started, (i) the stub libc intercepted at least one stub (vdso BOUND > 0), (m) the guest's malloc(64) answered a block -- on a 32-bit guest below 4 GiB with no host pointer refused (`ILP32_REFUSED` = 0) --,(d) dlopen returned a handle, (y) dlsym found fix_value and fix_ctor_value, (c) libfix's constructor saw 0x51a2, (v) fix_value() = 0x51b2.
//! Output: `qemu-dlopen: (x) ok` per check, `(x) FAIL <why>` otherwise; exit 0 when all pass. A start error prints `qemu-dlopen: (s) FAIL <error>`.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "calls the guest's functions through Guest::call and reads the guest's dlerror string at its identity address"
)]

use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use atl_native_bridge::engine::{Config, Guest, ILP32_REFUSED, qemu, vdso};
use object::{Object, ObjectSymbol};

/// The dl_exe.c functions the host calls, in the order `SYMBOLS` lists them.
const SYMBOLS: [&str; 5] = [
    "fx_open",
    "fx_fix_value",
    "fx_fix_ctor_value",
    "fx_dlerror",
    "fx_malloc",
];
/// libdep.c `table[1]`, the value dep_value() returns when its RELATIVE relocation was applied.
const DEP_VALUE: u64 = 0x51a2;
/// libfix.c `fix_value() = dep_value() + 0x10`.
const FIX_OFFSET: u64 = 0x10;

/// What the checks compare against: the executable's entry and the addresses of its functions, relative to its load bias.
struct Expected {
    e_entry: u64,
    /// Symbol values, in `SYMBOLS` order.
    values: Vec<u64>,
}
static EXPECTED: OnceLock<Expected> = OnceLock::new();

/// The guest arch the harness drives: `ATL_NB_ARCH` (an arches.toml ISA, xtask `--arch`), arm64 when unset.
fn guest_arch() -> qemu::Arch {
    std::env::var("ATL_NB_ARCH")
        .ok()
        .and_then(|a| qemu::Arch::of_isa(&a))
        .unwrap_or(qemu::Arch::Aarch64)
}

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("qemu-dlopen: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-dlopen: ({letter}) FAIL {why}");
            false
        }
    }
}

fn expected(exe: &Path) -> Result<Expected, String> {
    let bytes = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let elf = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
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

/// The guest's NUL-terminated `dlerror()` text, or the empty string. Read at its identity address (guest allocations are host addresses).
fn dl_error(guest: &mut Guest, dlerror: usize) -> String {
    // SAFETY: `dlerror` is the executable's `fx_dlerror` (no arguments); the string it returns lives in the linker's allocator, at an identity address the host reads.
    let p = unsafe { guest.call(dlerror, 0, 0, 0) };
    if p == 0 {
        return String::new();
    }
    // SAFETY: `p` is the NUL-terminated message dlerror returned (see above).
    unsafe { CStr::from_ptr(p as *const c_char) }
        .to_string_lossy()
        .into_owned()
}

#[path = "common/provider.rs"]
mod provider;
use provider::load_provider;

/// BOOT's continuation: the started guest (or its error) against the expected values. The answer is the boot executable's exit status.
fn started(r: Result<Guest, String>) -> u64 {
    let Some(x) = EXPECTED.get() else {
        return 2;
    };
    let mut guest = match r {
        Ok(g) => g,
        Err(e) => {
            println!("qemu-dlopen: (s) FAIL {e}");
            return 1;
        }
    };
    println!("qemu-dlopen: (s) ok");
    let bias = (guest.exe_entry as u64).wrapping_sub(x.e_entry);
    let addr = |i: usize| bias.wrapping_add(x.values[i]) as usize;
    let (open, fix_value, ctor_value, dlerror, malloc) =
        (addr(0), addr(1), addr(2), addr(3), addr(4));
    let bound = vdso::BOUND.load(Ordering::Relaxed);
    let unbound = vdso::UNBOUND.load(Ordering::Relaxed);

    // SAFETY: `malloc` is the executable's fx_malloc (no arguments): malloc(64) on the guest.
    let block = unsafe { guest.call(malloc, 0, 0, 0) } as u64;
    // SAFETY: `open` is the executable's fx_open (no arguments): dlopen("libfix.so", RTLD_NOW) on the guest.
    let handle = unsafe { guest.call(open, 0, 0, 0) };
    let refused = ILP32_REFUSED.load(Ordering::Relaxed);
    let mut ok = true;
    ok &= check(
        'i',
        if bound > 0 {
            Ok(())
        } else {
            Err("no stub intercepted: the stub libc's constructor did not run".into())
        },
    );
    // a 32-bit guest's heap is its own allocator's (NBS patch 0012), below 4 GiB, and no host pointer result was refused on the way (dispatch.rs `ilp32_narrow`); an aarch64 guest's block may be the host's
    let wide = guest_arch().pointer_bits() == 64;
    ok &= check(
        'm',
        if block != 0 && (wide || block <= u64::from(u32::MAX)) && refused == 0 {
            Ok(())
        } else {
            Err(format!(
                "malloc(64) = {block:#x}, host pointers refused {refused}"
            ))
        },
    );
    ok &= check(
        'd',
        if handle != 0 {
            Ok(())
        } else {
            Err(format!(
                "dlopen(libfix.so) returned NULL: {}",
                dl_error(&mut guest, dlerror)
            ))
        },
    );
    // SAFETY: `fix_value` and `ctor_value` are the executable's dlsym helpers, called with the handle dlopen returned (or 0, which dlsym answers with NULL).
    let (fix_fn, ctor_fn) = unsafe {
        (
            guest.call(fix_value, handle, 0, 0),
            guest.call(ctor_value, handle, 0, 0),
        )
    };
    ok &= check(
        'y',
        if fix_fn != 0 && ctor_fn != 0 {
            Ok(())
        } else {
            Err(format!(
                "dlsym: fix_value {fix_fn:#x}, fix_ctor_value {ctor_fn:#x}: {}",
                dl_error(&mut guest, dlerror)
            ))
        },
    );
    let (ctor, value) = if fix_fn != 0 && ctor_fn != 0 {
        // SAFETY: both are the guest's own functions of libfix.so (no arguments), returned by dlsym above.
        unsafe {
            (
                guest.call(ctor_fn, 0, 0, 0) as u64,
                guest.call(fix_fn, 0, 0, 0) as u64,
            )
        }
    } else {
        (0, 0)
    };
    println!(
        "qemu-dlopen: values handle {handle:#x} ctor {ctor:#x} value {value:#x} malloc {block:#x} stubs bound {bound} unbound {unbound} ilp32 refused {refused}"
    );
    ok &= check(
        'c',
        if ctor == DEP_VALUE {
            Ok(())
        } else {
            Err(format!(
                "fix_ctor {ctor:#x}, expected {DEP_VALUE:#x} (libdep's relocated value)"
            ))
        },
    );
    ok &= check(
        'v',
        if value == DEP_VALUE + FIX_OFFSET {
            Ok(())
        } else {
            Err(format!(
                "fix_value() {value:#x}, expected {:#x}",
                DEP_VALUE + FIX_OFFSET
            ))
        },
    );
    u64::from(!ok)
}

fn main() -> ExitCode {
    let vars = [
        "ATL_ENGINE_SO",
        "ATL_NB_BOOT",
        "ATL_NB_DLEXE",
        "ATL_NB_DLLIBS",
        "ATL_NB_LINKER",
        "ATL_NB_VDSO",
    ]
    .map(|k| std::env::var_os(k).map(PathBuf::from));
    let [
        Some(so),
        Some(boot),
        Some(exe),
        Some(dllibs),
        Some(linker),
        Some(vdso_path),
    ] = vars
    else {
        println!(
            "qemu-dlopen: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_DLEXE / ATL_NB_DLLIBS / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    match expected(&exe) {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("qemu-dlopen: FAIL fixture: {e}");
            return ExitCode::from(2);
        }
    }
    // the NB directory: the linker is at <nb>/native_bridge_guest_linker/linker{64,}; the stub libc, libdl, ld-android and libm sit beside it
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
            dllibs,
            nb.join("libnative_bridge_guest_libc"),
            nb.join("libdl"),
            nb.join("ld-android"),
            nb.join("libm"),
        ],
    };
    if let Err(e) = load_provider(&nb) {
        println!("qemu-dlopen: FAIL {e}");
        return ExitCode::from(2);
    }
    let r = qemu::run(guest_arch(), &so, &boot, cfg, started);
    println!("qemu-dlopen: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
