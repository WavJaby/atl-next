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
use crate::jni_fixture::{self, NdkTarget};
use crate::patch;
use crate::slot;
use crate::sources;
use crate::verdict::Verdict;

/// The sources.toml row of the QEMU checkout, `src/qemu`.
pub const UPSTREAM: &str = "qemu";
/// The arch built without `--arch`: the first bridge arch (owner decision D3, QEMU bridge plan section 3).
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
    /// Out dir (default: this checkout's slot, see `xtask slot`; else <workspace>/out).
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
    /// `<out>/qemu/<target>-linux-user`, or `-pic` for the PIC variant.
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
            format!("--cc={}", self.cc),
            format!("--cross-cc-{}={}", self.target, self.guest_cc.display()),
            format!("--cross-cc-cflags-{}=-static", self.target),
        ];
        if self.pic {
            // meson's b_pie=true appends -fPIE after --extra-cflags, and -fPIE wins for clang (TPOFF32 on current_cpu in a shared object).
            // --disable-pie drops -fPIE (meson.build: -fno-pie instead); the explicit -fPIC then sets the codegen.
            args.push("--disable-pie".to_owned());
            args.push("--extra-cflags=-fPIC".to_owned());
        }
        args
    }
}

/// The build of `arch` into `out` (default: this checkout's slot). Reads only: the pin, the NDK and the deps rows, then the out dir (which takes the slot lock, as every build does).
pub fn plan(root: &Path, arch: &str, out: Option<&Path>, pic: bool) -> Result<Build> {
    let arches = arches::load(root)?;
    let target = arches::qemu_target(&arches, arch)?.to_owned();
    let (src, version) = pinned(root)?;
    let ndk = jni_fixture::default_ndk(root)?;
    let guest_cc = NdkTarget::new(root, &ndk, arch, GUEST_API)?
        .wrapper()
        .to_owned();
    ensure!(
        guest_cc.is_file(),
        "{}: no NDK clang (NDK at {})",
        guest_cc.display(),
        ndk.display()
    );
    let out = slot::out_dir(root, out, "--out", |ws| ws.join("out"))?;
    let suffix = if pic { "-pic" } else { "" };
    Ok(Build {
        arch: arch.to_owned(),
        dir: out.join(DIR).join(format!("{target}-linux-user{suffix}")),
        target,
        pic,
        src,
        version,
        guest_cc,
        cc: deps::binary(root, "clang")?,
        ninja: deps::binary(root, "ninja")?,
    })
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

/// Configures `build/` unless it already holds this configuration. A different or interrupted configuration starts from nothing.
/// The stamp names the source dir too: build.ninja holds the source paths, so a task copy's build dir must not be reused from `src/qemu`.
fn configure(b: &Build) -> Result<()> {
    let args = b.configure_args();
    let text = format!("{} source={}", args.join(" "), b.src.display());
    let stamp = b.dir.join(STAMP);
    let build = b.build_dir();
    if std::fs::read_to_string(&stamp).ok().as_deref() == Some(text.as_str())
        && build.join("build.ninja").is_file()
    {
        return Ok(());
    }
    remove_if_present(&stamp)?;
    if build.exists() {
        std::fs::remove_dir_all(&build).with_context(|| format!("removing {}", build.display()))?;
    }
    std::fs::create_dir_all(&build)?;
    let log = b.dir.join("configure.log");
    let status = Command::new(b.src.join("configure"))
        .args(&args)
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

/// Builds `qemu-<target>` (incremental) with ninja.
fn ninja(b: &Build) -> Result<()> {
    let log = b.dir.join("ninja.log");
    let status = Command::new(&b.ninja)
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

/// Configures and builds under one build token, then checks the version. Returns the executable.
pub fn execute(root: &Path, b: &Build) -> Result<PathBuf> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let _token = build_pool::acquire(
        &build_pool::dir(&ws),
        build_pool::TOKENS,
        &build_pool::this_holder(root),
        build_pool::POLL,
        |line| println!("qemu: {line}"),
    )?;
    configure(b)?;
    ninja(b)?;
    check_version(b)?;
    Ok(b.exe())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = plan(root, &args.arch, args.out.as_deref(), args.pic)?;
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
}
