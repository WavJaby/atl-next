// SPDX-License-Identifier: MIT
//! `xtask test qemu-thread`: guest threads under the QEMU crossing on the real arm64 NB images (QEMU bridge plan Q5-6). A guest `pthread_create` attaches its own engine CPU, the provider's foreach reaches a guest callback, a guest key destructor runs at thread exit through a crossing. Then each break must turn its run red.
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`), as `qemu-linker` uses it. Fixture: `xtask/testdata/qemu-thread/fixture.c` (PIE, NDK clang, DT_NEEDED libc.so). Boot: the engine boot executable (`qemu::boot_exe`, crates/atl-qemu-boot).
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`), built as for `test qemu-linker`.
//! Harness: `tests/qemu_thread.rs` (harness-less; checks s, p, j, d) run through `qemu_linker::harness`. Breaks (`ATL_NB_BREAK`): `qt_skip_dtor`, `qt_routine_arg`.
//! Property area (check p): the guest cannot set properties (the provider's set is ENOENT), so the gate writes `SEEDED` `atl.qt.p.<i>` properties and `atl.qt.expect` into a copy of the native out dir's build.prop, and the area writer (atl-property-service, the broker's own) lays out the area the provider maps through `ATL_PROP_AREA`.
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-thread/`, one log per run; the plain run's log carries the call-cost line.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::host_art;
use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker::{self, Harness};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/qemu-thread";
/// Properties the gate seeds; the foreach must count exactly these (`SEED_EXPECT` carries the same number).
const SEEDED: usize = 4;
/// fixture.c `qt_prefix`: the seeded properties' names are this and an index.
const SEED_PREFIX: &str = "atl.qt.p.";
/// fixture.c `qt_props_expected` reads this property.
const SEED_EXPECT: &str = "atl.qt.expect";
const HARNESS: Harness = Harness {
    test: "qemu_thread",
    prefix: "qemu-thread",
    checks: &['s', 'p', 'j', 'd'],
    breaks: &["qt_skip_dtor", "qt_routine_arg"],
};

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The arm64 NB out dir (`<out>/nb/arm64`), as for `test qemu-linker`.
    #[arg(long)]
    pub nb: PathBuf,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-thread");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::engine_so_file(root, &b, &dir.join("engine"))?;
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
    let fixture = fixture_build(root, &target, &dir.join("fixture"))?;
    let boot = qemu::boot_exe(root, &b, &dir)?;
    let out = args
        .nb
        .ancestors()
        .nth(2)
        .context("the NB directory is not <out>/nb/<arch>")?;
    let area = seed_area(root, out, &dir)?;
    let env: [(&str, &Path); 6] = [
        ("ATL_ENGINE_SO", &so),
        ("ATL_NB_BOOT", &boot),
        ("ATL_NB_THREADEXE", &fixture),
        ("ATL_NB_LINKER", &linker),
        ("ATL_NB_VDSO", &vdso),
        ("ATL_PROP_AREA", &area),
    ];
    let mut runs: Vec<Option<&str>> = vec![None];
    if !args.no_breaks {
        runs.extend(HARNESS.breaks.iter().map(|b| Some(*b)));
    }
    let total = runs.len();
    let mut fails = Vec::new();
    for brk in runs {
        let name = format!("{}-{}", HARNESS.prefix, brk.unwrap_or("plain"));
        if let Err(e) = qemu_linker::harness(root, &dir, &HARNESS, &env, &dir, brk, &name) {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-thread",
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

/// The property area the harness maps (`ATL_PROP_AREA`): the native out dir's default build.prop plus the seeded properties, laid out by the area writer into `dir`.
fn seed_area(root: &Path, out: &Path, dir: &Path) -> Result<PathBuf> {
    let base = out.join(host_art::BUILD_PROP);
    let mut text =
        std::fs::read_to_string(&base).with_context(|| format!("reading {}", base.display()))?;
    for i in 0..SEEDED {
        text.push_str(&format!("{SEED_PREFIX}{i}=qt\n"));
    }
    text.push_str(&format!("{SEED_EXPECT}={SEEDED}\n"));
    let build_prop = dir.join("build.prop");
    std::fs::write(&build_prop, text)
        .with_context(|| format!("writing {}", build_prop.display()))?;
    let service = host_art::build_property_service(root, &out.join("cargo"))?;
    let area = dir.join("properties");
    let status = Command::new(&service)
        .arg("--build-prop")
        .arg(&build_prop)
        .arg("--out")
        .arg(&area)
        .status()
        .context("running atl-property-service")?;
    ensure!(status.success(), "atl-property-service failed ({status})");
    Ok(area)
}

/// `fixture.c` for the guest: a PIE executable on the NDK's libc.so (no `-nostdlib`, so DT_NEEDED names libc.so, which the guest linker resolves to the stub libc).
fn fixture_build(root: &Path, target: &NdkTarget, bin: &Path) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join("fixture.c");
    let status = target
        .cmd("clang")
        .args(["-fPIE", "-pie", "-O1", "-Wall", "-Werror", "-o"])
        .arg(bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin.to_path_buf())
}
