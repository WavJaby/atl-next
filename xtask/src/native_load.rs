// SPDX-License-Identifier: MIT
//! `xtask test native-load`: the converter-built libandroid_runtime on the converter's ART (`stage_art`), started as an app process starts: atl-app-launcher over the default property area atl-property-service writes (the broker's place), in four stages.
//!
//! 1. load: `System.load` with `LD_BIND_NOW=1`, so every undefined symbol of every object it pulls in must resolve at load time (lazy binding would fail only at the first call).
//! 2. register: `AndroidRuntime::startReg`, then gRegJNI walked entry by entry (startReg's own loop, register_jni_procs) -> how many register before the first failure.
//!    framework.jar (the converter's framework-minus-apex) is on the boot classpath; the walk stops at the first entry that fails or aborts: a measured count, never a failure. Then register_android_os_SystemClock / _util_Log / _util_EventLog alone: each must return >= 0.
//! 3. call: natives checked against Android semantics -- uptimeMillis is CLOCK_MONOTONIC in ms (android_os_SystemClock.cpp; bracketed by System.nanoTime, also CLOCK_MONOTONIC in ART),
//!    isLoggable defaults to INFO (Log.isLoggable docs; liblog __android_log_is_loggable), println_native returns liblog __android_log_buf_write's 1 for INFO and -EPERM (-1) for VERBOSE, which liblog drops before any logger under the minimum priority INFO every run sets (`ANDROID_LOG_TAGS=*:i`; without it buf_write's default is VERBOSE and Log.v is written, as on Android);
//!    the INFO message reaches stderr as the launcher's logger writes it (`<L> liblog/<tag>: <text>`), the VERBOSE one nowhere: the logger receives only what liblog let through.
//! 4. copies: each library of [`SINGLE_COPY`] mapped once after the load (/proc/self/maps). A warning by default; `--strict-copies` makes it a failure.
//!
//! Fixture (xtask/testdata/native-load): NativeLoad.java (driver), compiled against the boot class jars (framework's included) with the Java pipeline's JDK and d8;
//! shim.rs, a JNI cdylib built with plain rustc, calls the C++ entry points (hidden register_* by .symtab vaddr). Built into <out>/native-load, never shipped.
//! atl-app-launcher and atl-property-service are built with cargo into <out>/cargo; the area from the converter's <out>/gen/build.prop.
//! `--self-test` feeds the guards broken inputs and fails unless each fires: the load guard (a non-ELF file, a library with an undefined symbol), the launcher's (no property area); the result checks are unit tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use object::{
    Object, ObjectSection, ObjectSymbol, ObjectSymbolTable, RelocationTarget, SectionKind,
};

use crate::{arches, farm};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): dalvikvm<pointer_bits>, the JDK's prebuilt OS directory, the default --out.
    #[arg(long)]
    host: String,
    /// ART root: bin/dalvikvm<bits>, com.android.art, com.android.i18n, com.android.tzdata, e.g. Soong's host ART (<workspace>/out/aosp/<product>/host/<soong_prebuilt_os>, as `xtask soong` builds it).
    /// Default: the converter's ART in --out (`xtask native` goals dalvikvm, libart, ...), staged into <out>/native-load-art as an install would lay it out (see `stage_art`).
    #[arg(long)]
    art: Option<PathBuf>,
    /// Java pipeline build directory: the ART host boot jars, framework-minus-apex and its d8 (default <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Native build directory holding libandroid_runtime (default <workspace>/out/native/<host>, as `xtask native`); the fixture is built into its native-load/.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Stage 4 fails, instead of warning, when a library is mapped more than once (for an ART built by the converter).
    #[arg(long)]
    strict_copies: bool,
    /// Instead of the stages: check the load guard refuses broken inputs.
    #[arg(long)]
    self_test: bool,
    /// Print every ART run's command, stdout and stderr.
    #[arg(long, short)]
    verbose: bool,
}

/// The boot classpath: the ART host boot jars (core-oj, core-libart, core-icu4j hostdex), then framework.jar (framework-minus-apex's dex
/// jar, stem framework), as the Java pipeline lays them out under --java-out.
const BOOT_JARS: [&str; 4] = [
    "device/core-oj/aligned/core-oj.jar",
    "device/core-libart/aligned/core-libart.jar",
    "device/core-icu4j-host/dex-withres-aligned/core-icu4j.jar",
    "device/framework-minus-apex/aligned/framework.jar",
];
/// Their class jars: javac's bootclasspath and d8's --lib.
const JAVAC_BOOT: [&str; 3] = [
    "device/core-oj/javac/core-oj.jar",
    "device/core-libart/javac/core-libart.jar",
    "device/framework-minus-apex/javac-header/framework.jar",
];
/// Soong's JDK (build/soong/java/config: prebuilts/jdk/jdk21), under the host's soong_prebuilt_os.
const JDK: &str = "prebuilts/jdk/jdk21";
const FIXTURE: &str = "xtask/testdata/native-load";
const RUNTIME: &str = "device/libandroid_runtime/libandroid_runtime.so";
/// `android::AndroidRuntime::startReg(JNIEnv*)` and `android::gRegJNI` (internal linkage), Itanium-mangled.
const START_REG: &str = "_ZN7android14AndroidRuntime8startRegEP7_JNIEnv";
const REG_JNI: &str = "_ZN7androidL7gRegJNIE";
/// Registered alone in stage 2 (the classes stage 3 calls into, plus EventLog).
const ALONE: [&str; 3] = [
    "register_android_os_SystemClock",
    "register_android_util_Log",
    "register_android_util_EventLog",
];
/// Stage 4: libraries that must be mapped once. Key = file name up to `.so`, without `-host` (Soong's host ICU is libicuuc-host.so).
const SINGLE_COPY: [&str; 7] = [
    "libbase",
    "liblog",
    "libnativehelper",
    "libicuuc",
    "libicui18n",
    "libicu",
    "libc++",
];

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let env = Env::new(root, args)?;
    let fixture = env.build_fixture(root)?;
    if args.self_test {
        return self_test(root, &env, &fixture);
    }
    let mut report = Report::default();

    // 1 + 4: one run
    let load = env.art(&fixture, "load", &[env.runtime.display().to_string()])?;
    let maps = match load_result(&load) {
        Ok(maps) => {
            report.pass(
                "load",
                "loaded, every symbol bound (LD_BIND_NOW=1)".to_owned(),
            );
            maps
        }
        Err(e) => {
            report.fail("load", e);
            return Ok(report.finish());
        }
    };

    // 2
    let elf = RuntimeElf::read(&env.runtime)?;
    let base = [
        env.runtime.display().to_string(),
        fixture.shim.display().to_string(),
    ];
    let start = env.art(&fixture, "startreg", &base)?;
    let walk_args: Vec<String> = base
        .iter()
        .cloned()
        .chain(std::iter::once(format!("{:x}", elf.start_reg)))
        .chain(elf.reg_jni.iter().map(|(v, _)| format!("{v:x}")))
        .collect();
    let walk = env.art(&fixture, "walk", &walk_args)?;
    let names: Vec<&str> = elf.reg_jni.iter().map(|(_, n)| n.as_str()).collect();
    report.measured("startReg", start_reg_result(&start));
    report.measured("gRegJNI", walk_result(&walk, &names));

    let mut call_args: Vec<String> = base.to_vec();
    call_args.push(format!("{:x}", elf.start_reg));
    for name in ALONE {
        call_args.push(format!("{:x}", elf.symbol(&register_symbol(name))?));
    }
    let call = env.art(&fixture, "call", &call_args)?;
    match alone_result(&call, &ALONE) {
        Ok(s) => report.pass("register", s),
        Err(e) => report.fail("register", e),
    }

    // 3
    let checks = call_checks(&call);
    let failed: Vec<&String> = checks.iter().filter_map(|c| c.as_ref().err()).collect();
    if failed.is_empty() {
        let ok: Vec<&str> = checks
            .iter()
            .filter_map(|c| c.as_ref().ok().map(String::as_str))
            .collect();
        report.pass("call", ok.join("; "));
    } else {
        report.fail(
            "call",
            failed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }

    // 4
    let dups = duplicates(&maps);
    if dups.is_empty() {
        report.pass(
            "copies",
            format!("{} libraries mapped once", SINGLE_COPY.len()),
        );
    } else {
        let detail = dups
            .iter()
            .map(|(lib, paths)| format!("{lib} x{}: {}", paths.len(), paths.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        if args.strict_copies {
            report.fail("copies", detail);
        } else {
            report.warn("copies", detail);
        }
    }
    Ok(report.finish())
}

// ---- inputs --------------------------------------------------------------------------------------

struct Env {
    art: PathBuf,
    /// The staged ART's library dir, every run's LD_LIBRARY_PATH (stage_art).
    lib_path: PathBuf,
    launcher: PathBuf,
    property_service: PathBuf,
    /// The converter's build.prop, the property service's input.
    build_prop: PathBuf,
    boot: Vec<PathBuf>,
    javac_boot: Vec<PathBuf>,
    d8: PathBuf,
    jdk: PathBuf,
    runtime: PathBuf,
    work: PathBuf,
    verbose: bool,
}

struct Fixture {
    dex: PathBuf,
    shim: PathBuf,
    /// Every run's property-area variable as atl-property-service printed it: (name, the area it wrote).
    area: (String, String),
}

/// The workspace binaries an app process starts with, built by cargo into <out>/cargo.
const LAUNCHER: &str = "atl-app-launcher";
const PROPERTY_SERVICE: &str = "atl-property-service";
/// Written by the converter (bp2ninja buildprop.go) when it builds with a release config.
const BUILD_PROP: &str = "gen/build.prop";
/// The launcher's refusal without a property area (atl-app-launcher main.rs).
const NO_AREA: &str = "no system property area";

impl Env {
    fn new(root: &Path, args: &Args) -> Result<Self> {
        let arches = arches::load(root)?;
        let arch = arches
            .get(&args.host)
            .filter(|a| a.host.is_some())
            .with_context(|| format!("{}: not a host arch in {}", args.host, arches::FILE))?;
        let host = arch.host.as_ref().context("checked above")?;
        let os = host.soong_prebuilt_os.as_deref().with_context(|| {
            format!(
                "{}: no soong_prebuilt_os (no prebuilt JDK or host ART)",
                args.host
            )
        })?;
        let layout = Layout::of(root)?;
        let ws = layout.workspace();
        let java_out = match &args.java_out {
            Some(dir) => std::path::absolute(dir)?,
            None => ws.join("out/java/build"),
        };
        let out = match &args.out {
            Some(dir) => std::path::absolute(dir)?,
            None => ws.join("out/native").join(&args.host),
        };
        let art = out.join("native-load-art");
        let lib_path = stage_art(&out, &art, &farm::shared_tree(&layout), arch.pointer_bits)?;
        let bin = build_binaries(root, &out)?;
        let env = Env {
            art,
            lib_path,
            launcher: bin.join(LAUNCHER),
            property_service: bin.join(PROPERTY_SERVICE),
            build_prop: out.join(BUILD_PROP),
            boot: BOOT_JARS.iter().map(|j| java_out.join(j)).collect(),
            javac_boot: JAVAC_BOOT.iter().map(|j| java_out.join(j)).collect(),
            d8: java_out.join("host/bin/d8"),
            jdk: farm::shared_tree(&layout).join(JDK).join(os),
            runtime: out.join(RUNTIME),
            work: out.join("native-load"),
            verbose: args.verbose,
        };
        for p in [&env.build_prop, &env.d8, &env.jdk, &env.runtime]
            .into_iter()
            .chain(&env.boot)
            .chain(&env.javac_boot)
        {
            ensure!(p.exists(), "missing input {}", p.display());
        }
        Ok(env)
    }

    /// The fixture into <out>/native-load, rebuilt every run (a few seconds; no staleness logic to get wrong).
    fn build_fixture(&self, root: &Path) -> Result<Fixture> {
        let fixture = root.join(FIXTURE);
        if self.work.exists() {
            std::fs::remove_dir_all(&self.work)?;
        }
        let classes = self.work.join("classes");
        std::fs::create_dir_all(&classes)?;
        std::fs::create_dir_all(self.work.join("data"))?;
        let mut java = Vec::new();
        collect(&fixture.join("java"), "java", &mut java)?;
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
                .args(&java),
            "javac",
        )?;
        let jar = self.work.join("classes.jar");
        exec(
            Command::new(jdk_bin.join("jar"))
                .arg("cf")
                .arg(&jar)
                .arg("-C")
                .arg(&classes)
                .arg("."),
            "jar",
        )?;
        let dex = self.work.join("dex");
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
        let shim = self.work.join("libnlt_shim.so");
        rustc(root, &fixture.join("shim.rs"), &shim, &[])?;
        // the broker's step: the default property area, its variable printed as NAME=path
        let printed = output(
            Command::new(&self.property_service)
                .arg("--build-prop")
                .arg(&self.build_prop)
                .arg("--out")
                .arg(self.work.join("properties")),
            "atl-property-service",
        )?;
        let area = printed
            .trim()
            .split_once('=')
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .with_context(|| format!("atl-property-service printed {printed:?}, not NAME=path"))?;
        Ok(Fixture {
            dex: dex.join("classes.dex"),
            shim,
            area,
        })
    }

    /// NativeLoad.java `mode args`, started by the launcher over the fixture's property area.
    fn art(&self, fixture: &Fixture, mode: &str, args: &[String]) -> Result<ArtRun> {
        self.launch(fixture, &fixture.area.1, mode, args)
    }

    /// `art` with the property-area variable set to `area`.
    fn launch(&self, fixture: &Fixture, area: &str, mode: &str, args: &[String]) -> Result<ArtRun> {
        let boot = join_paths(&self.boot);
        let mut cmd = Command::new(&self.launcher);
        cmd.arg(format!("-Xbootclasspath:{boot}"))
            .arg(format!("-Xbootclasspath-locations:{boot}"))
            .arg("-Xnoimage-dex2oat")
            .arg("-cp")
            .arg(&fixture.dex)
            .arg("NativeLoad")
            .arg(mode)
            .args(args)
            .env("ANDROID_ROOT", &self.art)
            .env("ANDROID_ART_ROOT", self.art.join("com.android.art"))
            .env("ANDROID_I18N_ROOT", self.art.join("com.android.i18n"))
            .env("ANDROID_TZDATA_ROOT", self.art.join("com.android.tzdata"))
            .env("ANDROID_DATA", self.work.join("data"))
            .env("LD_BIND_NOW", "1")
            .env("LD_LIBRARY_PATH", &self.lib_path)
            .env(&fixture.area.0, area)
            // liblog's minimum priority INFO: libbase InitLogging (ART's runtime start) reads it into __android_log_set_minimum_priority, so liblog itself refuses VERBOSE (stage 3)
            .env("ANDROID_LOG_TAGS", "*:i");
        let out = cmd
            .output()
            .with_context(|| format!("running {}", self.launcher.display()))?;
        let run = ArtRun {
            ok: out.status.success(),
            status: out.status.to_string(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        if self.verbose {
            println!("---- {mode}: {cmd:?} -> {}", run.status);
            println!("{}{}", run.stdout, run.stderr);
        }
        Ok(run)
    }
}

/// Where Android installs the ICU data ART's ICU opens (external/icu/icu4c/source/stubdata/Android.bp apex_icu.dat: prebuilt_etc of `icu*.dat`, sub_dir icu, in the com.android.i18n APEX).
const ICU_DATA: &str = "external/icu/icu4c/source/stubdata";

/// Stages the converter's ART in `out` as an ART root at `art`, laid out as Android installs it and rebuilt every run (links only); returns its library dir.
/// - lib<64|"">: every shared library the converter linked (<out>/device/*/*.so), one dir as Android's APEX lib dirs. The build tree has none: the launcher dlopens libnativehelper, libnativehelper libart.so and libcore System.loadLibrary's libjavacore by name, found on Android through the linker's default search path, here through LD_LIBRARY_PATH. Their own RUNPATHs reach the rest.
/// - com.android.art -> the root (ANDROID_ART_ROOT); com.android.i18n/etc/icu/<ICU_DATA's icu*.dat>; an empty com.android.tzdata.
fn stage_art(out: &Path, art: &Path, tree: &Path, pointer_bits: u32) -> Result<PathBuf> {
    let device = out.join("device");
    for module in ["libart", "libnativehelper"] {
        ensure!(
            device.join(module).join(format!("{module}.so")).is_file(),
            "no converter ART in {}: build it with `xtask native ... libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime`",
            out.display()
        );
    }
    if art.exists() {
        std::fs::remove_dir_all(art)?;
    }
    let lib = art.join(if pointer_bits == 64 { "lib64" } else { "lib" });
    let icu = art.join("com.android.i18n/etc/icu");
    for d in [&lib, &icu, &art.join("com.android.tzdata")] {
        std::fs::create_dir_all(d)?;
    }
    let link = |target: &Path, at: &Path| {
        std::os::unix::fs::symlink(target, at)
            .with_context(|| format!("linking {} -> {}", at.display(), target.display()))
    };
    link(Path::new("."), &art.join("com.android.art"))?;
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

fn collect(dir: &Path, ext: &str, into: &mut Vec<PathBuf>) -> Result<()> {
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

fn exec(cmd: &mut Command, what: &str) -> Result<()> {
    output(cmd, what).map(drop)
}

/// `cmd`'s stdout; a failure with its output.
fn output(cmd: &mut Command, what: &str) -> Result<String> {
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

/// [`LAUNCHER`] and [`PROPERTY_SERVICE`], release, in the build directory's cargo target dir (cargo's own staleness check: a no-op when current); their directory.
fn build_binaries(root: &Path, out: &Path) -> Result<PathBuf> {
    let target = out.join("cargo");
    exec(
        crate::cargo_command()
            .current_dir(root)
            .args([
                "build",
                "--release",
                "--locked",
                "-p",
                LAUNCHER,
                "-p",
                PROPERTY_SERVICE,
            ])
            .arg("--target-dir")
            .arg(&target),
        "cargo build (launcher, property service)",
    )?;
    let bin = target.join("release");
    for b in [LAUNCHER, PROPERTY_SERVICE] {
        ensure!(
            bin.join(b).is_file(),
            "{} missing after cargo build",
            bin.join(b).display()
        );
    }
    Ok(bin)
}

/// A host cdylib from one Rust file, with the repo's pinned toolchain (rustup reads rust-toolchain.toml in `root`).
fn rustc(root: &Path, src: &Path, out: &Path, extra: &[&str]) -> Result<()> {
    exec(
        Command::new("rustc")
            .current_dir(root)
            .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
            .arg(out)
            .args(extra)
            .arg(src),
        "rustc",
    )
}

// ---- the library ---------------------------------------------------------------------------------

struct RuntimeElf {
    symbols: BTreeMap<String, u64>,
    start_reg: u64,
    /// gRegJNI's mProc slots in order: (slot vaddr, function name).
    reg_jni: Vec<(u64, String)>,
}

impl RuntimeElf {
    fn read(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file =
            object::File::parse(&*data).with_context(|| format!("parsing {}", path.display()))?;
        let mut symbols = BTreeMap::new();
        let mut functions = BTreeMap::new();
        for s in file.symbols() {
            if let Ok(name) = s.name() {
                symbols.insert(name.to_owned(), s.address());
                if s.kind() == object::SymbolKind::Text && s.address() != 0 {
                    functions.insert(s.address(), name.to_owned());
                }
            }
        }
        ensure!(
            !symbols.is_empty(),
            "{}: no .symtab (stripped): register_* and gRegJNI are found there",
            path.display()
        );
        let start_reg = *symbols
            .get(START_REG)
            .with_context(|| format!("no {START_REG}"))?;
        let table = file
            .symbols()
            .find(|s| s.name() == Ok(REG_JNI))
            .with_context(|| format!("no {REG_JNI} in .symtab"))?;
        let reg_jni = reg_jni(&file, &functions, table.address(), table.size())?;
        Ok(RuntimeElf {
            symbols,
            start_reg,
            reg_jni,
        })
    }

    fn symbol(&self, name: &str) -> Result<u64> {
        self.symbols
            .get(name)
            .copied()
            .with_context(|| format!("no {name} in .symtab"))
    }
}

/// gRegJNI's mProc slots in order: (slot vaddr, function name). The walk calls through the slot as relocated at run time: 6 of 172 point into other libraries (libhwui's register_android_graphics_*), which no vaddr of this object reaches.
/// Read from the dynamic relocations over the table: RegJNIRec is {mProc} with NDEBUG (Soong's default cflags), {mProc, mName} without, so a slot is an mProc unless its value lands outside executable code (mName, a string).
/// Relocation types are not decoded: absolute target -> addend (RELATIVE); symbol -> that symbol (defined: its value + addend; undefined: another library's function); REL (implicit addend) reads the slot.
fn reg_jni(
    file: &object::File,
    functions: &BTreeMap<u64, String>,
    addr: u64,
    size: u64,
) -> Result<Vec<(u64, String)>> {
    let word = if file.is_64() { 8 } else { 4 };
    let little = file.is_little_endian();
    let bytes = file
        .sections()
        .find_map(|s| s.data_range(addr, size).ok().flatten())
        .context("gRegJNI's bytes are in no section")?;
    let dynsym = file.dynamic_symbol_table();
    let text = |v: u64| {
        file.sections().any(|s| {
            s.kind() == SectionKind::Text && (s.address()..s.address() + s.size()).contains(&v)
        })
    };
    let mut slots = BTreeMap::new();
    for (offset, r) in file.dynamic_relocations().into_iter().flatten() {
        if !(addr..addr + size).contains(&offset) {
            continue;
        }
        let at = usize::try_from(offset - addr)?;
        let implicit = if r.has_implicit_addend() {
            let b = bytes.get(at..at + word).context("slot past gRegJNI")?;
            let mut buf = [0u8; 8];
            if little {
                buf[..word].copy_from_slice(b);
                u64::from_le_bytes(buf)
            } else {
                buf[8 - word..].copy_from_slice(b);
                u64::from_be_bytes(buf)
            }
        } else {
            0
        };
        let value = |base: u64| base.wrapping_add_signed(r.addend()).wrapping_add(implicit);
        let local = |v: u64| {
            text(v).then(|| {
                functions
                    .get(&v)
                    .map_or_else(|| format!("<{v:#x}>"), |n| demangle_register(n))
            })
        };
        let slot = match r.target() {
            RelocationTarget::Absolute => local(value(0)),
            RelocationTarget::Symbol(i) => {
                let sym = dynsym
                    .as_ref()
                    .context("symbol relocation without .dynsym")?
                    .symbol_by_index(i)?;
                if sym.is_undefined() {
                    Some(demangle_register(sym.name()?))
                } else {
                    local(value(sym.address()))
                }
            }
            _ => bail!("gRegJNI slot {at}: unexpected relocation target"),
        };
        if let Some(name) = slot {
            slots.insert(offset, name);
        }
    }
    // every record relocated: a slot the walk would read as null otherwise
    let n = u64::try_from(slots.len())?;
    ensure!(
        n > 0 && (n * word as u64 == size || 2 * n * word as u64 == size),
        "gRegJNI: {n} relocated function slots in {size} bytes, neither {{mProc}} nor {{mProc, mName}} records"
    );
    Ok(slots.into_iter().collect())
}

/// `_ZN7android<n>register_xEP7_JNIEnv` / `_Z<n>register_xP7_JNIEnv` -> `register_x`; other names as they are.
fn demangle_register(name: &str) -> String {
    name.find("register_")
        .and_then(|i| {
            let s = &name[i..];
            s.strip_suffix("EP7_JNIEnv")
                .or_else(|| s.strip_suffix("P7_JNIEnv"))
        })
        .map_or_else(|| name.to_owned(), str::to_owned)
}

/// `android::<name>(JNIEnv*)`, Itanium-mangled.
fn register_symbol(name: &str) -> String {
    format!("_ZN7android{}{name}EP7_JNIEnv", name.len())
}

// ---- results -------------------------------------------------------------------------------------

struct ArtRun {
    ok: bool,
    status: String,
    stdout: String,
    stderr: String,
}

impl ArtRun {
    /// Values of the `nlt <key> <value>` lines with this key.
    fn values(&self, key: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter_map(|l| l.strip_prefix("nlt "))
            .filter_map(|l| {
                let (k, v) = l.split_once(' ').unwrap_or((l, ""));
                (k == key).then_some(v)
            })
            .collect()
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.values(key).into_iter().next()
    }

    /// Why the run ended badly: ART's / liblog's fatal line, else the last stderr line, plus the exit status.
    fn failure(&self) -> String {
        let fatal = self
            .stderr
            .lines()
            .find(|l| l.contains(" F ") || l.contains("FATAL") || l.contains("Abort message"))
            .or_else(|| self.stderr.lines().rfind(|l| !l.trim().is_empty()))
            .unwrap_or("(no stderr)");
        format!("{} [{}]", fatal.trim(), self.status)
    }
}

/// Stage 1: the mapped files, or the loader's message.
fn load_result(run: &ArtRun) -> Result<Vec<String>, String> {
    if let Some(e) = run.value("load-error") {
        return Err(format!("UnsatisfiedLinkError: {}", e.replace('\t', ": ")));
    }
    if run.value("loaded").is_none() || !run.ok {
        return Err(format!("no load: {}", run.failure()));
    }
    Ok(run.values("map").into_iter().map(str::to_owned).collect())
}

fn start_reg_result(run: &ArtRun) -> String {
    match run.value("startreg") {
        Some(rc) => format!("returned {rc}"),
        None => format!("aborted: {}", run.failure()),
    }
}

/// Stage 2 walk: `k/n registered`, then the entry that failed and why.
fn walk_result(run: &ArtRun, names: &[&str]) -> String {
    let ok = run
        .values("ret")
        .iter()
        .filter(|v| {
            v.split(' ')
                .nth(1)
                .and_then(|rc| rc.parse::<i64>().ok())
                .is_some_and(|rc| rc >= 0)
        })
        .count();
    let mut s = format!("{ok}/{} registered", names.len());
    if ok < names.len() {
        let name = names.get(ok).copied().unwrap_or("?");
        let why = match run.values("ret").get(ok) {
            Some(v) => format!("returned {}", v.split(' ').nth(1).unwrap_or("?")),
            None => format!("aborted: {}", run.failure()),
        };
        s.push_str(&format!("; entry {} {name} {why}", ok + 1));
    }
    s
}

/// Stage 2 alone: every listed registration returned >= 0.
fn alone_result(run: &ArtRun, names: &[&str]) -> Result<String, String> {
    let rets = run.values("ret");
    for (i, name) in names.iter().enumerate() {
        match rets.get(i).and_then(|v| v.split(' ').nth(1)) {
            Some(rc) if rc.parse::<i64>().is_ok_and(|rc| rc >= 0) => {}
            Some(rc) => return Err(format!("{name} returned {rc}")),
            None => return Err(format!("{name} aborted: {}", run.failure())),
        }
    }
    Ok(format!("{} registered alone", names.join(", ")))
}

/// Stage 3: one entry per native call, Ok(summary) or Err(why).
fn call_checks(run: &ArtRun) -> Vec<Result<String, String>> {
    let missing = || format!("no result: {}", run.failure());
    let expect = |key: &str, want: &str, what: &str| match run.value(key) {
        Some(v) if v == want => Ok(format!("{what} = {v}")),
        Some(v) => Err(format!("{what} = {v}, expected {want}")),
        None => Err(format!("{what}: {}", missing())),
    };
    let uptime = match run.value("uptime").map(|v| {
        v.split(' ')
            .map(str::parse::<i64>)
            .collect::<Result<Vec<_>, _>>()
    }) {
        Some(Ok(v)) if v.len() == 3 => {
            const NS_PER_MS: i64 = 1_000_000;
            let (lo, up, hi) = (v[0] / NS_PER_MS, v[1], v[2] / NS_PER_MS);
            if (lo..=hi).contains(&up) {
                Ok(format!(
                    "uptimeMillis = {up} (CLOCK_MONOTONIC {lo}..={hi} ms)"
                ))
            } else {
                Err(format!(
                    "uptimeMillis = {up}, outside CLOCK_MONOTONIC {lo}..={hi} ms"
                ))
            }
        }
        Some(_) => Err("uptimeMillis: malformed result".to_owned()),
        None => Err(format!("uptimeMillis: {}", missing())),
    };
    vec![
        uptime,
        // Log.isLoggable: "The default level of any tag is set to INFO"
        expect("loggable-verbose", "false", "isLoggable(VERBOSE)"),
        expect("loggable-info", "true", "isLoggable(INFO)"),
        // liblog logger_write.cpp __android_log_buf_write: `return 1` once the message is written
        expect("println", "1", "println_native"),
        // ... and `return -EPERM` (-1) when __android_log_is_loggable refuses, before any logger
        expect("println-verbose", "-1", "println_native(VERBOSE)"),
        logger_result(run),
    ]
}

/// NativeLoad.java's two println_native messages: INFO and VERBOSE.
const LOGGED: &str = "native-load println_native";
const FILTERED: &str = "native-load verbose";

/// The launcher's logger got the INFO message (its line: `I liblog/<tag>: <text>`, atl-app-launcher log.rs) and not the VERBOSE one liblog dropped.
fn logger_result(run: &ArtRun) -> Result<String, String> {
    let ours = format!(" I liblog/nlt: {LOGGED}");
    if !run.stderr.lines().any(|l| l.ends_with(&ours)) {
        return Err(format!("logger: no `{ours}` line on stderr"));
    }
    if run.stderr.contains(FILTERED) {
        return Err(format!("logger: wrote `{FILTERED}`, which liblog refuses"));
    }
    Ok("logger: INFO written, VERBOSE never reached it".to_owned())
}

/// Stage 4: libraries of [`SINGLE_COPY`] mapped from more than one path.
fn duplicates(maps: &[String]) -> BTreeMap<&'static str, Vec<String>> {
    let mut by_lib: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for path in maps {
        let file = path.rsplit('/').next().unwrap_or(path);
        let Some(stem) = file.split_once(".so").map(|(s, _)| s) else {
            continue;
        };
        let stem = stem.strip_suffix("-host").unwrap_or(stem);
        if let Some(lib) = SINGLE_COPY.iter().find(|l| **l == stem) {
            by_lib.entry(lib).or_default().push(path.clone());
        }
    }
    by_lib.retain(|_, paths| paths.len() > 1);
    by_lib
}

#[derive(Default)]
struct Report {
    failed: bool,
}

impl Report {
    fn line(&self, stage: &str, verdict: &str, detail: &str) {
        println!("native-load: {stage:<9} {verdict:<8} {detail}");
    }
    fn pass(&mut self, stage: &str, detail: String) {
        self.line(stage, "pass", &detail);
    }
    fn measured(&mut self, stage: &str, detail: String) {
        self.line(stage, "measured", &detail);
    }
    fn warn(&mut self, stage: &str, detail: String) {
        self.line(stage, "WARN", &detail);
    }
    fn fail(&mut self, stage: &str, detail: String) {
        self.failed = true;
        self.line(stage, "FAIL", &detail);
    }
    fn finish(self) -> ExitCode {
        if self.failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

// ---- self test -----------------------------------------------------------------------------------

/// The load guard on real ART: each broken input must be refused with the loader's reason.
fn self_test(root: &Path, env: &Env, fixture: &Fixture) -> Result<ExitCode> {
    let broken = env.work.join("broken");
    std::fs::create_dir_all(&broken)?;
    let not_elf = broken.join("libnot_elf.so");
    // longer than an ELF header: a shorter file is refused as "file too short" before the magic is read
    std::fs::write(&not_elf, "not an ELF object\n".repeat(16))?;
    let unresolved = broken.join("libnlt_unresolved.so");
    // the linker must not refuse the undefined symbol itself: the loader is what is tested
    rustc(
        root,
        &root.join(FIXTURE).join("unresolved.rs"),
        &unresolved,
        &["-C", "link-arg=-Wl,-z,undefs"],
    )?;
    let cases = [
        ("non-ELF file", not_elf, "invalid ELF header"),
        (
            "undefined symbol",
            unresolved,
            "undefined symbol: nlt_missing_symbol",
        ),
    ];
    let mut report = Report::default();
    for (what, lib, reason) in cases {
        let run = env.art(fixture, "load", &[lib.display().to_string()])?;
        match load_result(&run) {
            Err(e) if e.contains(reason) => {
                report.pass("self-test", format!("{what} refused: {e}"))
            }
            Err(e) => report.fail(
                "self-test",
                format!("{what} refused without `{reason}`: {e}"),
            ),
            Ok(_) => report.fail(
                "self-test",
                format!("{what} loaded: the load guard did not fire"),
            ),
        }
    }
    // the launcher's guard: an area path with no file behind it (and none at /dev/__properties__ on this host) must stop the start
    let missing = broken.join("no-property-area");
    let run = env.launch(
        fixture,
        &missing.display().to_string(),
        "load",
        &[env.runtime.display().to_string()],
    )?;
    match area_refused(&run) {
        Ok(s) => report.pass("self-test", s),
        Err(e) => report.fail("self-test", e),
    }
    Ok(report.finish())
}

/// The launcher refused to start: failed, its refusal on stderr, the fixture never ran.
fn area_refused(run: &ArtRun) -> Result<String, String> {
    if run.ok || run.value("loaded").is_some() {
        return Err("no property area: the launcher started the VM anyway".to_owned());
    }
    match run.stderr.lines().find(|l| l.contains(NO_AREA)) {
        Some(l) => Ok(format!("no property area refused: {}", l.trim())),
        None => Err(format!(
            "no property area: failed without `{NO_AREA}`: {}",
            run.failure()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stdout: &str, ok: bool) -> ArtRun {
        ArtRun {
            ok,
            status: if ok {
                "exit status: 0"
            } else {
                "signal: 6 (SIGABRT)"
            }
            .to_owned(),
            stdout: stdout.to_owned(),
            stderr: "x F nlt: Unable to find class android/util/MemoryIntArray\n".to_owned(),
        }
    }

    #[test]
    fn load_guard_fires_on_link_error() {
        let r = run(
            "nlt load-error /l.so\tdlopen failed: undefined symbol: foo\n",
            false,
        );
        let e = load_result(&r).expect_err("guard must fire");
        assert!(e.contains("undefined symbol: foo"), "{e}");
        assert!(load_result(&run("nlt loaded /l.so\nnlt map /a\n", true)).is_ok());
        // a crash after a successful System.load is still a load failure
        assert!(load_result(&run("nlt loaded /l.so\n", false)).is_err());
    }

    #[test]
    fn walk_counts_until_abort() {
        let r = run(
            "nlt reg 0\nnlt ret 0 0\nnlt reg 1\nnlt ret 1 0\nnlt reg 2\n",
            false,
        );
        let s = walk_result(&r, &["a", "b", "register_x", "d"]);
        assert!(
            s.starts_with("2/4 registered; entry 3 register_x aborted"),
            "{s}"
        );
        assert!(s.contains("MemoryIntArray"), "{s}");
        let neg = run("nlt ret 0 0\nnlt ret 1 -1\n", true);
        assert!(walk_result(&neg, &["a", "b"]).contains("entry 2 b returned -1"));
    }

    #[test]
    fn alone_fails_on_negative_or_abort() {
        let good = run("nlt ret 0 0\nnlt ret 1 0\n", true);
        assert!(alone_result(&good, &["a", "b"]).is_ok());
        assert!(alone_result(&run("nlt ret 0 0\nnlt ret 1 -1\n", true), &["a", "b"]).is_err());
        assert!(alone_result(&run("nlt ret 0 0\n", false), &["a", "b"]).is_err());
    }

    /// The launcher's logger line for the INFO message (atl-app-launcher log.rs format).
    const INFO_LINE: &str =
        "1762000000.007 5.250    42    43 I liblog/nlt: native-load println_native\n";

    #[test]
    fn call_checks_fire_on_wrong_results() {
        // 5 ms between the two nanoTime reads; uptime inside, at the edges, outside
        let base = "nlt loggable-verbose false\nnlt loggable-info true\nnlt println 1\nnlt println-verbose -1\n";
        let mut ok = run(
            &format!("nlt uptime 1000000000 1002 1005000000\n{base}"),
            true,
        );
        ok.stderr.push_str(INFO_LINE);
        assert!(
            call_checks(&ok).iter().all(Result::is_ok),
            "{:?}",
            call_checks(&ok)
        );
        let edge = run(
            &format!("nlt uptime 1000999999 1000 1005000000\n{base}"),
            true,
        );
        assert!(call_checks(&edge)[0].is_ok());
        let late = run(
            &format!("nlt uptime 1000000000 1006 1005000000\n{base}"),
            true,
        );
        assert!(call_checks(&late)[0].is_err());
        let wrong = run(
            "nlt uptime 1 0 1\nnlt loggable-verbose true\nnlt loggable-info false\nnlt println 0\nnlt println-verbose 1\n",
            true,
        );
        assert!(call_checks(&wrong).iter().skip(1).all(Result::is_err));
        assert!(call_checks(&run("", false)).iter().all(Result::is_err));
    }

    #[test]
    fn logger_check_fires() {
        let mut r = run("", true);
        // no line from our logger (e.g. liblog's own stderr logger still installed)
        assert!(logger_result(&r).is_err());
        r.stderr.push_str(INFO_LINE);
        assert!(logger_result(&r).is_ok());
        // a VERBOSE line written: something filtered nothing
        r.stderr
            .push_str("1 2 3 4 V liblog/nlt: native-load verbose\n");
        assert!(logger_result(&r).is_err());
    }

    #[test]
    fn area_guard_check() {
        let mut refused = run("", false);
        refused.stderr =
            "atl-app-launcher: no system property area: neither $ATL_PROP_AREA nor ...\n"
                .to_owned();
        assert!(area_refused(&refused).is_ok());
        // started anyway, or failed for another reason
        assert!(area_refused(&run("nlt loaded /l.so\n", true)).is_err());
        assert!(area_refused(&run("", false)).is_err());
    }

    #[test]
    fn copies_by_library() {
        let maps: Vec<String> = [
            "/art/lib64/libbase.so",
            "/out/libbase/libbase.so",
            "/art/lib64/libicuuc-host.so",
            "/usr/lib/libicuuc.so.76.1",
            "/usr/lib/libc++.so.1.0",
            "/art/lib64/libc++.so",
            "/art/lib64/liblog.so",
            "/art/lib64/libbase_extra.so",
        ]
        .map(str::to_owned)
        .to_vec();
        let d = duplicates(&maps);
        assert_eq!(
            d.keys().copied().collect::<Vec<_>>(),
            ["libbase", "libc++", "libicuuc"]
        );
        assert!(duplicates(&maps[6..]).is_empty());
    }

    #[test]
    fn symbol_names() {
        // Itanium: <length><identifier>; "register_android_os_SystemClock" is 31 bytes
        assert_eq!(
            register_symbol("register_android_os_SystemClock"),
            "_ZN7android31register_android_os_SystemClockEP7_JNIEnv"
        );
        assert_eq!(
            demangle_register("_ZN7android31register_android_os_SystemClockEP7_JNIEnv"),
            "register_android_os_SystemClock"
        );
    }
}
