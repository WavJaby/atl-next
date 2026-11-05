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
    // android-16 = SDK 36; >= 35 selects the libui IMapper5 path in u_gralloc
    ("platform-sdk-version", Opt::Int(36)),
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
];

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
    std::fs::write(args.dest.join("native.ini"), native_file(&pc_dir))?;
    println!(
        "mesa-config: {} .pc files + native.ini in {}",
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
    let mut includes: Vec<&str> = Vec::new();
    for f in &a.cxx {
        if f.starts_with("-I") && !includes.contains(&f.as_str()) {
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

/// Meson native file: pkg-config search path + [`OPTIONS`].
pub fn native_file(pc_dir: &Path) -> String {
    let mut s = String::from("# generated by xtask mesa-config\n[built-in options]\n");
    let _ = writeln!(
        s,
        "pkg_config_path = ['{}']\n\n[project options]",
        pc_dir.display()
    );
    for (n, o) in OPTIONS {
        let _ = writeln!(s, "{n} = {}", value(*o));
    }
    s
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
                    ],
                },
            );
        }
        Graph { artifacts }
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
        let text = native_file(pc);
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
            .map(|(n, _)| format!("option(\n  '{n}',\n)\n"))
            .collect();
        check_options(&all).unwrap();
        let without = all.replace("'llvm'", "'llvm-x'");
        assert!(
            check_options(&without)
                .unwrap_err()
                .to_string()
                .contains("llvm")
        );
    }
}
