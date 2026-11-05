// SPDX-License-Identifier: MIT
//! `xtask mesa-config`: inputs for Mesa's own meson build with `platforms=android` (GPU slice G1), from one converter out dir.
//! Mesa's `meson.build` looks up AOSP libraries by pkg-config (`dependency('cutils')` & co., AOSP Mesa `meson.build` `if with_platform_android`); on Android Soong supplies them, here the converter built them. Writes `<dest>/pkgconfig/<name>.pc` per looked-up name, each pointing at the converter's `.so` and the include dirs the converter compiled that library with (graph.json `artifacts.<module>.cxx` `-I`), plus `<dest>/native.ini`, a meson native file carrying `pkg_config_path` and the decided project options. Mesa itself is run as-is (`meson setup --native-file <dest>/native.ini <build> <mesa>`); no Mesa source change.
//! Include dirs are the library's own compile dirs, a superset of its Android.bp `export_include_dirs` (private dirs included): graph.json does not record the exported subset. Narrow it when the converter emits exports.
//! Every option name is checked against the pinned Mesa's `meson_options.txt`, so a pin bump that renames an option fails here, not as a silent meson default.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use serde::Deserialize;

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

/// One meson option value, in machine-file syntax.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Opt {
    Str(&'static str),
    Int(i64),
    Bool(bool),
    Arr(&'static [&'static str]),
}

/// G1 option set (owner 2026-11-06, g1-prep O7/O8): softpipe only, no LLVM, Android EGL platform only.
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
    // O8: softpipe needs no LLVM; llvmpipe waits for G3
    ("gallium-drivers", Opt::Arr(&["softpipe"])),
    ("llvm", Opt::Str("disabled")),
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
    let options_file = mesa.join("meson_options.txt");
    let options = std::fs::read_to_string(&options_file)
        .with_context(|| format!("read {}", options_file.display()))?;
    check_options(&options)?;
    let graph_file = args.out.join("graph.json");
    let graph: Graph = serde_json::from_slice(
        &std::fs::read(&graph_file).with_context(|| format!("read {}", graph_file.display()))?,
    )
    .with_context(|| format!("parse {}", graph_file.display()))?;
    let pc_dir = args.dest.join("pkgconfig");
    std::fs::create_dir_all(&pc_dir)?;
    for (name, module) in PC_MODULES {
        let text = pc_file(name, module, &graph)?;
        std::fs::write(pc_dir.join(format!("{name}.pc")), text)?;
    }
    let forced = args.dest.join(FORCED_HEADER);
    std::fs::write(&forced, forced_header(&graph.device))?;
    std::fs::write(
        args.dest.join("native.ini"),
        native_file(&pc_dir, &forced, &graph)?,
    )?;
    println!(
        "mesa-config: {} .pc files + {FORCED_HEADER} + native.ini in {}",
        PC_MODULES.len(),
        args.dest.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Fails on an option name the pinned Mesa does not declare.
pub fn check_options(meson_options: &str) -> Result<()> {
    let missing: Vec<&str> = OPTIONS
        .iter()
        .map(|(n, _)| *n)
        .chain([SDK_OPTION])
        .filter(|n| !meson_options.contains(&format!("'{n}'")))
        .collect();
    ensure!(
        missing.is_empty(),
        "Mesa meson_options.txt lacks option(s) {missing:?}"
    );
    Ok(())
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
pub fn native_file(pc_dir: &Path, forced: &Path, graph: &Graph) -> Result<String> {
    let d = &graph.device;
    let (c, cxx) = (split_forced(&d.c).0, split_forced(&d.cxx).0);
    let inc = ["-include".to_owned(), forced.display().to_string()];
    let keep = |f: &&String| !f.starts_with("-std=");
    let mut s = String::from("# generated by xtask mesa-config\n[binaries]\n");
    let _ = writeln!(
        s,
        "c = '{}'\ncpp = '{}'\nc_ld = 'lld'\ncpp_ld = 'lld'\n\n[built-in options]",
        d.cc, graph.link_driver
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
    let _ = writeln!(
        s,
        "pkg_config_path = ['{}']\n\n[project options]",
        pc_dir.display()
    );
    let _ = writeln!(
        s,
        "{SDK_OPTION} = {}",
        value(Opt::Int(graph.platform_sdk()?))
    );
    for (n, o) in OPTIONS {
        let _ = writeln!(s, "{n} = {}", value(*o));
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
        let text = native_file(Path::new("/d"), Path::new("/d/f.h"), &g).unwrap();
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
        let text = native_file(Path::new("/d"), Path::new("/d/f.h"), &g).unwrap();
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
        let text = native_file(pc, Path::new("/d/f.h"), &g).unwrap();
        assert!(text.contains(&format!("pkg_config_path = ['{}']", pc.display())));
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
