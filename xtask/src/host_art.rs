// SPDX-License-Identifier: MIT
//! Host ART as an app process starts, for test stages that run Java on it (`xtask test native-load`, `xtask test parcel`): the converter's ART staged as an install lays it out (`stage_art`),
//! atl-app-launcher over the default property area atl-property-service writes (the broker's place), the Java pipeline's derived boot classpath, a test's Java compiled with the Java pipeline's JDK and d8.
//! atl-app-launcher is linked against the converter's libsigchain as `xtask native` links it (packages.rs), atl-property-service built plainly, both with cargo into <out>/cargo; the area from the converter's <out>/gen/build.prop.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::{arches, packages, slot, tree};

/// The boot classpath: the dex jars the Java pipeline's goal `platform-bootclasspath` built, one per line in BOOTCLASSPATH order (bp2ninja bootclasspath.go derives it as Soong's product config and derive_classpath do).
const BOOTCLASSPATH: &str = "bootclasspath.txt";
/// The class jars a test compiles against (javac's bootclasspath, d8's --lib): ART's core and framework, the API the tests use.
const JAVAC_BOOT: [&str; 3] = [
    "device/core-oj/javac/core-oj.jar",
    "device/core-libart/javac/core-libart.jar",
    "device/framework-minus-apex/javac-header/framework.jar",
];
/// The property service binary, built by cargo into <out>/cargo (the launcher: packages.rs).
const PROPERTY_SERVICE: &str = "atl-property-service";
/// The launcher package and its executable.
const LAUNCHER: &str = "atl-app-launcher";
/// Written by the converter (bp2ninja buildprop.go) when it builds with a release config.
pub const BUILD_PROP: &str = "gen/build.prop";
/// Where Android installs the ICU data ART's ICU opens (external/icu/icu4c/source/stubdata/Android.bp apex_icu.dat: prebuilt_etc of `icu*.dat`, sub_dir icu, in the com.android.i18n APEX).
const ICU_DATA: &str = "external/icu/icu4c/source/stubdata";

/// The staged ART and the tools around it.
pub struct HostArt {
    pub art: PathBuf,
    /// The staged ART's library dir, every run's LD_LIBRARY_PATH (stage_art).
    pub lib_path: PathBuf,
    pub launcher: PathBuf,
    property_service: PathBuf,
    /// The converter's build.prop, the property service's input.
    pub build_prop: PathBuf,
    pub boot: Vec<PathBuf>,
    javac_boot: Vec<PathBuf>,
    /// The Java pipeline's d8 (a script running r8.jar with the JDK on PATH) and the JDK.
    pub d8: PathBuf,
    pub jdk: PathBuf,
    /// The build directory's cargo target dir: launcher, property service, test libraries.
    pub cargo: PathBuf,
    pub verbose: bool,
    /// atl-property-service `--linear-tier` for [`HostArt::compile`]'s area (a gate's break); None = the probed tier.
    pub linear_tier: Option<&'static str>,
}

/// One ART run's result.
pub struct ArtRun {
    pub ok: bool,
    /// The exit status; None when a signal ended the process.
    pub code: Option<i32>,
    /// The signal that ended the process, if one did.
    pub signal: Option<i32>,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}

impl ArtRun {
    /// Why the run ended badly: ART's / liblog's fatal line, else the last stderr line, plus the exit status.
    pub fn failure(&self) -> String {
        let fatal = self
            .stderr
            .lines()
            .find(|l| l.contains(" F ") || l.contains("FATAL") || l.contains("Abort message"))
            .or_else(|| self.stderr.lines().rfind(|l| !l.trim().is_empty()))
            .unwrap_or("(no stderr)");
        format!("{} [{}]", fatal.trim(), self.status)
    }
}

/// A compiled test: its dex and the property area every run uses.
pub struct JavaFixture {
    pub dex: PathBuf,
    /// The property-area variable as atl-property-service printed it: (name, the area it wrote).
    pub area: (String, String),
    /// The test's work dir (ANDROID_DATA = <work>/data).
    pub work: PathBuf,
}

/// The boot classpath list the Java pipeline wrote into `java_out` (BOOTCLASSPATH); absent when its goal `platform-bootclasspath` was not built or a jar on it could not be.
/// The converter writes the list from declared dex paths, not from built outputs, so a jar the failed build never made can be listed: every listed jar must exist, and the error names those that do not.
fn boot_jars(java_out: &Path) -> Result<Vec<PathBuf>> {
    let file = java_out.join(BOOTCLASSPATH);
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!(
            "reading {}: build goal platform-bootclasspath (cargo xtask native --out {} platform-bootclasspath)",
            file.display(),
            java_out.display()
        )
    })?;
    let jars: Vec<PathBuf> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    let missing: Vec<String> = jars
        .iter()
        .filter(|j| !j.is_file())
        .map(|j| j.display().to_string())
        .collect();
    ensure!(
        missing.is_empty(),
        "{} lists {} jar(s) that were not built; rebuild platform-bootclasspath: {}",
        file.display(),
        missing.len(),
        missing.join(", ")
    );
    Ok(jars)
}

/// The Java pipeline's d8 in `java_out` and the prebuilt JDK of host arch `host` (its arches.toml `soong_prebuilt_os`); not checked to exist.
pub fn java_tools(root: &Path, host: &str, java_out: &Path) -> Result<(PathBuf, PathBuf)> {
    let arches = arches::load(root)?;
    let os = arches
        .get(host)
        .and_then(|a| a.host.as_ref())
        .and_then(|h| h.soong_prebuilt_os.as_deref())
        .with_context(|| {
            format!("{host}: no host row with soong_prebuilt_os (no prebuilt JDK or host ART)")
        })?;
    let layout = Layout::of(root)?;
    let jdk = crate::deps::aosp_project(root, "jdk")?;
    Ok((
        java_out.join("host/bin/d8"),
        arches::java_tools(&tree::shared_tree(&layout), os, &jdk).0,
    ))
}

impl HostArt {
    /// Stages the converter's ART of build dir `out` into `<out>/<stage>` and checks every input exists; `java_out` = the Java pipeline build dir (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    pub fn new(
        root: &Path,
        host: &str,
        java_out: Option<&Path>,
        out: &Path,
        stage: &str,
        verbose: bool,
    ) -> Result<Self> {
        let arches = arches::load(root)?;
        let arch = arches
            .get(host)
            .filter(|a| a.host.is_some())
            .with_context(|| format!("{host}: not a host arch in {}", arches::FILE))?;
        let layout = Layout::of(root)?;
        let java_out = slot::out_dir(root, java_out, "--java-out", |workspace| {
            workspace.join("out/java/build")
        })?;
        let (d8, jdk) = java_tools(root, host, &java_out)?;
        let art = out.join(stage);
        let lib_path = stage_art(out, &art, &tree::shared_tree(&layout), arch.pointer_bits)?;
        let h = HostArt {
            art,
            lib_path,
            launcher: packages::bin(root, out, LAUNCHER, LAUNCHER)?,
            property_service: build_property_service(root, &out.join("cargo"))?,
            build_prop: out.join(BUILD_PROP),
            boot: boot_jars(&java_out)?,
            javac_boot: JAVAC_BOOT.iter().map(|j| java_out.join(j)).collect(),
            d8,
            jdk,
            cargo: out.join("cargo"),
            verbose,
            linear_tier: None,
        };
        for p in [&h.build_prop, &h.d8, &h.jdk]
            .into_iter()
            .chain(&h.boot)
            .chain(&h.javac_boot)
        {
            ensure!(p.exists(), "missing input {}", p.display());
        }
        Ok(h)
    }

    /// Compiles every .java below `java` into `work` (rebuilt every run: a few seconds, no staleness logic to get wrong) and writes the default property area there.
    pub fn compile(&self, java: &Path, work: &Path) -> Result<JavaFixture> {
        if work.exists() {
            std::fs::remove_dir_all(work)?;
        }
        let classes = work.join("classes");
        std::fs::create_dir_all(&classes)?;
        std::fs::create_dir_all(work.join("data"))?;
        let mut sources = Vec::new();
        collect(java, "java", &mut sources)?;
        let jdk_bin = self.jdk.join("bin");
        let bootcp = join_paths(&self.javac_boot);
        exec(
            Command::new(jdk_bin.join("javac"))
                .args([
                    "-source",
                    "8",
                    "-target",
                    "8",
                    "-Xlint:-options",
                    "-bootclasspath",
                ])
                .arg(&bootcp)
                .arg("-d")
                .arg(&classes)
                .args(&sources),
            "javac",
        )?;
        let jar = work.join("classes.jar");
        exec(
            Command::new(jdk_bin.join("jar"))
                .arg("cf")
                .arg(&jar)
                .arg("-C")
                .arg(&classes)
                .arg("."),
            "jar",
        )?;
        let dex = work.join("dex");
        std::fs::create_dir_all(&dex)?;
        let path = std::env::join_paths(std::iter::once(jdk_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))?;
        // installed executable by the Java pipeline (a bash script running r8.jar with the JDK on PATH)
        let mut d8 = Command::new(&self.d8);
        d8.env("PATH", path);
        for lib in &self.javac_boot {
            d8.arg("--lib").arg(lib);
        }
        exec(d8.arg("--output").arg(&dex).arg(&jar), "d8")?;
        // the broker's step: the default property area, its variable printed as NAME=path
        let printed = output(
            Command::new(&self.property_service)
                .arg("--build-prop")
                .arg(&self.build_prop)
                .arg("--out")
                .arg(work.join("properties"))
                .args(self.linear_tier.iter().flat_map(|t| ["--linear-tier", t])),
            "atl-property-service",
        )?;
        let area = printed
            .trim()
            .split_once('=')
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .with_context(|| format!("atl-property-service printed {printed:?}, not NAME=path"))?;
        Ok(JavaFixture {
            dex: dex.join("classes.dex"),
            area,
            work: work.to_path_buf(),
        })
    }

    /// `class args` from `fixture`'s dex, started by the launcher with the property-area variable set to `area` and the extra VM options `vm`.
    pub fn launch(
        &self,
        fixture: &JavaFixture,
        area: &str,
        vm: &[&str],
        class: &str,
        args: &[String],
    ) -> Result<ArtRun> {
        let mut cmd = self.command(fixture, area, vm, class, args);
        let out = cmd
            .output()
            .with_context(|| format!("running {}", self.launcher.display()))?;
        Ok(self.result(&format!("{cmd:?}"), out))
    }

    /// `launch`'s command, for a caller that spawns it itself (through binderd's harness); its stdout and stderr piped for `finish`.
    pub fn command(
        &self,
        fixture: &JavaFixture,
        area: &str,
        vm: &[&str],
        class: &str,
        args: &[String],
    ) -> Command {
        let boot = join_paths(&self.boot);
        let mut cmd = Command::new(&self.launcher);
        cmd.arg(format!("-Xbootclasspath:{boot}"))
            .arg(format!("-Xbootclasspath-locations:{boot}"))
            .arg("-Xnoimage-dex2oat")
            .args(vm)
            .arg("-cp")
            .arg(&fixture.dex)
            .arg(class)
            .args(args)
            .env("ANDROID_ROOT", &self.art)
            .env("ANDROID_ART_ROOT", self.art.join("com.android.art"))
            .env("ANDROID_I18N_ROOT", self.art.join("com.android.i18n"))
            .env("ANDROID_TZDATA_ROOT", self.art.join("com.android.tzdata"))
            .env("ANDROID_DATA", fixture.work.join("data"))
            .env("LD_BIND_NOW", "1")
            .env("LD_LIBRARY_PATH", &self.lib_path)
            .env(&fixture.area.0, area)
            // liblog's minimum priority INFO: libbase InitLogging (ART's runtime start) reads it into __android_log_set_minimum_priority, so liblog itself refuses VERBOSE
            .env("ANDROID_LOG_TAGS", "*:i")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }

    /// The result of a `command` the caller spawned; `what` = that command as printed (`{cmd:?}`), for --verbose.
    pub fn finish(&self, what: &str, child: std::process::Child) -> Result<ArtRun> {
        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for {}", self.launcher.display()))?;
        Ok(self.result(what, out))
    }

    fn result(&self, what: &str, out: std::process::Output) -> ArtRun {
        let run = ArtRun {
            ok: out.status.success(),
            code: out.status.code(),
            signal: std::os::unix::process::ExitStatusExt::signal(&out.status),
            status: out.status.to_string(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        if self.verbose {
            println!("---- {what} -> {}", run.status);
            println!("{}{}", run.stdout, run.stderr);
        }
        run
    }
}

impl HostArt {
    /// What the runtime reads from the staged ART: its tree and library dir (whole, remote.rs `Inputs`), the launcher, the property service's input and the boot jars.
    pub fn runtime_inputs(&self) -> crate::remote::Inputs {
        crate::remote::Inputs {
            files: [
                self.launcher.clone(),
                self.property_service.clone(),
                self.build_prop.clone(),
            ]
            .into_iter()
            .chain(self.boot.iter().cloned())
            .collect(),
            dirs: vec![self.art.clone(), self.lib_path.clone()],
        }
    }
}

/// The lib dirs of the providers `xtask native` built outside the tree: `<out>/crates/<crate>/lib`, `<out>/upstream/<name>/prefix/lib`.
fn provided_lib_dirs(out: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for (parent, sub) in [("crates", "lib"), ("upstream", "prefix/lib")] {
        let Ok(entries) = std::fs::read_dir(out.join(parent)) else {
            continue;
        };
        for e in entries {
            let d = e?.path().join(sub);
            if d.is_dir() {
                dirs.push(d);
            }
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Stages the converter's ART in `out` as an ART root at `art`, laid out as Android installs it and rebuilt every run (links only); returns its library dir.
/// - lib<64|"">: every shared library the converter linked (<out>/device/*/*.so), one dir as Android's APEX lib dirs. The build tree has none: the launcher dlopens libnativehelper, libnativehelper libart.so and libcore System.loadLibrary's libjavacore by name, found on Android through the linker's default search path, here through LD_LIBRARY_PATH.
///   Plus the provided libraries built outside the tree (<out>/crates/*/lib, <out>/upstream/*/prefix/lib: libatl_bionic as libc, BoringSSL), installed beside them as Android installs libc: converter outputs carry no runpath (runpath_gate), so this dir is the only place their needs are found. System providers stay in the distro's default path.
///   libhwui dlopens libandroid by name too (HardwareBufferHelpers, HardwareRenderer, HintSessionWrapper), with no DT_NEEDED on it, so no dependency closure builds it: required here.
/// - com.android.art -> the root (ANDROID_ART_ROOT); com.android.i18n/etc/icu/<ICU_DATA's icu*.dat>; an empty com.android.tzdata.
/// - etc/security/cacerts -> /system/etc/security/cacerts (dangling outside the sandbox): Conscrypt's system CA dir is `$ANDROID_ROOT/etc/security/cacerts` when no APEX one exists; atld writes /system's from the distro bundle.
fn stage_art(out: &Path, art: &Path, tree: &Path, pointer_bits: u32) -> Result<PathBuf> {
    let device = out.join("device");
    for module in ["libart", "libnativehelper", "libandroid"] {
        ensure!(
            device.join(module).join(format!("{module}.so")).is_file(),
            "no {module} in {}: build it with `xtask native ... libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime libandroid`",
            out.display()
        );
    }
    if art.exists() {
        std::fs::remove_dir_all(art)?;
    }
    let lib = art.join(if pointer_bits == 64 { "lib64" } else { "lib" });
    let icu = art.join("com.android.i18n/etc/icu");
    for d in [
        &lib,
        &icu,
        &art.join("com.android.tzdata"),
        &art.join("etc/security"),
    ] {
        std::fs::create_dir_all(d)?;
    }
    let link = |target: &Path, at: &Path| {
        std::os::unix::fs::symlink(target, at)
            .with_context(|| format!("linking {} -> {}", at.display(), target.display()))
    };
    link(Path::new("."), &art.join("com.android.art"))?;
    link(
        Path::new(atl_android_paths::SYSTEM_CACERTS),
        &art.join("etc/security/cacerts"),
    )?;
    for module in std::fs::read_dir(&device)? {
        let module = module?.path();
        if !module.is_dir() {
            continue;
        }
        for f in std::fs::read_dir(&module)? {
            let f = f?.path();
            if f.extension().is_some_and(|e| e == "so") && f.is_file() {
                let name = f.file_name().context("a file name")?;
                link(&f, &lib.join(name))?;
            }
        }
    }
    for provider in provided_lib_dirs(out)? {
        for f in std::fs::read_dir(&provider)? {
            let f = f?.path();
            let name = f.file_name().context("a file name")?;
            // the soname file (libatl_bionic.so.0.0.0) is what DT_NEEDED names
            if name.to_string_lossy().contains(".so") && f.is_file() {
                link(&f, &lib.join(name))?;
            }
        }
    }
    let mut data = 0;
    for f in std::fs::read_dir(tree.join(ICU_DATA))? {
        let f = f?.path();
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with("icu") && name.ends_with(".dat") {
            link(&f, &icu.join(name))?;
            data += 1;
        }
    }
    ensure!(data > 0, "no icu*.dat in {}", tree.join(ICU_DATA).display());
    Ok(lib)
}

pub fn collect(dir: &Path, ext: &str, into: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.is_dir() {
            collect(&p, ext, into)?;
        } else if p.extension().is_some_and(|x| x == ext) {
            into.push(p);
        }
    }
    into.sort();
    Ok(())
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

pub fn exec(cmd: &mut Command, what: &str) -> Result<()> {
    output(cmd, what).map(drop)
}

/// `cmd`'s stdout; a failure with its output.
pub fn output(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    ensure!(
        out.status.success(),
        "{what} failed ({}):\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`PROPERTY_SERVICE`], release, in the cargo target dir `target` (cargo's own staleness check: a no-op when current); its path.
fn build_property_service(root: &Path, target: &Path) -> Result<PathBuf> {
    exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", PROPERTY_SERVICE])
            .arg("--target-dir")
            .arg(target),
        "cargo build (property service)",
    )?;
    let bin = target.join("release").join(PROPERTY_SERVICE);
    ensure!(bin.is_file(), "{} missing after cargo build", bin.display());
    Ok(bin)
}
