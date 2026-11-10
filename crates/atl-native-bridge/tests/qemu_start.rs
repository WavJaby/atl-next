// SPDX-License-Identifier: MIT
//! Q4-4 gate (QEMU bridge plan Q4-4), harness-less: the real arm64 guest linker's `__linker_init` runs to completion under QEMU and returns the main executable's entry. The production start runs: `active::run` boots the engine on the boot executable (BOOT answers at once), attaches this thread, and runs `Guest::start` (map, place, register, argument block from QEMU's auxv, `__linker_init` on the scratch TCB, static TLS of the main thread).
//! Env (xtask qemu_linker.rs sets it; unset prints SKIP): ATL_ENGINE_SO (the engine `.so`), ATL_NB_BOOT (testdata/qemu-linker/boot.c, static), ATL_NB_EXE (testdata/qemu-linker/exe.c, PIE), ATL_NB_LINKER, ATL_NB_VDSO (the arm64 NB images).
//! Expected values come from the executable's ELF file (read here with `object`), the AArch64 TLS ABI and bionic's headers (`MIN_TLS_SLOT`), never from the engine: (e) the entry is `e_entry` plus a load bias at which /proc/self/maps shows the executable's first page; (t) one static-TLS report whose init image holds the executable's `.tdata` at TP + round_up(16, p_align) (ELF TLS variant I, AArch64), with `tpoff` at least bionic's slots below the TP; (h) `get_host_pthread` ran; (g) the host's GS base is unchanged and addresses are identity.
//! Output: `qemu-start: (x) ok` per check, `(x) FAIL <why>` otherwise; exit 0 when all pass. A start error prints `qemu-start: (s) FAIL <error>`.
#![allow(clippy::print_stdout, reason = "test harness output is its verdict")]
#![allow(
    unsafe_code,
    reason = "reads the host GS base and the guest's init image at its identity address"
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use atl_native_bridge::engine::{Config, Guest, active, thread, vdso};
use object::{Object, read::elf::ProgramHeader};

/// `ARCH_GET_GS` (asm/prctl.h), the arch_prctl code that reads GS base without FSGSBASE.
const ARCH_GET_GS: libc::c_int = 0x1004;
/// bionic's TCB words below the arm64 thread pointer: `MIN_TLS_SLOT` = -3 (bionic/libc/platform/bionic/tls_defines.h, `__aarch64__`), so `tpoff` is at least this many words.
const SLOTS_BELOW_TP: u64 = 3;
/// AArch64 ELF TLS (variant I): the executable's block starts at TP + round_up(TCB_SIZE, p_align), TCB_SIZE = 16 (AAELF64, "TLS").
const AARCH64_TCB_SIZE: u64 = 16;

/// What the checks compare against: the executable's ELF facts and the GS base before QEMU started.
struct Expected {
    exe: PathBuf,
    e_entry: u64,
    tls_align: u64,
    tls_memsz: u64,
    tdata: Vec<u8>,
    gs_before: usize,
}
static EXPECTED: OnceLock<Expected> = OnceLock::new();

fn gs_base() -> usize {
    let mut v: usize = 0;
    // SAFETY: arch_prctl(ARCH_GET_GS, &v) writes one word to `v`.
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_GET_GS, &raw mut v) };
    assert_eq!(rc, 0, "arch_prctl(ARCH_GET_GS)");
    v
}

fn check(letter: char, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => {
            println!("qemu-start: ({letter}) ok");
            true
        }
        Err(why) => {
            println!("qemu-start: ({letter}) FAIL {why}");
            false
        }
    }
}

fn expected(exe: &Path) -> Result<Expected, String> {
    let bytes = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let elf = object::read::elf::ElfFile64::<object::Endianness>::parse(&*bytes)
        .map_err(|e| e.to_string())?;
    let endian = elf.endian();
    let tls = elf
        .elf_program_headers()
        .iter()
        .find(|p| p.p_type(endian) == object::elf::PT_TLS)
        .ok_or("exe has no PT_TLS")?;
    let off = tls.p_offset(endian) as usize;
    let filesz = tls.p_filesz(endian) as usize;
    let tdata = bytes
        .get(off..off + filesz)
        .ok_or("PT_TLS beyond the file")?
        .to_vec();
    Ok(Expected {
        exe: exe.canonicalize().map_err(|e| e.to_string())?,
        e_entry: elf.entry(),
        tls_align: tls.p_align(endian),
        tls_memsz: tls.p_memsz(endian),
        tdata,
        gs_before: gs_base(),
    })
}

/// The start of a /proc/self/maps line naming `path` at file offset 0, if `start` is one.
fn maps_exe_at(path: &Path, start: u64) -> bool {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return false;
    };
    let name = path.to_string_lossy();
    maps.lines().any(|l| {
        let mut f = l.split_whitespace();
        let range = f.next().unwrap_or("");
        let offset = f.nth(1).unwrap_or("");
        let file = f.nth(2).unwrap_or("");
        let lo = range.split('-').next().unwrap_or("");
        u64::from_str_radix(lo, 16) == Ok(start)
            && u64::from_str_radix(offset, 16) == Ok(0)
            && file == name
    })
}

fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align.max(1)) * align.max(1)
}

/// BOOT's continuation: the started guest (or its error) against the expected values. The answer is the boot executable's exit status.
fn started(r: Result<Guest, String>) -> u64 {
    let Some(x) = EXPECTED.get() else {
        return 2;
    };
    let guest = match r {
        Ok(g) => g,
        Err(e) => {
            println!("qemu-start: (s) FAIL {e}");
            return 1;
        }
    };
    println!("qemu-start: (s) ok");
    let tls = thread::static_tls();
    println!(
        "qemu-start: values entry {:#x} e_entry {:#x} tls size {:#x} tpoff {:#x} exe p_align {} host_pthread calls {} stubs bound {} unbound {}",
        guest.exe_entry,
        x.e_entry,
        tls.map_or(0, |t| t.size),
        tls.map_or(0, |t| t.tpoff),
        x.tls_align,
        vdso::HOST_PTHREADS.load(Ordering::Relaxed),
        vdso::BOUND.load(Ordering::Relaxed),
        vdso::UNBOUND.load(Ordering::Relaxed)
    );
    let mut ok = true;
    ok &= check('e', {
        let entry = guest.exe_entry as u64;
        let bias = entry.wrapping_sub(x.e_entry);
        if bias % page() != 0 {
            Err(format!(
                "entry {entry:#x} - e_entry {:#x}: bias not page aligned",
                x.e_entry
            ))
        } else if !maps_exe_at(&x.exe, bias) {
            Err(format!(
                "entry {entry:#x}: no mapping of {} at bias {bias:#x}",
                x.exe.display()
            ))
        } else {
            Ok(())
        }
    });
    ok &= check(
        't',
        (|| {
            let reports = vdso::STATIC_TLS_CONFIGS.load(Ordering::Relaxed);
            if reports != 1 {
                return Err(format!("{reports} static-TLS reports, expected 1"));
            }
            let t = thread::static_tls().ok_or("no static TLS recorded")?;
            let below = SLOTS_BELOW_TP * std::mem::size_of::<u64>() as u64;
            let exe_at = t.tpoff as u64 + round_up(AARCH64_TCB_SIZE, x.tls_align);
            if (t.tpoff as u64) < below {
                return Err(format!(
                    "tpoff {:#x} below bionic's {below:#x} of slots",
                    t.tpoff
                ));
            }
            if (t.size as u64) < exe_at + x.tls_memsz {
                return Err(format!(
                    "size {:#x} < exe block end {:#x}",
                    t.size,
                    exe_at + x.tls_memsz
                ));
            }
            // SAFETY: init_img holds `size` bytes (NBS `new char[layout.size()]`), at an identity address; the range is inside it (checked above).
            let img = unsafe {
                std::slice::from_raw_parts(
                    (t.init_img + exe_at as usize) as *const u8,
                    x.tdata.len(),
                )
            };
            if img != x.tdata.as_slice() {
                return Err(format!(
                    "init_img at {exe_at:#x}: {img:02x?}, exe .tdata {:02x?}",
                    x.tdata
                ));
            }
            Ok(())
        })(),
    );
    ok &= check('h', {
        let n = vdso::HOST_PTHREADS.load(Ordering::Relaxed);
        if n >= 1 {
            Ok(())
        } else {
            Err("get_host_pthread never ran".into())
        }
    });
    ok &= check('g', {
        let after = gs_base();
        if after == x.gs_before {
            Ok(())
        } else {
            Err(format!("GS base {:#x} -> {after:#x}", x.gs_before))
        }
    });
    u64::from(!ok)
}

fn page() -> u64 {
    // SAFETY: no memory effects.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
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
            "qemu-start: SKIP (ATL_ENGINE_SO / ATL_NB_BOOT / ATL_NB_EXE / ATL_NB_LINKER / ATL_NB_VDSO unset)"
        );
        return ExitCode::SUCCESS;
    };
    match expected(&exe) {
        Ok(x) => {
            let _ = EXPECTED.set(x);
        }
        Err(e) => {
            println!("qemu-start: FAIL fixture: {e}");
            return ExitCode::from(2);
        }
    }
    let lib_dir = linker
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let cfg = Config {
        linker,
        vdso: vdso_path,
        exe,
        library_path: vec![lib_dir],
    };
    let r = active::run(&so, &boot, cfg, started);
    println!("qemu-start: FAIL QEMU main returned {r:?}");
    ExitCode::from(2)
}
