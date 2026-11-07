// SPDX-License-Identifier: MIT
//! `xtask mesa-config`: inputs for Mesa's own meson build with `platforms=android` (GPU slice G1), from one converter out dir.
//! Mesa's `meson.build` looks up AOSP libraries by pkg-config (`dependency('cutils')` & co., AOSP Mesa `meson.build` `if with_platform_android`); on Android Soong supplies them, here the converter built them. Writes `<dest>/pkgconfig/<name>.pc` per looked-up name, each pointing at the converter's `.so` and the include dirs the converter compiled that library with (graph.json `artifacts.<module>.cxx` `-I`), the host libraries Mesa may find ([`HOST_PC`]), `<dest>/forced.h`, and `<dest>/native.ini`, a meson native file carrying the converter's device compilers and flags (graph.json `device`), `pkg_config_libdir` and the decided project options. Mesa itself is run by meson (`meson setup --native-file <dest>/native.ini <build> <mesa>`) from its checkout = pin + patches/aosp/external/mesa3d (`--mesa`: the task copy while one exists).
//! Include dirs are the library's own compile dirs, a superset of its Android.bp `export_include_dirs` (private dirs included): graph.json does not record the exported subset. Narrow it when the converter emits exports.
//! Every option name is checked against the pinned Mesa's `meson_options.txt`, so a pin bump that renames an option fails here, not as a silent meson default.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use serde::Deserialize;

use crate::{llvm_host, provided_gate};

/// Mesa's tree path in the AOSP checkout (sources.toml `[aosp.projects]`).
pub const MESA_TREE: &str = "external/mesa3d";

/// pkg-config names AOSP Mesa's `meson.build` asks for with `platforms=android`, `platform-sdk-version` >= 35, `android-libbacktrace` off, `android-stub` off -> converter module that provides it.
/// `backtrace` is skipped (only with `android-libbacktrace`); `android.hardware.graphics.mapper` is `required: false` and unused once `ui` is found.
pub const PC_MODULES: &[(&str, &str)] = &[
    ("cutils", "device/libcutils"),
    ("hardware", "device/libhardware"),
    ("log", "device/liblog"),
    ("sync", "device/libsync"),
    ("nativewindow", "device/libnativewindow"),
    ("ui", "device/libui"),
];

/// The native goals (module names) behind [`PC_MODULES`]: what a build needs for Mesa to be built at all.
pub fn pc_goals() -> Vec<&'static str> {
    PC_MODULES
        .iter()
        .map(|(_, dir)| dir.strip_prefix("device/").unwrap_or(dir))
        .collect()
}

/// Host (distro) libraries Mesa may find by pkg-config; their `.pc` files are copied next to [`PC_MODULES`]' and native.ini restricts pkg-config to that dir (`pkg_config_libdir`).
/// Mesa looks up libudev, SPIRV-Tools and libelf as `required: false` with no option to turn them off (owner 2026-11-06: not linked; SPIRV-Tools is a libstdc++ static archive, which breaks the libc++ link of libgallium_dri). Whatever the build host has installed, only these are found.
pub const HOST_PC: &[&str] = &[
    // disk cache and blob compression (`zlib` / `zstd` options)
    "zlib", "libzstd", // platform_android.c, u_gralloc: render node / dmabuf handling
    "libdrm",
];

/// One meson option value, in machine-file syntax.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Opt {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    Arr(&'static [&'static str]),
}

/// The gallium drivers built: [`DRIVER`] must export `__driDriverGetExtensions_<name>` for each ([`driver_links`]).
pub const GALLIUM_DRIVERS: &[&str] = &["llvmpipe", "iris"];

/// Option set (G1, owner 2026-11-06; llvmpipe plan S2): llvmpipe on the host LLVM (llvm_host.rs), Android EGL platform only.
pub const OPTIONS: &[(&str, Opt)] = &[
    // Android is the only platform meson accepts with `android` (R-B not buildable)
    ("platforms", Opt::Arr(&["android"])),
    // EGL_DEFAULT_DISPLAY means the Android platform
    ("egl-native-platform", Opt::Str("android")),
    // libbacktrace is not built; u_debug_stack_android.cpp includes it only with this on
    ("android-libbacktrace", Opt::Str("disabled")),
    ("android-strict", Opt::Bool(true)),
    // real AOSP libs, not Mesa's stub libraries
    ("android-stub", Opt::Bool(false)),
    // software tier llvmpipe JITs through the host LLVM (llvm_host.rs); hardware tier iris (Intel Gen8+). Mesa's loader picks the tier from the DRM node's kernel driver (loader_get_driver_for_fd), so one libgallium_dri holds both
    ("gallium-drivers", Opt::Arr(GALLIUM_DRIVERS)),
    // iris' internal shaders are compiled at build time by the host intel_clc (intel_clc.rs); `enabled` would build it with the device toolchain against LLVM+Clang+libclc
    ("intel-clc", Opt::Str("system")),
    ("llvm", Opt::Str("enabled")),
    // meson's default `auto` may pick a shared libLLVM; only the static archives are built and allowed (owner D2)
    ("shared-llvm", Opt::Str("disabled")),
    ("vulkan-drivers", Opt::Arr(&[])),
    ("glx", Opt::Str("disabled")),
    ("gbm", Opt::Str("disabled")),
    ("egl", Opt::Str("enabled")),
    // EGL library is libEGL itself, not a glvnd vendor library
    ("glvnd", Opt::Str("disabled")),
    // GLES1 kept for old native games
    ("gles1", Opt::Str("enabled")),
    ("gles2", Opt::Str("enabled")),
    ("valgrind", Opt::Str("disabled")),
    // meson.build: Android requires libbacktrace, not libunwind
    ("libunwind", Opt::Str("disabled")),
    // AOSP's Loader.cpp looks for lib{EGL,GLESv1_CM,GLESv2}_<ro.hardware.egl>.so in the driver dir; the suffix is Mesa's own way to produce those names
    ("egl-lib-suffix", Opt::Str("_mesa")),
    ("gles-lib-suffix", Opt::Str("_mesa")),
];

/// Mesa's option for the product's SDK version (`ANDROID_API_LEVEL`; >= 35 selects the libui IMapper5 path in u_gralloc). Its value is the converter's `platform_sdk_version` product variable (graph.json), the release the AOSP libraries Mesa links were built for, so the two cannot disagree.
pub const SDK_OPTION: &str = "platform-sdk-version";

/// Deliberate breaks of Mesa's configure (`native --break`): each must turn the native's Mesa step or the gl gate red.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Break {
    /// Mesa configured against distro LLVM 19 ([`DISTRO_LLVM_CONFIG`]): `configure` refuses its version, the last native.ini stays.
    #[value(name = "llvm-config-19")]
    LlvmConfig19,
    /// gallium-drivers with softpipe in place of llvmpipe: the Mesa step's driver-export check is red.
    #[value(name = "no-llvmpipe")]
    NoLlvmpipe,
    /// gallium-drivers without iris: the Mesa step's driver-export check is red (on Intel hardware also the gl gate's renderer).
    #[value(name = "no-iris")]
    NoIris,
}

/// The distro's llvm-config (Debian llvm-19) that [`Break::LlvmConfig19`] points Mesa at.
const DISTRO_LLVM_CONFIG: &str = "/usr/bin/llvm-config-19";

#[derive(clap::Args)]
pub struct Args {
    /// Converter out dir whose graph.json and `.so` files the `.pc` files point at (read only).
    #[arg(long)]
    out: PathBuf,
    /// Directory to write `pkgconfig/*.pc` and `native.ini` into.
    #[arg(long)]
    dest: PathBuf,
    /// Mesa checkout whose meson_options.txt the options are checked against (default: <workspace>/src/aosp/external/mesa3d).
    #[arg(long)]
    mesa: Option<PathBuf>,
    /// The host LLVM's `llvm-config` (llvm_host.rs `config`): Mesa's meson finds LLVM through it.
    #[arg(long)]
    llvm_config: PathBuf,
    /// The host `intel_clc` (intel_clc.rs) Mesa runs at build time for iris' shaders.
    #[arg(long)]
    intel_clc: PathBuf,
}

/// graph.json, the fields read here (bp2ninja graph.go).
#[derive(Deserialize)]
pub struct Graph {
    artifacts: BTreeMap<String, Artifact>,
    #[serde(default)]
    product_variables: BTreeMap<String, serde_json::Value>,
    /// The C++ compiler and link driver the converter builds with.
    link_driver: String,
    device: Platform,
}

/// graph.json `device`: the flags every converter device module gets from the toolchain and the libc provider (bp2ninja graph.go devicePlatform).
#[derive(Deserialize)]
pub struct Platform {
    cc: String,
    c: Vec<String>,
    cxx: Vec<String>,
    link: Vec<String>,
}

impl Graph {
    /// The product's SDK version the converter built for (bp2ninja productvars.go `platform_sdk_version`).
    pub fn platform_sdk(&self) -> Result<i64> {
        self.product_variables
            .get("platform_sdk_version")
            .and_then(serde_json::Value::as_i64)
            .context("graph.json has no product variable platform_sdk_version (converter without -release?)")
    }
}

#[derive(Deserialize)]
struct Artifact {
    kind: String,
    out: PathBuf,
    #[serde(default)]
    cxx: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let mesa = match &args.mesa {
        Some(m) => m.clone(),
        None => Layout::of(root)?.src().join("aosp").join(MESA_TREE),
    };
    configure(
        &read_graph(&args.out)?,
        &mesa,
        &args.dest,
        &args.llvm_config,
        &args.intel_clc,
        None,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Mesa's install dir in a converter out dir: [`build_after_ninja`] writes its config, build and outputs there.
pub const OUT_SUBDIR: &str = "mesa";

/// The Mesa libraries [`build_after_ninja`] builds, as Mesa's build dir names them (meson `-Degl-lib-suffix` & co.): the EGL / GLES fronts AOSP's loader opens by `ro.hardware.egl`, the dispatch table they share, the gallium driver.
/// The gallium driver library (build-relative): the output whose link facts [`driver_links`] checks.
pub const DRIVER: &str = "src/gallium/targets/dri/libgallium_dri.so";

pub const OUTPUTS: &[&str] = &[
    "src/egl/libEGL_mesa.so",
    "src/mapi/es1api/libGLESv1_CM_mesa.so",
    "src/mapi/es2api/libGLESv2_mesa.so",
    "src/mapi/shared-glapi/libglapi.so",
    DRIVER,
];

/// The Mesa driver's placement: (host file, Android path) pairs, and library dirs.
pub type Driver = (Vec<(PathBuf, String)>, Vec<PathBuf>);

/// The Mesa driver as the runtime root holds it: (host file, Android path) to place (the [`OUTPUTS`] fronts in the driver dir, a /vendor/build.prop with `ro.hardware.egl`), and the build dirs of what they link (`libglapi`, `libgallium_dri`) for the library path.
/// Their RUNPATH names those dirs by `$ORIGIN`, which a link placed in /vendor does not keep.
pub fn driver(out: &Path) -> Result<Driver> {
    let build = out.join(OUT_SUBDIR).join("build");
    let suffix = OPTIONS
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (&"egl-lib-suffix", Opt::Str(s)) => Some(*s),
            _ => None,
        })
        .context("mesa OPTIONS: no egl-lib-suffix")?;
    // Loader.cpp builds `lib<kind>_<ro.hardware.egl>.so`: the property is the suffix without its `_`
    let hardware = suffix
        .strip_prefix('_')
        .context("egl-lib-suffix without `_`")?;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for o in OUTPUTS {
        let path = build.join(o);
        ensure!(path.is_file(), "missing Mesa output {}", path.display());
        let (Some(name), Some(dir)) = (path.file_name(), path.parent()) else {
            bail!("Mesa output {o} has no file name");
        };
        let name = name.to_string_lossy().into_owned();
        let dir = dir.to_path_buf();
        if name.ends_with(&format!("{suffix}.so")) {
            files.push((path, format!("{}/{name}", atl_android_paths::VENDOR_EGL)));
        } else {
            dirs.push(dir);
        }
    }
    let prop = out.join(OUT_SUBDIR).join("vendor-build.prop");
    std::fs::write(&prop, format!("ro.hardware.egl={hardware}\n"))?;
    files.push((prop, atl_android_paths::VENDOR_BUILD_PROP.to_owned()));
    Ok((files, dirs))
}

pub fn read_graph(out: &Path) -> Result<Graph> {
    let graph_file = out.join("graph.json");
    serde_json::from_slice(
        &std::fs::read(&graph_file).with_context(|| format!("read {}", graph_file.display()))?,
    )
    .with_context(|| format!("parse {}", graph_file.display()))
}

/// `xtask native`'s step after ninja (run under the build token): Mesa's meson build against the converter out dir `out`, in `<out>/mesa/build` (set up once; meson reconfigures itself when native.ini changes).
/// Skipped with a printed reason when the goals did not build all of [`PC_MODULES`]' libraries; a missing host LLVM (`host`'s, llvm_host.rs) fails it instead. A failure carries the first error line (whole log: `<out>/mesa/ninja.log`).
/// `clc_llvm_config`: the distro llvm-config the host intel_clc builds against (deps.toml `llvm-dev`).
pub fn build_after_ninja(
    out: &Path,
    mesa: &Path,
    host: &str,
    clc_llvm_config: &str,
    brk: Option<Break>,
) -> Result<Step> {
    Ok(
        match build_or_skip(out, mesa, host, clc_llvm_config, brk)? {
            None => Step::Skipped,
            Some(Ok(build)) => Step::Built(build),
            Some(Err(first)) => Step::Failed(first),
        },
    )
}

/// libgallium_dri's link facts (plan llvmpipe S2): its DT_NEEDED list, and the failures: an LLVM or libstdc++ NEEDED, an exported LLVM symbol.
/// `libunwind.so.1` is listed, not failed: libc++ and libc++abi NEED it, the distro C++ runtime that stays (owner D1).
pub struct Links {
    pub needed: Vec<String>,
    pub failures: Vec<String>,
}

/// Static archives behind `dri.sym`'s `local: *` (owner D2): no LLVM symbol exported, no LLVM or libstdc++ NEEDED (libc++ only, D1).
pub fn driver_links(lib: &Path) -> Result<Links> {
    let needed = provided_gate::needed(lib)?;
    let mut failures: Vec<String> = needed
        .iter()
        // host-only LLVM/Clang NEEDED: app_links, run on every output
        .filter(|n| n.starts_with("libstdc++"))
        .map(|n| format!("NEEDED {n}"))
        .collect();
    let symbols = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(lib)
        .output()
        .context("running nm")?;
    ensure!(
        symbols.status.success(),
        "nm -D {}: {}",
        lib.display(),
        String::from_utf8_lossy(&symbols.stderr).trim()
    );
    // C++ mangles namespace llvm as `4llvm`; the LLVM C API is `LLVM*`
    let text = String::from_utf8_lossy(&symbols.stdout);
    let exported = text
        .lines()
        .filter(|l| l.contains("4llvm") || l.contains("LLVM"))
        .count();
    if exported > 0 {
        failures.push(format!("{exported} exported LLVM symbols"));
    }
    // a driver missing from the build (Break::NoIris / NoLlvmpipe) or dropped by a Mesa pin bump
    for d in GALLIUM_DRIVERS {
        let entry = format!("__driDriverGetExtensions_{d}");
        if !text
            .lines()
            .any(|l| l.split_whitespace().last() == Some(entry.as_str()))
        {
            failures.push(format!("{} exports no {entry}", lib.display()));
        }
    }
    Ok(Links { needed, failures })
}

/// Distro LLVM/Clang libraries that may run only in host tools (intel_clc.rs), never in an app process (owner D1).
const HOST_ONLY_LIBS: &[&str] = &["libLLVM", "libclang", "libLLVMSPIRVLib"];

/// The NEEDED entries of app-side output `lib` that name a [`HOST_ONLY_LIBS`] library.
pub fn app_links(lib: &Path) -> Result<Vec<String>> {
    Ok(host_only(&provided_gate::needed(lib)?)
        .into_iter()
        .map(|n| format!("{} NEEDS {n}", lib.display()))
        .collect())
}

fn host_only(needed: &[String]) -> Vec<String> {
    needed
        .iter()
        .filter(|n| HOST_ONLY_LIBS.iter().any(|p| n.starts_with(p)))
        .cloned()
        .collect()
}

/// What [`build_after_ninja`] did.
pub enum Step {
    Skipped,
    /// Mesa's build dir, holding every one of [`OUTPUTS`].
    Built(PathBuf),
    Failed(String),
}

fn build_or_skip(
    out: &Path,
    mesa: &Path,
    host: &str,
    clc_llvm_config: &str,
    brk: Option<Break>,
) -> Result<Option<Result<PathBuf, String>>> {
    let graph = read_graph(out)?;
    let missing: Vec<&str> = PC_MODULES
        .iter()
        .filter(|(_, m)| graph.artifacts.get(*m).is_none_or(|a| !a.out.exists()))
        .map(|(_, m)| *m)
        .collect();
    if !missing.is_empty() {
        println!("native: mesa: skipped, the goals built none of {missing:?} (PC_MODULES)");
        return Ok(None);
    }
    let llvm = llvm_host::config(out, host);
    if !llvm.is_file() {
        return Ok(Some(Err(format!(
            "host LLVM missing: {} (cargo xtask native --for llvm)",
            llvm.display()
        ))));
    }
    let dest = out.join(OUT_SUBDIR);
    let intel_clc = match crate::intel_clc::build(&dest, mesa, clc_llvm_config)? {
        Ok(p) => p,
        Err(first) => return Ok(Some(Err(first))),
    };
    if let Err(e) = configure(&graph, mesa, &dest, &llvm, &intel_clc, brk) {
        return Ok(Some(Err(format!("{e:#}"))));
    }
    let build = dest.join("build");
    let log = dest.join("ninja.log");
    // device-linked, no runpath (runpath_gate): the sanity exe and Mesa's build-time executables find their libraries only through the stage env
    let stage = crate::binder::lib_env(out)?;
    if !build.join("build.ninja").exists() {
        let status = Command::new("meson")
            .args(["setup", "--native-file"])
            .arg(dest.join("native.ini"))
            .arg(&build)
            .arg(mesa)
            .envs(stage.clone())
            .stdout(std::fs::File::create(dest.join("meson-setup.log"))?)
            .stderr(std::process::Stdio::from(
                std::fs::File::options()
                    .append(true)
                    .open(dest.join("meson-setup.log"))?,
            ))
            .status()
            .context("running meson")?;
        if !status.success() {
            return Ok(Some(Err(format!(
                "meson setup failed, see {}",
                dest.join("meson-setup.log").display()
            ))));
        }
    }
    let status = Command::new("ninja")
        .arg("-C")
        .arg(&build)
        .envs(stage)
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::process::Stdio::from(
            std::fs::File::options().append(true).open(&log)?,
        ))
        .status()
        .context("running ninja for Mesa")?;
    if !status.success() {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let first = text
            .lines()
            .find(|l| l.contains("error:") || l.starts_with("FAILED:"))
            .unwrap_or("ninja failed; its log names no error line");
        return Ok(Some(Err(format!("{first} (log {})", log.display()))));
    }
    let absent: Vec<&str> = OUTPUTS
        .iter()
        .copied()
        .filter(|o| !build.join(o).exists())
        .collect();
    if !absent.is_empty() {
        return Ok(Some(Err(format!(
            "Mesa's build produced none of {absent:?}"
        ))));
    }
    let mut failures = driver_links(&build.join(DRIVER))?.failures;
    for o in OUTPUTS {
        failures.extend(app_links(&build.join(o))?);
    }
    if !failures.is_empty() {
        return Ok(Some(Err(failures.join("; "))));
    }
    println!(
        "native: mesa: {} libraries in {}",
        OUTPUTS.len(),
        build.display()
    );
    Ok(Some(Ok(build)))
}

/// Writes `<dest>/pkgconfig/*.pc`, the forced header and native.ini for `graph`, checking the options against `mesa`'s meson_options.txt. A file is rewritten only when its text changed, so meson does not reconfigure on an unchanged input.
pub fn configure(
    graph: &Graph,
    mesa: &Path,
    dest: &Path,
    llvm_config: &Path,
    intel_clc: &Path,
    brk: Option<Break>,
) -> Result<()> {
    let options_file = mesa.join("meson_options.txt");
    let options = std::fs::read_to_string(&options_file)
        .with_context(|| format!("read {}", options_file.display()))?;
    check_options(&options)?;
    let clc_missing = missing_options(&options, crate::intel_clc::OPTIONS.iter().map(|(n, _)| *n));
    ensure!(
        clc_missing.is_empty(),
        "Mesa meson_options.txt lacks intel_clc option(s) {clc_missing:?}"
    );
    // before any file is written: a refused LLVM leaves the last native.ini in place
    let llvm = match brk {
        Some(Break::LlvmConfig19) => Path::new(DISTRO_LLVM_CONFIG),
        _ => llvm_config,
    };
    llvm_host::check_config(llvm)?;
    let rtti = llvm_host::has_rtti(llvm)?;
    let pc_dir = dest.join("pkgconfig");
    std::fs::create_dir_all(&pc_dir)?;
    for (name, module) in PC_MODULES {
        write_changed(
            &pc_dir.join(format!("{name}.pc")),
            &pc_file(name, module, graph)?,
        )?;
    }
    for name in HOST_PC {
        let out = Command::new("pkg-config")
            .args(["--variable=pcfiledir", name])
            .output()
            .context("running pkg-config")?;
        ensure!(
            out.status.success(),
            "pkg-config does not find host library `{name}` (HOST_PC)"
        );
        let dir = PathBuf::from(String::from_utf8(out.stdout)?.trim());
        let text = std::fs::read_to_string(dir.join(format!("{name}.pc")))?;
        write_changed(&pc_dir.join(format!("{name}.pc")), &text)?;
    }
    let forced = dest.join(FORCED_HEADER);
    write_changed(&forced, &forced_header(&graph.device))?;
    write_changed(
        &dest.join("native.ini"),
        &native_file(&pc_dir, &forced, graph, llvm, intel_clc, rtti, brk)?,
    )?;
    println!(
        "mesa-config: {} .pc files + {FORCED_HEADER} + native.ini in {}",
        PC_MODULES.len(),
        dest.display()
    );
    Ok(())
}

fn write_changed(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(text) {
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// Fails on an option name the pinned Mesa does not declare.
pub fn check_options(meson_options: &str) -> Result<()> {
    let missing = missing_options(
        meson_options,
        OPTIONS.iter().map(|(n, _)| *n).chain([SDK_OPTION]),
    );
    ensure!(
        missing.is_empty(),
        "Mesa meson_options.txt lacks option(s) {missing:?}"
    );
    Ok(())
}

/// The `names` meson_options.txt does not declare.
fn missing_options<'a>(meson_options: &str, names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    names
        .filter(|n| !meson_options.contains(&format!("'{n}'")))
        .collect()
}

/// The `.pc` text for pkg-config `name`, from converter module `module`'s artifact.
pub fn pc_file(name: &str, module: &str, graph: &Graph) -> Result<String> {
    let Some(a) = graph.artifacts.get(module) else {
        bail!("graph.json has no artifact `{module}` (pkg-config `{name}`)")
    };
    ensure!(a.kind == "shared", "`{module}` is `{}`, not shared", a.kind);
    let dir = a.out.parent().context("artifact path has no parent")?;
    let stem = a.out.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let Some(lib) = stem.strip_prefix("lib") else {
        bail!("`{}` is not lib<name>.so", a.out.display())
    };
    // the platform's dirs come from native.ini, per language: libui's C++ compile dirs hold the C++-only compat/cxx (its stdatomic.h needs libc++)
    let mut includes: Vec<&str> = Vec::new();
    for f in &a.cxx {
        if f.starts_with("-I") && !includes.contains(&f.as_str()) && !graph.device.cxx.contains(f) {
            includes.push(f);
        }
    }
    let mut s = format!("# generated by xtask mesa-config from converter module {module}\n");
    let _ = writeln!(
        s,
        "Name: {name}\nDescription: AOSP {module} (converter build)\nVersion: 0"
    );
    let _ = writeln!(s, "Cflags: {}", includes.join(" "));
    let d = dir.display();
    let _ = writeln!(s, "Libs: -L{d} -l{lib} -Wl,-rpath,{d}");
    Ok(s)
}

fn value(o: Opt) -> String {
    match o {
        Opt::Str(v) => format!("'{v}'"),
        Opt::Int(v) => v.to_string(),
        Opt::Bool(v) => v.to_string(),
        Opt::Arr(v) => format!(
            "[{}]",
            v.iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A meson string array of `flags`.
fn array<'a>(flags: impl Iterator<Item = &'a String>) -> String {
    let quoted: Vec<String> = flags
        .map(|f| format!("'{}'", f.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// The header standing for the platform's `-include` files in Mesa's compiles.
pub const FORCED_HEADER: &str = "forced.h";

/// The platform's `-include <h>` pairs removed from `flags`, and the `h`s in order.
fn split_forced(flags: &[String]) -> (Vec<&String>, Vec<&String>) {
    let (mut rest, mut forced) = (Vec::new(), Vec::new());
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        match (f.as_str(), it.clone().next()) {
            ("-include", Some(h)) => {
                forced.push(h);
                it.next();
            }
            _ => rest.push(f),
        }
    }
    (rest, forced)
}

/// [`FORCED_HEADER`]: the platform's forced includes (C and C++ force the same ones), skipped in assembly: meson passes c_args to `.S` compiles too, and the headers are C (Mesa's blake3 `.S`); the converter gives assembly no forced include.
pub fn forced_header(d: &Platform) -> String {
    let mut s = String::from(
        "// generated by xtask mesa-config: the converter's device forced includes\n#ifndef __ASSEMBLER__\n",
    );
    for h in split_forced(&d.c).1 {
        let _ = writeln!(s, "#include <{h}>");
    }
    s.push_str("#endif\n");
    s
}

/// Meson native file: the converter's device compilers and platform flags (Mesa compiles and links as a converter device module does: same libc provider headers and library, same libc++), pkg-config search path, [`OPTIONS`] + [`SDK_OPTION`] = the graph's SDK.
/// `-std=` is dropped: Mesa sets its own (`default_options` c_std / cpp_std). The `-include`s become one of `forced` ([`forced_header`]).
pub fn native_file(
    pc_dir: &Path,
    forced: &Path,
    graph: &Graph,
    llvm_config: &Path,
    intel_clc: &Path,
    rtti: bool,
    brk: Option<Break>,
) -> Result<String> {
    let d = &graph.device;
    let (c, cxx) = (split_forced(&d.c).0, split_forced(&d.cxx).0);
    let inc = ["-include".to_owned(), forced.display().to_string()];
    let keep = |f: &&String| !f.starts_with("-std=");
    let mut s = String::from("# generated by xtask mesa-config\n[binaries]\n");
    let _ = writeln!(
        s,
        "c = '{}'\ncpp = '{}'\nc_ld = 'lld'\ncpp_ld = 'lld'\nllvm-config = '{}'\nintel_clc = '{}'\n\n[built-in options]",
        d.cc,
        graph.link_driver,
        llvm_config.display(),
        intel_clc.display()
    );
    let _ = writeln!(
        s,
        "c_args = {}",
        array(c.into_iter().filter(keep).chain(&inc))
    );
    let _ = writeln!(
        s,
        "cpp_args = {}",
        array(cxx.into_iter().filter(keep).chain(&inc))
    );
    let _ = writeln!(s, "c_link_args = {}", array(d.link.iter()));
    let _ = writeln!(s, "cpp_link_args = {}", array(d.link.iter()));
    // meson refuses a Mesa whose cpp_rtti differs from the LLVM's ("LLVM was built without RTTI"); the device flags already carry -fno-rtti
    let _ = writeln!(s, "cpp_rtti = {rtti}");
    // libdir, not path: pkg-config then searches only this dir, never the host's defaults (HOST_PC)
    let _ = writeln!(
        s,
        "\n[properties]\npkg_config_libdir = ['{}']\n\n[project options]",
        pc_dir.display()
    );
    let _ = writeln!(
        s,
        "{SDK_OPTION} = {}",
        value(Opt::Int(graph.platform_sdk()?))
    );
    for (n, o) in OPTIONS {
        let o = match (brk, *n) {
            (Some(Break::NoLlvmpipe), "gallium-drivers") => Opt::Arr(&["softpipe", "iris"]),
            (Some(Break::NoIris), "gallium-drivers") => Opt::Arr(&["llvmpipe"]),
            _ => *o,
        };
        let _ = writeln!(s, "{n} = {}", value(o));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        let mut artifacts = BTreeMap::new();
        for (_, m) in PC_MODULES {
            let lib = m.rsplit('/').next().unwrap();
            artifacts.insert(
                (*m).to_owned(),
                Artifact {
                    kind: "shared".into(),
                    out: PathBuf::from(format!("/o/{m}/{lib}.so")),
                    cxx: vec![
                        "-isystem".into(),
                        "/libcxx".into(),
                        format!("-I/t/{lib}/include"),
                        "-DANDROID".into(),
                        format!("-I/t/{lib}/include"),
                        // a platform dir: native.ini's, not the .pc's
                        "-I/compat/cxx".into(),
                    ],
                },
            );
        }
        Graph {
            artifacts,
            product_variables: BTreeMap::new(),
            link_driver: "clang++-x".into(),
            device: Platform {
                cc: "clang-x".into(),
                c: [
                    "-std=gnu11",
                    "-D__ANDROID__",
                    "-include",
                    "a/v.h",
                    "-include",
                    "a/l.h",
                ]
                .map(String::from)
                .to_vec(),
                cxx: [
                    "-nostdinc++",
                    "-std=gnu++20",
                    "-D'q'",
                    "-include",
                    "a/v.h",
                    "-include",
                    "a/l.h",
                    "-I/compat/cxx",
                ]
                .map(String::from)
                .to_vec(),
                link: vec!["/c/libc.so".into(), "-lc++".into()],
            },
        }
    }

    /// The LLVM the test native files point at.
    const LLVM: &str = "/l/llvm-config";

    fn native_text(g: &Graph) -> String {
        native_file(
            Path::new("/d"),
            Path::new("/d/f.h"),
            g,
            Path::new(LLVM),
            Path::new(CLC),
            false,
            None,
        )
        .unwrap()
    }

    /// The intel_clc the test native files point at.
    const CLC: &str = "/c/intel_clc";

    #[test]
    fn forced_includes_skip_assembly() {
        assert_eq!(
            forced_header(&graph().device),
            "// generated by xtask mesa-config: the converter's device forced includes\n#ifndef __ASSEMBLER__\n#include <a/v.h>\n#include <a/l.h>\n#endif\n"
        );
    }

    #[test]
    fn native_file_carries_the_device_platform() {
        let mut g = graph();
        g.product_variables
            .insert("platform_sdk_version".into(), 1.into());
        let text = native_text(&g);
        for want in [
            "c = 'clang-x'\n",
            "cpp = 'clang++-x'\n",
            // -std dropped: Mesa's own default_options decide; -include pairs -> the one forced header
            "c_args = ['-D__ANDROID__', '-include', '/d/f.h']\n",
            "cpp_args = ['-nostdinc++', '-D\\'q\\'', '-I/compat/cxx', '-include', '/d/f.h']\n",
            "c_link_args = ['/c/libc.so', '-lc++']\n",
            "cpp_link_args = ['/c/libc.so', '-lc++']\n",
        ] {
            assert!(text.contains(want), "{want:?} not in {text}");
        }
    }

    #[test]
    fn sdk_comes_from_the_converter() {
        let mut g = graph();
        assert!(g.platform_sdk().is_err());
        // any value: the option must carry what graph.json says, not a constant of ours
        let sdk = 41;
        g.product_variables
            .insert("platform_sdk_version".into(), sdk.into());
        assert_eq!(g.platform_sdk().unwrap(), sdk);
        let text = native_text(&g);
        assert!(text.contains(&format!("{SDK_OPTION} = {sdk}\n")));
    }

    /// `key: value` field of a .pc text.
    fn field<'a>(text: &'a str, key: &str) -> &'a str {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}: ")))
            .unwrap_or_else(|| panic!("no {key} in {text}"))
    }

    #[test]
    fn pc_fields_come_from_the_artifact() {
        let g = graph();
        for (name, module) in PC_MODULES {
            let a = &g.artifacts[*module];
            let text = pc_file(name, module, &g).unwrap();
            let dir = a.out.parent().unwrap().display().to_string();
            let lib = a.out.file_stem().unwrap().to_str().unwrap();
            assert_eq!(field(&text, "Name"), *name);
            assert_eq!(
                field(&text, "Libs"),
                format!("-L{dir} -l{} -Wl,-rpath,{dir}", &lib[3..])
            );
            // only -I flags, once each, in compile order
            let want: Vec<&String> = a
                .cxx
                .iter()
                .filter(|f| f.starts_with("-I"))
                .take(1)
                .collect();
            assert_eq!(field(&text, "Cflags"), want[0].as_str());
        }
    }

    #[test]
    fn pc_refuses_missing_or_static() {
        let mut g = graph();
        g.artifacts.remove("device/libui");
        assert!(pc_file("ui", "device/libui", &g).is_err());
        g.artifacts.get_mut("device/liblog").unwrap().kind = "static".into();
        assert!(pc_file("log", "device/liblog", &g).is_err());
    }

    #[test]
    fn native_file_carries_every_option() {
        let pc = Path::new("/d/pkgconfig");
        let mut g = graph();
        g.product_variables
            .insert("platform_sdk_version".into(), 1.into());
        let text = native_file(
            pc,
            Path::new("/d/f.h"),
            &g,
            Path::new(LLVM),
            Path::new(CLC),
            false,
            None,
        )
        .unwrap();
        assert!(text.contains(&format!(
            "[properties]\npkg_config_libdir = ['{}']\n",
            pc.display()
        )));
        for (n, o) in OPTIONS {
            let line = text
                .lines()
                .find(|l| l.starts_with(&format!("{n} = ")))
                .unwrap();
            assert_eq!(line, format!("{n} = {}", value(*o)));
        }
        assert_eq!(value(Opt::Arr(&[])), "[]");
        assert_eq!(value(Opt::Arr(&["a", "b"])), "['a', 'b']");
    }

    #[test]
    fn native_file_links_the_host_llvm_and_breaks_gallium() {
        let mut g = graph();
        g.product_variables
            .insert("platform_sdk_version".into(), 1.into());
        let text = native_text(&g);
        assert!(text.contains(&format!("llvm-config = '{LLVM}'\n")));
        assert!(text.contains(&format!("intel_clc = '{CLC}'\n")));
        assert!(text.contains("gallium-drivers = ['llvmpipe', 'iris']\n"));
        assert!(text.contains("intel-clc = 'system'\n"));
        assert!(text.contains("shared-llvm = 'disabled'\n"));
        assert!(text.contains("cpp_rtti = false\n"));
        let broken = |b| {
            native_file(
                Path::new("/d"),
                Path::new("/d/f.h"),
                &g,
                Path::new(LLVM),
                Path::new(CLC),
                false,
                Some(b),
            )
            .unwrap()
        };
        let no_lp = broken(Break::NoLlvmpipe);
        assert!(no_lp.contains("gallium-drivers = ['softpipe', 'iris']\n"));
        assert!(!no_lp.contains("'llvmpipe'"));
        let no_iris = broken(Break::NoIris);
        assert!(no_iris.contains("gallium-drivers = ['llvmpipe']\n"));
        assert!(!no_iris.contains("'iris'"));
    }

    #[test]
    fn host_only_libs_are_named() {
        let needed: Vec<String> = [
            "libc.so",
            "libLLVM.so.19.1",
            "libclang-cpp.so.19.1",
            "libLLVMSPIRVLib.so.19.1",
            "libdrm.so.2",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            host_only(&needed),
            [
                "libLLVM.so.19.1",
                "libclang-cpp.so.19.1",
                "libLLVMSPIRVLib.so.19.1"
            ]
        );
    }

    #[test]
    fn options_checked_against_meson_options() {
        let all: String = OPTIONS
            .iter()
            .map(|(n, _)| *n)
            .chain([SDK_OPTION])
            .map(|n| format!("option(\n  '{n}',\n)\n"))
            .collect();
        check_options(&all).unwrap();
        assert!(check_options(&all.replace(SDK_OPTION, "x")).is_err());
        let without = all.replace("'llvm'", "'llvm-x'");
        assert!(
            check_options(&without)
                .unwrap_err()
                .to_string()
                .contains("llvm")
        );
    }
}
