// SPDX-License-Identifier: MIT
//! `xtask test qemu-linker`: the QEMU crossing on the real arm64 NB images, three harnesses, each plain then with each of its breaks (QEMU bridge plan Q4-3, Q4-4, Q4-5).
//!
//! Library: the spike's `-shared` QEMU library (`qemu_spike::link`). Fixtures in `xtask/testdata/qemu-linker/`, built by the NDK clang: `fixture.c` (static, Q4-3), `boot.c` (static, the Q4-4 boot executable), `exe.c` (PIE without libc, the main executable the guest linker loads), `dl_exe.c` (PIE with DT_NEEDED on libc.so and libdl.so, the Q4-5 dlopen executable), `libdep.c` and `libfix.c` (the Q4-5 dlopen'd pair: libfix.so's DT_NEEDED is libdep.so).
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`, from `xtask native --host x86_64 --native-bridge arm64 nb:libc nb:native_bridge_guest_linker nb:libnative_bridge_vdso nb:libnative_bridge_guest_libc nb:libdl nb:libm nb:ld-android`): `native_bridge_guest_linker/linker64`, `libnative_bridge_vdso/libnative_bridge_vdso.so`, and the guest library directories `libnative_bridge_guest_libc`, `libdl`, `libm`, `ld-android` that the Q4-5 harness puts on the guest's library path.
//! Harnesses: `cargo test -p atl-native-bridge --features qemu-guest --test <name>`, harness-less; each header lists its env and checks.
//! - `qemu_linker` (Q4-3 placement): checks p, r, t, a, s. Breaks (`ATL_NB_BREAK`): `qe_site` (the vdso's placeholder search fails closed on call_guest), `qe_nomap` (no page registered: a fetch from a placed site faults), `qe_id` (a binding trap calls its neighbour's host function).
//! - `qemu_start` (Q4-4 linker init): checks s (started), e (entry), t (static TLS), h (get_host_pthread), g (GS base). Breaks: `qe_guest_base` (QEMU at a nonzero guest base: BOOT refuses), `qe_no_tls` (the static-TLS hook unpatched: the linker's `bl` branches to 0). The AT_RANDOM break (plan Q4-4 (b)) was never wired: it did not abort (Q4-5 log).
//! - `qemu_dlopen` (Q4-5 guest dlopen): checks s (started), i (the stub libc intercepted stubs), d (dlopen returned a handle), y (dlsym found both functions), c (libfix's constructor saw libdep's relocated value), v (fix_value()). Break: `skip_reloc` (harness-level: the run's libdep.so has DT_ANDROID_RELRSZ zeroed, so its packed RELATIVE relocations are not applied; no product cfg). The AT_RANDOM break was dropped: nothing on this path aborts without AT_RANDOM.
//!
//! Pass: every check prints ok and the run exits 0; each break must turn its run red. Output: `<out>/qemu/<target>-linux-user-pic/qemu-linker/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSection};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_spike;
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/qemu-linker";
const PACKAGE: &str = "atl-native-bridge";
/// Per harness run. A run that outlives it hangs (a guest fault QEMU does not end) and counts as failed.
const RUN_TIMEOUT_SECS: u32 = 240;
/// Harness-level break (no product cfg): the run's libdep.so has DT_ANDROID_RELRSZ zeroed, so its RELATIVE relocations are not applied.
const SKIP_RELOC: &str = "skip_reloc";

/// One harness: its cargo test, the prefix and letters of its check lines, its breaks.
struct Harness {
    test: &'static str,
    prefix: &'static str,
    checks: &'static [char],
    breaks: &'static [&'static str],
}

const HARNESSES: [Harness; 3] = [
    Harness {
        test: "qemu_linker",
        prefix: "qemu-linker",
        checks: &['p', 'r', 't', 'a', 's'],
        breaks: &["qe_site", "qe_nomap", "qe_id"],
    },
    Harness {
        test: "qemu_start",
        prefix: "qemu-start",
        checks: &['s', 'e', 't', 'h', 'g'],
        breaks: &["qe_guest_base", "qe_no_tls"],
    },
    Harness {
        test: "qemu_dlopen",
        prefix: "qemu-dlopen",
        checks: &['s', 'i', 'd', 'y', 'c', 'v'],
        breaks: &[SKIP_RELOC],
    },
];

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The arm64 NB out dir (`<out>/nb/arm64`): holds `native_bridge_guest_linker/linker64`, `libnative_bridge_vdso/libnative_bridge_vdso.so`, and the guest libraries `libnative_bridge_guest_libc`, `libdl`, `libm`, `ld-android`.
    #[arg(long)]
    pub nb: PathBuf,
    /// Only the plain runs, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-linker");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::link(&b, &dir.join("libqemu-spike.so"), None)?;
    let linker = args.nb.join("native_bridge_guest_linker/linker64");
    let vdso = args
        .nb
        .join("libnative_bridge_vdso/libnative_bridge_vdso.so");
    let stub_libc = args.nb.join("libnative_bridge_guest_libc/libc.so");
    ensure!(
        linker.is_file() && vdso.is_file() && stub_libc.is_file(),
        "missing {}, {} or {}: build the arm64 NB first (nb:libnative_bridge_guest_libc among its goals)",
        linker.display(),
        vdso.display(),
        stub_libc.display()
    );
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let fixture = guest_build(
        root,
        &target,
        "fixture",
        &dir.join("fixture"),
        &["-static"],
        &[],
    )?;
    let boot = guest_build(root, &target, "boot", &dir.join("boot"), &["-static"], &[])?;
    let exe = guest_build(
        root,
        &target,
        "exe",
        &dir.join("exe"),
        &["-nostdlib", "-fPIE", "-pie"],
        &[],
    )?;
    let dl_exe = guest_build(
        root,
        &target,
        "dl_exe",
        &dir.join("dl_exe"),
        &["-fPIE", "-pie"],
        &["-ldl"],
    )?;
    // the dlopen pair: the plain run loads libdep.so from `plain`; break skip_reloc loads a copy with its RELATIVE relocations removed from `skipped`
    let plain = dir.join("dlopen-libs");
    let skipped = dir.join("dlopen-libs-skip-reloc");
    for d in [&plain, &skipped] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let libdep = guest_build(
        root,
        &target,
        "libdep",
        &plain.join("libdep.so"),
        &["-shared", "-fPIC", "-nostdlib", "-Wl,-soname,libdep.so"],
        &[],
    )?;
    let link_dir = format!("-L{}", plain.display());
    let libfix = guest_build(
        root,
        &target,
        "libfix",
        &plain.join("libfix.so"),
        &[
            "-shared",
            "-fPIC",
            "-nostdlib",
            "-Wl,-soname,libfix.so",
            link_dir.as_str(),
        ],
        &["-ldep"],
    )?;
    skip_relative(&libdep, &skipped.join("libdep.so"))?;
    std::fs::copy(&libfix, skipped.join("libfix.so"))
        .with_context(|| format!("copying {}", libfix.display()))?;
    let build_dir = b.build_dir();
    let env: [(&str, &Path); 8] = [
        ("ATL_QEMU_BUILD", &build_dir),
        ("ATL_QEMU_SPIKE_SO", &so),
        ("ATL_QEMU_SPIKE_FIXTURE", &fixture),
        ("ATL_NB_BOOT", &boot),
        ("ATL_NB_EXE", &exe),
        ("ATL_NB_DLEXE", &dl_exe),
        ("ATL_NB_LINKER", &linker),
        ("ATL_NB_VDSO", &vdso),
    ];
    let mut fails = Vec::new();
    let mut total = 0;
    for h in &HARNESSES {
        let mut runs: Vec<Option<&str>> = vec![None];
        if !args.no_breaks {
            runs.extend(h.breaks.iter().map(|b| Some(*b)));
        }
        for brk in runs {
            total += 1;
            let name = format!("{}-{}", h.test, brk.unwrap_or("plain"));
            let dllibs = if brk == Some(SKIP_RELOC) {
                &skipped
            } else {
                &plain
            };
            if let Err(e) = harness(root, &dir, h, &env, dllibs, brk, &name) {
                fails.push(Failure::step(&name, format!("{e:#}")));
            }
        }
    }
    let verdict = Verdict {
        cmd: "qemu-linker",
        total: Some((total, "runs")),
        skipped: Vec::new(),
        failures: fails,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `<TESTDATA>/<stem>.c` built for the guest by the NDK clang: `flags` before the source, `libs` after it, output `bin`.
fn guest_build(
    root: &Path,
    target: &NdkTarget,
    stem: &str,
    bin: &Path,
    flags: &[&str],
    libs: &[&str],
) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join(format!("{stem}.c"));
    let status = target
        .cmd("clang")
        .args(flags)
        .args(["-O1", "-Wall", "-Werror", "-o"])
        .arg(bin)
        .arg(&src)
        .args(libs)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin.to_path_buf())
}

/// Elf64_Dyn (16 bytes: d_tag, d_val, little-endian LP64). DT_ANDROID_RELRSZ: the byte size of the packed (RELR) relocation table; the NDK's lld packs a shared library's RELATIVE relocations into it by default (`llvm-readelf -d`).
const DYN_BYTES: usize = 16;
const DT_ANDROID_RELRSZ: u64 = 0x6fff_e001;

/// `so` copied to `out` with DT_ANDROID_RELRSZ zeroed: the guest linker reads no packed relocation, so the RELATIVE class is skipped (break `skip_reloc`). Fails unless exactly one such entry exists, so a break that changed nothing cannot pass as a plain run.
fn skip_relative(so: &Path, out: &Path) -> Result<()> {
    let mut bytes = std::fs::read(so).with_context(|| format!("reading {}", so.display()))?;
    let dynamic = {
        let elf = object::read::elf::ElfFile64::<object::Endianness>::parse(&*bytes)
            .with_context(|| format!("parsing {}", so.display()))?;
        elf.section_by_name(".dynamic")
            .and_then(|s| s.file_range())
            .with_context(|| format!("{}: no .dynamic section", so.display()))?
    };
    let (dyn_at, dyn_len) = (dynamic.0 as usize, dynamic.1 as usize);
    let mut zeroed = 0;
    for at in (dyn_at..dyn_at + dyn_len).step_by(DYN_BYTES) {
        let slot = bytes
            .get_mut(at..at + DYN_BYTES)
            .context(".dynamic entry past the file")?;
        let d_tag = u64::from_le_bytes(<[u8; 8]>::try_from(&slot[..8])?);
        if d_tag == DT_ANDROID_RELRSZ {
            slot[8..].copy_from_slice(&0u64.to_le_bytes());
            zeroed += 1;
        }
    }
    ensure!(
        zeroed == 1,
        "{}: {zeroed} DT_ANDROID_RELRSZ entries, expected one (not a packed library?)",
        so.display()
    );
    std::fs::write(out, &bytes).with_context(|| format!("writing {}", out.display()))
}

/// One harness run: plain (every check ok, exit 0) or a break (not every check ok, and a nonzero exit). `dllibs` is the guest library directory of `qemu_dlopen` (ATL_NB_DLLIBS).
fn harness(
    root: &Path,
    dir: &Path,
    h: &Harness,
    env: &[(&str, &Path)],
    dllibs: &Path,
    brk: Option<&str>,
    name: &str,
) -> Result<()> {
    // `timeout -s KILL` kills the run's whole process group and exits 137 (not 124): a hung run is a failed run either way
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args([
        "test",
        "--locked",
        "-p",
        PACKAGE,
        "--features",
        "qemu-guest",
        "--test",
        h.test,
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--"])
    .envs(env.iter().copied())
    .env("ATL_NB_DLLIBS", dllibs)
    .current_dir(root);
    match brk {
        // a harness-level break changes the fixture files, not the product build
        Some(b) if b != SKIP_RELOC => cmd.env("ATL_NB_BREAK", b),
        _ => cmd.env_remove("ATL_NB_BREAK"),
    };
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    let text = &run.stdout;
    let ok = |c: char| text.lines().any(|l| l == format!("{}: ({c}) ok", h.prefix));
    match brk {
        None => {
            let red: Vec<char> = h.checks.iter().copied().filter(|&c| !ok(c)).collect();
            ensure!(
                red.is_empty() && run.status.success(),
                "checks {red:?} not ok ({}), log {}",
                run.status,
                log.display()
            );
        }
        Some(_) => {
            ensure!(
                !h.checks.iter().all(|&c| ok(c)),
                "break left every check ok, log {}",
                log.display()
            );
            ensure!(
                !run.status.success(),
                "break run exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}
