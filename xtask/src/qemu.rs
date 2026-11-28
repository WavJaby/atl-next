// SPDX-License-Identifier: MIT
//! `xtask qemu`: the pinned QEMU's user-mode executable for one guest arch, built with no patch (QEMU bridge plan Q1).
//!
//! Source: the QEMU checkout (`patch::preflight`: the sources.toml `[qemu]` pin + patches/qemu, clean; a task copy when one exists).
//! Build: QEMU's own `configure` (meson underneath) into `<out>/qemu/<target>-linux-user/build`, then ninja for `qemu-<target>` alone.
//! `--pic`: the same build with `-fPIC` into `<target>-linux-user-pic`, the objects the spike library links (qemu-spike).
//! Compilers: the `clang` deps row as host compiler (the converter's toolchain, as BoringSSL's recipe); the NDK clang wrapper of the arch's triple as guest compiler for `tests/tcg` (qemu_gate.rs). One build dir serves both gates.
//! Traps: configure's default download fetches PyPI wheels into `src/qemu/python/wheels` and meson wraps into `src/qemu/subprojects/`, both git-ignored, so the checkout stays clean. `--disable-download` is refused unless `subprojects/keycodemapdb` exists, which the pin does not check out.
//! A changed configuration wipes the build dir (stamp `configure.args`); an interrupted configure leaves no stamp, so the next run starts over.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::arches;
use crate::build_pool;
use crate::deps;
use crate::git::git;
use crate::isa_gate;
use crate::ndk::{self, NdkTarget};
use crate::patch;
use crate::sources;
use crate::verdict::Verdict;

/// The sources.toml row of the QEMU checkout, `src/qemu`.
pub const UPSTREAM: &str = "qemu";
/// The arch built without `--arch`: the first bridge arch (maintainer decision D3, QEMU bridge plan section 3).
pub const DEFAULT_ARCH: &str = "arm64";
/// API level of the guest compiler: 29 is the first with ELF TLS, the shape of a modern app's `.so` (jni_fixture.rs).
pub const GUEST_API: u32 = 29;
/// Out dir under the build out dir: `<out>/qemu/<target>-linux-user/`.
const DIR: &str = "qemu";
/// Written once configure succeeds: the arguments it ran with. Any other text wipes the build dir.
const STAMP: &str = "configure.args";

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is built.
    #[arg(long, default_value = DEFAULT_ARCH)]
    pub arch: String,
    /// arches.toml host row the executable runs on (default: the build machine). Another arch is a cross build (`<target>-linux-user-<host>`) gated by the host's isa, glibc_floor and sysroot instead of a `--version` run.
    #[arg(long)]
    pub host: Option<String>,
    /// Out dir (default: this checkout's build dir, see `xtask out`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The `-fPIC` variant (`<target>-linux-user-pic`) the spike library links, not the executable the gates run.
    #[arg(long)]
    pub pic: bool,
    /// Configure and build. Without it: print the plan and build nothing.
    #[arg(long)]
    pub execute: bool,
}

/// One arch's build: where it sits, the pinned source and the compilers.
pub struct Build {
    /// The arches.toml row (`arm64`).
    pub arch: String,
    /// The `qemu_target` column (`aarch64`); QEMU's target is `<target>-linux-user`.
    pub target: String,
    /// Whether this is the `-fPIC` variant (`--pic`).
    pub pic: bool,
    /// `<host out>/qemu/<target>-linux-user` (native.rs host_out of the executable's host), or `-pic` for the PIC variant.
    pub dir: PathBuf,
    /// The pinned checkout.
    pub src: PathBuf,
    /// The pin's version: its tag without the `v` (`v11.1.3` -> `11.1.3`).
    pub version: String,
    /// The NDK clang wrapper of the arch's triple at `GUEST_API`.
    pub guest_cc: PathBuf,
    /// The host compiler binary (deps row `clang`).
    pub cc: String,
    /// The ninja binary (deps row `ninja`).
    pub ninja: String,
    /// The host of another arch than the build machine, None for a native build.
    pub cross: Option<Cross>,
}

/// A cross host for QEMU's own configure: `--cross-prefix` makes it write a meson cross file (`[host_machine]` from `--cpu`, which QEMU maps to meson's cpu_family); compiler, binutils and pkg-config come from the arches.toml host row and deps rows, never from `<prefix>gcc` tools the build machine lacks.
pub struct Cross {
    /// The arches.toml host row (`riscv64`).
    pub host: String,
    /// Its meson_cpu_family, QEMU's `--cpu`.
    pub cpu: String,
    /// Its Rust target (`rust` column): what Rust code linked into the build's objects compiles for.
    pub rust: String,
    /// Its clang triple: `--target` and the `--cross-prefix` QEMU derives tool names from.
    pub clang: String,
    /// The unpacked sysroot (deps `sysroot_debs` row).
    pub sysroot: PathBuf,
    /// Its isa, passed as `-march` (clang takes the arches.toml spelling as is).
    pub isa: String,
    /// The cross compiler binary (row `cc`).
    pub cc: String,
    /// `AR`, `NM`, ... for configure: the llvm row's tools, which read every target's objects.
    pub tools: Vec<(String, String)>,
}

impl Cross {
    /// configure's environment: the binutils and pkg-config restricted to the sysroot (no build-machine .pc file can leak in).
    fn env(&self) -> Vec<(String, String)> {
        let mut env = self.tools.clone();
        env.push(("PKG_CONFIG".to_owned(), "pkg-config".to_owned()));
        env.extend(
            crate::cross::pkg_config_env(&self.sysroot, &self.clang)
                .map(|(k, v)| (k.to_owned(), v)),
        );
        env
    }
}

impl Build {
    /// QEMU's target name: `aarch64-linux-user`.
    pub fn name(&self) -> String {
        format!("{}-linux-user", self.target)
    }

    /// The executable's name: `qemu-aarch64`.
    pub fn exe_name(&self) -> String {
        format!("qemu-{}", self.target)
    }

    /// QEMU's meson build dir.
    pub fn build_dir(&self) -> PathBuf {
        self.dir.join("build")
    }

    /// The built executable.
    pub fn exe(&self) -> PathBuf {
        self.build_dir().join(self.exe_name())
    }

    /// QEMU configure arguments: one linux-user target, no system emulation, no docs, no tools (meson option `tools`).
    /// `--cross-cc-<target>` is what `tests/tcg` builds its guest programs with. Its `-static` flag: the programs run with no dynamic loader.
    pub fn configure_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--target-list={}", self.name()),
            "--disable-system".to_owned(),
            "--enable-linux-user".to_owned(),
            "--disable-docs".to_owned(),
            "-Dtools=disabled".to_owned(),
            format!("--cross-cc-{}={}", self.target, self.guest_cc.display()),
            format!("--cross-cc-cflags-{}=-static", self.target),
        ];
        match &self.cross {
            None => args.push(format!("--cc={}", self.cc)),
            Some(c) => {
                // configure splits --cc on spaces into the cross file's `c = [...]`; the build machine's compiler builds QEMU's own build-time tools
                args.push(format!("--cross-prefix={}-", c.clang));
                args.push(format!("--cpu={}", c.cpu));
                args.push(format!(
                    "--cc={} --target={} --sysroot={} -march={}",
                    c.cc,
                    c.clang,
                    c.sysroot.display(),
                    c.isa
                ));
                args.push(format!("--host-cc={}", self.cc));
                args.push("--extra-ldflags=-fuse-ld=lld".to_owned());
            }
        }
        if self.pic {
            // meson's b_pie=true appends -fPIE after --extra-cflags, and -fPIE wins for clang (TPOFF32 on current_cpu in a shared object).
            // --disable-pie drops -fPIE (meson.build: -fno-pie instead); the explicit -fPIC then sets the codegen.
            args.push("--disable-pie".to_owned());
            args.push("--extra-cflags=-fPIC".to_owned());
        }
        args
    }
}

/// The build of `arch` into `out` (default: this checkout's build dir). Reads only: the pin, the NDK and the deps rows, then the out dir (which takes the dir lock, as every build does).
pub fn plan(root: &Path, arch: &str, out: Option<&Path>, pic: bool) -> Result<Build> {
    plan_on(root, arch, None, out, pic)
}

/// [`plan`] for the executable running on arches.toml host `host` (None or the build machine: native).
pub fn plan_on(
    root: &Path,
    arch: &str,
    host: Option<&str>,
    out: Option<&Path>,
    pic: bool,
) -> Result<Build> {
    let arches = arches::load(root)?;
    let cross = match host {
        None => None,
        Some(host) => cross_host(root, &arches, host)?,
    };
    let target = arches::qemu_target(&arches, arch)?.to_owned();
    let (src, version) = pinned(root)?;
    let ndk = ndk::default_ndk(root)?;
    let guest_cc = NdkTarget::new(root, &ndk, arch, GUEST_API)?
        .wrapper()
        .to_owned();
    ensure!(
        guest_cc.is_file(),
        "{}: no NDK clang (NDK at {})",
        guest_cc.display(),
        ndk.display()
    );
    // the executable is a product of its host: under that host's out dir (native.rs host_out), as the host's other outputs
    let host_name = match &cross {
        Some(c) => c.host.clone(),
        None => arches::build_machine(root, &arches)?.0.to_owned(),
    };
    let out = crate::native::host_out(root, out, &host_name)?;
    let pic_suffix = if pic { "-pic" } else { "" };
    Ok(Build {
        arch: arch.to_owned(),
        dir: out
            .join(DIR)
            .join(format!("{target}-linux-user{pic_suffix}")),
        target,
        pic,
        src,
        version,
        guest_cc,
        cc: deps::binary(root, "clang")?,
        ninja: deps::binary(root, "ninja")?,
        cross,
    })
}

/// The cross host `host` (arches::cross_target), None when it is the build machine. Its rows must be satisfied (deps::host_rows_problems names the fetch command).
fn cross_host(root: &Path, arches: &arches::Arches, host: &str) -> Result<Option<Cross>> {
    let (build, _) = arches::build_machine(root, arches)?;
    let row = &arches::host(arches, host)?.host;
    let Some(t) = arches::cross_target(host, row, build)? else {
        return Ok(None);
    };
    let problems = deps::host_rows_problems(root, host)?;
    ensure!(
        problems.is_empty(),
        "cross host {host}: {}",
        problems.join("; ")
    );
    // the llvm row names llvm-ar-<major>; its siblings share the suffix
    let ar = deps::binary(root, "llvm")?;
    let tool = |name: &str| ar.replacen("llvm-ar", &format!("llvm-{name}"), 1);
    let tools = [
        ("AR", "ar"),
        ("NM", "nm"),
        ("RANLIB", "ranlib"),
        ("STRIP", "strip"),
        ("OBJCOPY", "objcopy"),
    ]
    .into_iter()
    .map(|(var, name)| (var.to_owned(), tool(name)))
    .collect();
    Ok(Some(Cross {
        host: host.to_owned(),
        cpu: row.meson_cpu_family.clone(),
        rust: row.rust.clone(),
        sysroot: deps::sysroot_root(root, &t.sysroot)?,
        clang: t.clang,
        isa: t.isa,
        cc: deps::binary(root, &t.cc)?,
        tools,
    }))
}

/// The checkout as `patch::preflight` allows it (pin + patches/qemu, clean), and the pin's version.
fn pinned(root: &Path) -> Result<(PathBuf, String)> {
    let sources = sources::load(root)?;
    let pin = sources
        .git
        .get(UPSTREAM)
        .with_context(|| format!("{}: no [{UPSTREAM}] row", sources::FILE))?;
    let tag = pin
        .tag
        .as_deref()
        .with_context(|| format!("{}: [{UPSTREAM}] pins a branch, not a tag", sources::FILE))?;
    let version = tag
        .strip_prefix('v')
        .with_context(|| format!("tag {tag} has no `v` prefix"))?;
    let src = Layout::of(root)?.locate(Path::new(UPSTREAM)).dir;
    git(Some(&src), &["rev-parse", "HEAD^{commit}"]).with_context(|| {
        format!(
            "{}: not fetched (`cargo xtask fetch --execute`)",
            src.display()
        )
    })?;
    patch::preflight(root, [UPSTREAM])?;
    Ok((src, version.to_owned()))
}

/// The stamp text of this configuration. It names the source dir too: build.ninja holds the source paths, so a task copy's build dir must not be reused from `src/qemu`.
fn stamp_text(b: &Build) -> String {
    format!(
        "{} source={}",
        b.configure_args().join(" "),
        b.src.display()
    )
}

/// Whether `build/` already holds this configuration (configure would do nothing).
fn configured(b: &Build) -> bool {
    std::fs::read_to_string(b.dir.join(STAMP)).ok().as_deref() == Some(stamp_text(b).as_str())
        && b.build_dir().join("build.ninja").is_file()
}

/// What ninja would do for a target, read from a dry run without a grant.
#[derive(Debug, PartialEq, Eq)]
enum Work {
    /// "no work to do".
    None,
    /// Every dirty root is an always-stale edge (meson `build_always_stale`: QEMU's `qemu-version.h` hangs off `PHONY`, a phony edge with no inputs). The real run regenerates it and restat stops the rebuild there; a dry run cannot simulate restat, so it lists the dependents too.
    AlwaysStale,
    /// Any other dirty root, a failing dry run, or an explain line not recognised: real compile work.
    Real,
}

/// The explain line ninja prints for an always-stale root (ninja `src/graph.cc`, `RecomputeOutputDirty`).
const ALWAYS_STALE: &str = "of phony edge with no inputs doesn't exist";

/// Classifies `target` in `dir` by `ninja -n -d explain` (stats only). Only root causes count: `<x> is dirty` lines are propagation, every other explain line names a root. A doubtful case is `Real`, so it takes the build grant.
/// Bound: ninja stops explaining an edge at its first dirty input, so a changed source of an edge downstream of the always-stale one is hidden and that edge runs under `light` at `-j 1` (QEMU: `linux-user/main.c` + the link; 1 job = the 1 slot light holds). Every edge off the version header is seen.
fn ninja_work(ninja: &str, dir: &Path, target: &str) -> Result<Work> {
    let out = Command::new(ninja)
        .args(["-n", "-d", "explain", "-C"])
        .arg(dir)
        .arg(target)
        .output()
        .context("running ninja -n -d explain")?;
    if !out.status.success() {
        return Ok(Work::Real);
    }
    if String::from_utf8_lossy(&out.stdout).contains("ninja: no work to do.") {
        return Ok(Work::None);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut roots = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("ninja explain: "))
        .filter(|l| !l.ends_with(" is dirty"))
        .peekable();
    if roots.peek().is_none() {
        return Ok(Work::Real);
    }
    Ok(if roots.all(|l| l.contains(ALWAYS_STALE)) {
        Work::AlwaysStale
    } else {
        Work::Real
    })
}

/// Configures `build/` unless it already holds this configuration. A different or interrupted configuration starts from nothing.
fn configure(b: &Build) -> Result<()> {
    if configured(b) {
        return Ok(());
    }
    let args = b.configure_args();
    let text = stamp_text(b);
    let stamp = b.dir.join(STAMP);
    let build = b.build_dir();
    remove_if_present(&stamp)?;
    if build.exists() {
        std::fs::remove_dir_all(&build).with_context(|| format!("removing {}", build.display()))?;
    }
    std::fs::create_dir_all(&build)?;
    let log = b.dir.join("configure.log");
    let status = Command::new(b.src.join("configure"))
        .args(&args)
        .envs(b.cross.iter().flat_map(Cross::env))
        .current_dir(&build)
        .stdout(File::create(&log)?)
        .stderr(File::options().append(true).open(&log)?)
        .status()
        .context("running QEMU's configure")?;
    ensure!(
        status.success(),
        "QEMU configure failed, log {}",
        log.display()
    );
    std::fs::write(&stamp, text)?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Builds `qemu-<target>` (incremental) with ninja, `jobs` wide (the grant's CPU slots).
fn ninja(b: &Build, jobs: u32) -> Result<()> {
    let log = b.dir.join("ninja.log");
    let status = Command::new(&b.ninja)
        .arg("-j")
        .arg(jobs.to_string())
        .arg("-C")
        .arg(b.build_dir())
        .arg(b.exe_name())
        .stdout(File::create(&log)?)
        .stderr(File::options().append(true).open(&log)?)
        .status()
        .context("running ninja")?;
    ensure!(status.success(), "ninja failed, log {}", log.display());
    Ok(())
}

/// `qemu-<target> --version` prints `qemu-<target> version <pin version> (...)`.
fn check_version(b: &Build) -> Result<()> {
    let exe = b.exe();
    let out = Command::new(&exe)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", exe.display()))?;
    ensure!(
        out.status.success(),
        "{} --version exited {}",
        exe.display(),
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let name = b.exe_name();
    let mut words = text.split_whitespace();
    let got = (words.next(), words.next(), words.next());
    ensure!(
        got == (
            Some(name.as_str()),
            Some("version"),
            Some(b.version.as_str())
        ),
        "{} --version says {:?}, the pin is {}",
        exe.display(),
        text.lines().next().unwrap_or_default(),
        b.version
    );
    Ok(())
}

/// Configures and builds under one grant when there is work, then checks the version. Returns the executable.
/// Freshness is checked first without a grant (`ninja_work`), so an up-to-date engine (every run-apk launch of a built tree) never queues behind other checkouts' heavy builds: nothing to do takes no class; only always-stale edges (the version header, restat stops there) take `light`, which bypasses the queue for one free slot; real work in a configured graph is warm (native-incr), a (re)configure cold (native-full), as native.rs picks its class.
pub fn execute(root: &Path, b: &Build) -> Result<PathBuf> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let class = if !configured(b) {
        Some(build_pool::Class::NativeFull)
    } else {
        match ninja_work(&b.ninja, &b.build_dir(), &b.exe_name())? {
            Work::None => None,
            Work::AlwaysStale => Some(build_pool::Class::Light),
            Work::Real => Some(build_pool::Class::NativeIncr),
        }
    };
    match class {
        None => println!("qemu: {} up to date: no pool grant", b.exe_name()),
        Some(class) => {
            let grant = build_pool::acquire(&ws, root, class)?;
            println!(
                "qemu: {} pool {} cpu {}",
                b.exe_name(),
                class.name(),
                grant.cpu
            );
            configure(b)?;
            ninja(b, grant.cpu)?;
        }
    }
    match &b.cross {
        None => check_version(b)?,
        Some(c) => isa_gate_cross(root, c, &b.exe(), "qemu")?,
    }
    Ok(b.exe())
}

/// A cross output cannot run here: the host's isa gate holds `path` to the row's isa closure, glibc_floor and sysroot libraries; summary lines print under `tag`.
pub fn isa_gate_cross(root: &Path, c: &Cross, path: &Path, tag: &str) -> Result<()> {
    let arches = arches::load(root)?;
    let row = arches::host(&arches, &c.host)?;
    let host = &row.host;
    let report = isa_gate::check_executable(path, host, &isa_gate::sysroot_dirs(root, host)?)?;
    for line in isa_gate::summary(&report) {
        println!("{tag}: isa gate: {line}");
    }
    ensure!(
        report.violations.is_empty(),
        "{tag}: isa gate: {}",
        isa_gate::first_violation(&report).unwrap_or_default()
    );
    Ok(())
}

/// The engine's guest executables' crate root (atl-qemu-boot): `no_std`, each entry in `global_asm!`, chosen by cfg.
const BOOT_CRATE: &str = "crates/atl-qemu-boot/src/lib.rs";
/// The boot entry's file name, as atl-android-paths `engine_boot` installs it.
const BOOT_NAME: &str = "atl_engine_boot";
/// The cpuinfo entry's file name, as atl-android-paths `engine_cpuinfo` installs it.
const CPUINFO_NAME: &str = "atl_engine_cpuinfo";

/// The boot executable's deliberate break: the cfg that drops its BOOT svc (atl-qemu-boot lib.rs; its build.rs `CFGS` declares the same cfg).
pub const SKIP_BOOT: &str = "atl_qb_skip_boot";
/// The cfg that selects atl-qemu-boot's cpuinfo entry (its build.rs `CFGS` declares it).
const CPUINFO: &str = "atl_qb_cpuinfo";

/// The engine boot executable for `b`'s guest arch, `<out>/atl_engine_boot`: atl-qemu-boot compiled by rust-toolchain.toml's rustc for the arches.toml guest `rust` triple as a static non-PIE executable, linked by rust-lld (no NDK, no libc: the entry is the crate's `_start`).
pub fn boot_exe(root: &Path, b: &Build, out: &Path) -> Result<PathBuf> {
    guest_exe(root, b, out, BOOT_NAME, None)
}

/// [`boot_exe`] with `brk`, a break cfg ([`SKIP_BOOT`]) passed as `--cfg`.
pub fn boot_exe_break(root: &Path, b: &Build, out: &Path, brk: Option<&str>) -> Result<PathBuf> {
    guest_exe(root, b, out, BOOT_NAME, brk)
}

/// The engine cpuinfo probe for `b`'s guest arch, `<out>/atl_engine_cpuinfo`: atl-qemu-boot's cpuinfo entry, built as [`boot_exe`].
pub fn cpuinfo_exe(root: &Path, b: &Build, out: &Path) -> Result<PathBuf> {
    guest_exe(root, b, out, CPUINFO_NAME, Some(CPUINFO))
}

/// atl-qemu-boot as `<out>/<name>`, with `cfg` passed as `--cfg`.
fn guest_exe(root: &Path, b: &Build, out: &Path, name: &str, cfg: Option<&str>) -> Result<PathBuf> {
    let arches = arches::load(root)?;
    let triple = arches::guest_rust(&arches, &b.arch)?;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let bin = out.join(name);
    let mut cmd = Command::new("rustc");
    if let Some(cfg) = cfg {
        cmd.args(["--cfg", cfg]);
    }
    let status = cmd
        .current_dir(root)
        .args(["--edition", "2024", "--target", triple])
        .args(["--crate-type", "bin", "--crate-name", name])
        .args(["-C", "panic=abort", "-C", "opt-level=1"])
        .args(["-C", "linker=rust-lld", "-C", "linker-flavor=ld.lld"])
        .args([
            "-C",
            "relocation-model=static",
            "-C",
            "target-feature=+crt-static",
        ])
        .arg("-o")
        .arg(&bin)
        .arg(root.join(BOOT_CRATE))
        .status()
        .context("running rustc")?;
    ensure!(status.success(), "rustc failed on {BOOT_CRATE} ({triple})");
    Ok(bin)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = plan_on(
        root,
        &args.arch,
        args.host.as_deref(),
        args.out.as_deref(),
        args.pic,
    )?;
    if !args.execute {
        println!(
            "qemu: plan for {} (dry run; `--execute` configures and builds)",
            b.arch
        );
        println!(
            "qemu: source    {} (sources.toml [{UPSTREAM}] pin, v{})",
            b.src.display(),
            b.version
        );
        println!("qemu: build dir {}", b.build_dir().display());
        println!("qemu: configure {}", b.configure_args().join(" "));
        println!("qemu: output    {}", b.exe().display());
        return Ok(ExitCode::SUCCESS);
    }
    let exe = execute(root, &b)?;
    println!(
        "qemu: {} built, {} version {}",
        b.name(),
        exe.display(),
        b.version
    );
    println!(
        "{}",
        Verdict {
            cmd: "qemu",
            total: None,
            skipped: Vec::new(),
            failures: Vec::new()
        }
        .line()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grant-free probe on two graphs: plain (`out` from `in`) and QEMU's shape (`ver` always-stale off `PHONY`, restat, `bin` depends on it). Not built and a newer input are real work; built is none for the plain graph and always-stale for QEMU's (a dry run cannot restat).
    #[test]
    fn ninja_dry_run_classifies_work() {
        let tmp = std::env::temp_dir().join(format!("xtask-qemu-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("in"), "a").unwrap();
        std::fs::write(
            tmp.join("build.ninja"),
            concat!(
                "rule cp\n  command = cp $in $out\n",
                "rule gen\n  command = echo v > $out.t && (cmp -s $out.t $out || mv $out.t $out)\n  restat = 1\n",
                "build out: cp in\n",
                "build PHONY: phony\n",
                "build ver: gen PHONY\n",
                "build bin: cp in | ver\n",
                "build all: phony out bin\n",
            ),
        )
        .unwrap();
        let work = |t: &str| ninja_work("ninja", &tmp, t).unwrap();
        let build = || {
            let o = Command::new("ninja")
                .arg("-C")
                .arg(&tmp)
                .args(["out", "bin"])
                .output()
                .unwrap();
            assert!(o.status.success());
        };
        assert_eq!(work("out"), Work::Real, "never built");
        assert_eq!(work("all"), Work::Real, "never built");
        build();
        assert_eq!(work("out"), Work::None);
        assert_eq!(work("bin"), Work::AlwaysStale);
        // a newer input: ninja compares mtimes, so step past the filesystem's resolution
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        File::options()
            .write(true)
            .open(tmp.join("in"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(work("out"), Work::Real, "input changed");
        // the known bound: ninja stops explaining an edge at its first dirty input, so a root behind the always-stale one is hidden (`ninja_work`)
        assert_eq!(work("bin"), Work::AlwaysStale, "hidden behind ver");
        assert_eq!(work("all"), Work::Real, "seen on an edge off ver");
        // the real run of the always-stale case: restat stops at `ver`, so `bin` is not rebuilt (`in` back in the past first: a future mtime keeps every output older)
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(tmp.join("in"))
            .unwrap()
            .set_modified(earlier)
            .unwrap();
        build();
        let mtime = || {
            std::fs::metadata(tmp.join("bin"))
                .unwrap()
                .modified()
                .unwrap()
        };
        let before = mtime();
        build();
        assert_eq!(mtime(), before, "restat kept bin");
        assert_eq!(work("bin"), Work::AlwaysStale);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// The boot crate's build.rs declares every cfg xtask passes, so its host lint build knows them.
    #[test]
    fn boot_cfgs_are_declared_by_the_boot_crate() {
        let build_rs = include_str!("../../crates/atl-qemu-boot/build.rs");
        for cfg in [SKIP_BOOT, CPUINFO] {
            assert!(build_rs.contains(&format!("\"{cfg}\"")), "{cfg}");
        }
    }

    fn build(target: &str) -> Build {
        Build {
            arch: "arm64".to_owned(),
            target: target.to_owned(),
            pic: false,
            dir: PathBuf::from("/out/qemu/aarch64-linux-user"),
            src: PathBuf::from("/src/qemu"),
            version: "11.1.3".to_owned(),
            guest_cc: PathBuf::from("/ndk/aarch64-linux-android29-clang"),
            cc: "clang-19".to_owned(),
            ninja: "ninja".to_owned(),
            cross: None,
        }
    }

    #[test]
    fn names_follow_the_qemu_target() {
        let b = build("aarch64");
        assert_eq!(b.name(), "aarch64-linux-user");
        assert_eq!(b.exe_name(), "qemu-aarch64");
        assert_eq!(
            b.exe(),
            PathBuf::from("/out/qemu/aarch64-linux-user/build/qemu-aarch64")
        );
    }

    #[test]
    fn configure_builds_one_linux_user_target_with_both_compilers() {
        let args = build("aarch64").configure_args();
        assert_eq!(args[0], "--target-list=aarch64-linux-user");
        assert!(args.contains(&"--disable-system".to_owned()));
        assert!(args.contains(&"--cc=clang-19".to_owned()));
        assert!(args.contains(&"--cross-cc-aarch64=/ndk/aarch64-linux-android29-clang".to_owned()));
        assert!(args.contains(&"--cross-cc-cflags-aarch64=-static".to_owned()));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("-fPIC") || a == "--disable-pie")
        );
    }

    #[test]
    fn pic_variant_drops_fpie_and_adds_fpic() {
        let mut b = build("aarch64");
        b.pic = true;
        let args = b.configure_args();
        assert!(args.contains(&"--disable-pie".to_owned()));
        assert!(args.contains(&"--extra-cflags=-fPIC".to_owned()));
    }

    /// A cross host: configure only writes a meson cross file with --cross-prefix; the cross clang is one --cc word list, the build machine's clang builds the build-time tools, and pkg-config sees only the sysroot.
    #[test]
    fn cross_host_configures_a_meson_cross_build() {
        let mut b = build("aarch64");
        b.cross = Some(Cross {
            host: "riscv64".to_owned(),
            cpu: "riscv64".to_owned(),
            rust: "riscv64gc-unknown-linux-gnu".to_owned(),
            clang: "riscv64-linux-gnu".to_owned(),
            sysroot: PathBuf::from("/s"),
            isa: "rv64gc".to_owned(),
            cc: "clang-21".to_owned(),
            tools: vec![("AR".to_owned(), "llvm-ar-19".to_owned())],
        });
        let args = b.configure_args();
        for want in [
            "--cross-prefix=riscv64-linux-gnu-",
            "--cpu=riscv64",
            "--cc=clang-21 --target=riscv64-linux-gnu --sysroot=/s -march=rv64gc",
            "--host-cc=clang-19",
            "--extra-ldflags=-fuse-ld=lld",
        ] {
            assert!(args.contains(&want.to_owned()), "{want}: {args:?}");
        }
        assert!(!args.contains(&"--cc=clang-19".to_owned()));
        let env = b.cross.as_ref().unwrap().env();
        assert!(env.contains(&("AR".to_owned(), "llvm-ar-19".to_owned())));
        assert!(env.contains(&(
            "PKG_CONFIG_LIBDIR".to_owned(),
            "/s/usr/lib/riscv64-linux-gnu/pkgconfig:/s/usr/share/pkgconfig".to_owned()
        )));
    }
}
