// SPDX-License-Identifier: MIT
//! Build scripts of packages `xtask native` builds after the Blueprint converter: a workspace package naming converter modules in its Cargo.toml (`[package.metadata.atl] converter = ["device/libbinder_rs"]`, graph.json module directories `<side>/<name>`) gets them linked when its feature `converter` is on.
//! What is linked comes from `$ATL_NATIVE_OUT/graph.json` `artifacts` (tools/bp2ninja graph.go), so it is what the converter's own links take: an rlib as a crate (`extern crate <name>;` finds it on the `crate=` search path, its crate closure on `dependency=`) plus its cc libraries as link-driver arguments; a static library as its archive plus the arguments a final link of it takes beyond it; a shared library as `-l` + RUNPATH, after the provided shared libraries it names (`needs`) are searched.
//! `rustc-link-lib` lands before std's `-lc`, so a shared module leads DT_NEEDED in the order named (libsigchain must precede libc); rlib link arguments are `rustc-link-arg`, which reach only this package's own links (a dependent package names the modules itself).
//! The build around it is `xtask native`'s: cargo's linker is the converter's link driver (graph.json `link_driver`; rlib arguments are clang driver arguments such as `-stdlib=libc++`) and the profile's `panic` is `abort`, as the converter's device crates are compiled (a panic strategy mismatch fails the link).
//! `[package.metadata.atl.aidl]` adds the package's AIDL crate `device/<package>-stubs-rust` (crate `<package>_stubs`), which the converter generates from those types' import closure (xtask packages.rs, bp2ninja aidlcrates.go).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

/// The environment variable naming the converter's build directory; `xtask native` sets it.
pub const NATIVE_OUT: &str = "ATL_NATIVE_OUT";
/// The Cargo feature under which a package links converter modules; `xtask native` turns it on, plain workspace builds and tests leave it off.
pub const FEATURE: &str = "converter";

/// The kinds of graph.json artifacts (bp2ninja `Kind.String`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Rlib,
    Shared,
    /// a cc_library_static's archive: `link` is what a final link takes beyond it
    Static,
    Binary,
    /// a cc_object's object file (xtask native's runtimes pass 2 links crtbegin_so)
    Object,
}

/// One graph.json artifact (bp2ninja graph.go `artifact`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: Kind,
    pub out: PathBuf,
    /// rlib: its crate name
    #[serde(rename = "crate", default)]
    pub crate_name: Option<String>,
    /// rlib: the dirs of its crate closure (rustc `-L dependency=`)
    #[serde(default)]
    pub search: Vec<PathBuf>,
    /// rlib, static: link-driver arguments (static: what a final link takes beyond its archive)
    #[serde(default)]
    pub link: Vec<String>,
    /// rlib: those libraries' dirs, where rustc finds its crates' `#[link(name = ...)]` (rustc `-L native=`)
    #[serde(default)]
    pub native: Vec<PathBuf>,
    /// shared: the C++ argv of the module's own C++ compiles (libc++, device defines, include paths); a package compiling a C++ wrapper against the module takes it
    #[serde(default)]
    pub cxx: Vec<String>,
    /// shared: the provided shared libraries its link names, by module (`device/libcrypto`); outside `out`, so the stage holds no copy and `link_lines` searches their dirs
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Deserialize)]
struct GraphFile {
    artifacts: BTreeMap<String, Artifact>,
    link_driver: String,
}

/// The converter's build directory as graph.json describes it.
#[derive(Debug)]
pub struct Converter {
    graph: PathBuf,
    artifacts: BTreeMap<String, Artifact>,
    /// The C++ compiler and link driver every converter link runs (graph.json `link_driver`, e.g. `clang++-19`); a C++ unit built against a module compiles with it.
    pub link_driver: String,
}

impl Converter {
    /// The build directory `$ATL_NATIVE_OUT` names.
    pub fn from_env() -> Result<Self> {
        let out = std::env::var_os(NATIVE_OUT).with_context(|| {
            format!("feature `{FEATURE}` needs ${NATIVE_OUT}: the package is built by `cargo xtask native`")
        })?;
        Self::load(Path::new(&out))
    }

    /// The build directory `out`.
    pub fn load(out: &Path) -> Result<Self> {
        let graph = out.join("graph.json");
        let text = std::fs::read_to_string(&graph).with_context(|| {
            format!(
                "reading {} (run `cargo xtask native` first)",
                graph.display()
            )
        })?;
        let file: GraphFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", graph.display()))?;
        Ok(Converter {
            graph,
            artifacts: file.artifacts,
            link_driver: file.link_driver,
        })
    }

    /// The artifact of `module` (`device/libbinder_rs`).
    pub fn artifact(&self, module: &str) -> Result<&Artifact> {
        self.artifacts.get(module).with_context(|| {
            format!(
                "{module}: no such artifact in {} (a module the build's goals do not reach, or not a Rust library, static or shared library, or binary)",
                self.graph.display()
            )
        })
    }

    /// The `cargo::` lines linking `modules`, in order, and rerunning when graph.json or an artifact changes.
    pub fn link_lines(&self, modules: &[String]) -> Result<Vec<String>> {
        let mut lines = vec![format!("cargo::rerun-if-changed={}", self.graph.display())];
        let mut search = BTreeSet::new();
        let mut native = BTreeSet::new();
        for m in modules {
            let a = self.artifact(m)?;
            let dir = a
                .out
                .parent()
                .with_context(|| format!("{m}: output without a directory"))?;
            lines.push(format!("cargo::rerun-if-changed={}", a.out.display()));
            match a.kind {
                Kind::Rlib => {
                    a.crate_name
                        .as_ref()
                        .with_context(|| format!("{m}: rlib without a crate name"))?;
                    lines.push(format!("cargo::rustc-link-search=crate={}", dir.display()));
                    for s in &a.search {
                        if search.insert(s.clone()) {
                            lines.push(format!(
                                "cargo::rustc-link-search=dependency={}",
                                s.display()
                            ));
                        }
                    }
                    push_link_args(&mut lines, &a.link);
                    for d in &a.native {
                        if native.insert(d.clone()) {
                            lines.push(format!("cargo::rustc-link-search=native={}", d.display()));
                        }
                    }
                }
                Kind::Shared => {
                    let name =
                        shared_name(&a.out).with_context(|| format!("{m}: {}", a.out.display()))?;
                    lines.push(format!("cargo::rustc-link-search=native={}", dir.display()));
                    for need in &a.needs {
                        let p = self
                            .artifact(need)
                            .with_context(|| format!("{m}: needs {need}"))?;
                        ensure!(
                            p.kind == Kind::Shared,
                            "{m}: needs {need}, which is not a shared library"
                        );
                        let d = p
                            .out
                            .parent()
                            .with_context(|| format!("{need}: output without a directory"))?;
                        lines.push(format!("cargo::rerun-if-changed={}", p.out.display()));
                        if native.insert(d.to_path_buf()) {
                            lines.push(format!("cargo::rustc-link-search=native={}", d.display()));
                        }
                    }
                    lines.push(format!("cargo::rustc-link-lib=dylib={name}"));
                    // RUNPATH, not RPATH: LD_LIBRARY_PATH still overrides it. Unlike converter device links, which carry none (xtask runpath_gate).
                    lines.push(format!(
                        "cargo::rustc-link-arg=-Wl,--enable-new-dtags,-rpath,{}",
                        dir.display()
                    ));
                }
                Kind::Static => {
                    lines.push(format!("cargo::rustc-link-arg={}", a.out.display()));
                    push_link_args(&mut lines, &a.link);
                }
                Kind::Binary => {
                    bail!("{m}: a binary links into nothing")
                }
                Kind::Object => {
                    bail!("{m}: a crt object is the guest toolchain's, not a cargo package's")
                }
            }
        }
        Ok(lines)
    }
}

/// A link's cc arguments: an archive relinks when it changes (its members are copied in); shared objects are looked up at run time.
fn push_link_args(lines: &mut Vec<String>, link: &[String]) {
    for arg in link {
        if arg.ends_with(".a") && Path::new(arg).is_absolute() {
            lines.push(format!("cargo::rerun-if-changed={arg}"));
        }
        lines.push(format!("cargo::rustc-link-arg={arg}"));
    }
}

/// `name` of `lib<name>.so`, the only shape `-l` finds.
fn shared_name(out: &Path) -> Result<&str> {
    out.file_name()
        .and_then(|f| f.to_str())
        .and_then(|f| f.strip_prefix("lib"))
        .and_then(|f| f.strip_suffix(".so"))
        .filter(|n| !n.is_empty())
        .context("not lib<name>.so: `-l` cannot name it")
}

/// The suffix of a package's AIDL crate interface (xtask packages.rs `AIDL_SUFFIX`).
const AIDL_SUFFIX: &str = "-stubs";

/// The converter modules a package's Cargo.toml text names: `[package.metadata.atl] converter`, then, when it has `aidl`, its AIDL crate's rlib `device/<package>-stubs-rust` (xtask packages.rs: the converter builds it; the table is checked there).
pub fn package_modules(cargo_toml: &str) -> Result<Vec<String>> {
    let table: toml::Table = toml::from_str(cargo_toml).context("parsing Cargo.toml")?;
    let atl = table
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("atl"));
    let list = atl.and_then(|a| a.get(FEATURE)).context(
        "no [package.metadata.atl] converter list: the package names no converter module",
    )?;
    let mut modules: Vec<String> = list
        .clone()
        .try_into()
        .context("[package.metadata.atl] converter: a list of strings")?;
    ensure!(
        !modules.is_empty(),
        "[package.metadata.atl] converter: empty"
    );
    for m in &modules {
        ensure!(
            m.split_once('/')
                .is_some_and(|(side, name)| (side == "device" || side == "host")
                    && !name.is_empty()
                    && !name.contains('/')),
            "[package.metadata.atl] converter: `{m}` is not a module directory <device|host>/<name>"
        );
    }
    if atl.and_then(|a| a.get("aidl")).is_some() {
        let name = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .context("[package] name: a string")?;
        modules.push(format!("device/{name}{AIDL_SUFFIX}-rust"));
    }
    Ok(modules)
}

/// The whole build-script step of a package with feature `converter`: reads `$ATL_NATIVE_OUT` and the package's Cargo.toml; returns the `cargo::` lines to print.
pub fn link_package(manifest_dir: &Path) -> Result<Vec<String>> {
    let manifest = manifest_dir.join("Cargo.toml");
    let modules = package_modules(
        &std::fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?,
    )
    .with_context(|| manifest.display().to_string())?;
    let conv = Converter::from_env()?;
    let mut lines = vec![format!("cargo::rerun-if-env-changed={NATIVE_OUT}")];
    lines.extend(conv.link_lines(&modules)?);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A graph.json as bp2ninja writes one (graph_test.go TestGraphJSONArtifacts, TestGraphJSONStaticAndProvided shapes): an rlib with a cc closure, a shared library, a binary, a static archive, a provided shared library, and a shared library naming it in `needs`.
    fn fixture_json() -> serde_json::Value {
        serde_json::json!({
            "goals": ["device/libgoal"],
            "deps": {},
            "dropped": {},
            "none_exports": {},
            "link_driver": "clang++-19",
            "artifacts": {
                "device/libbinder_rs": {
                    "kind": "rlib", "out": "/o/device/libbinder_rs/libbinder.rlib", "crate": "binder",
                    "search": ["/o/device/libbinder_rs", "/o/device/libbinder_ndk_sys", "/o/host/libzerocopy_derive"],
                    "link": ["-Wl,--start-group", "/o/device/libcc/libcc.a", "-Wl,--end-group", "/o/device/libbinder_ndk/libbinder_ndk.so", "-Wl,-rpath,/o/device/libbinder_ndk"],
                    "native": ["/o/device/libcc", "/o/device/libbinder_ndk"]
                },
                "device/libdep": {
                    "kind": "rlib", "out": "/o/device/libdep/libdep.rlib", "crate": "dep",
                    "search": ["/o/device/libdep", "/o/device/libbinder_ndk_sys"]
                },
                "device/libsigchain": { "kind": "shared", "out": "/o/device/libsigchain/libsigchain.so" },
                "device/odd": { "kind": "shared", "out": "/o/device/odd/odd.so" },
                "host/aidl": { "kind": "binary", "out": "/o/host/aidl/aidl" },
                "device/libaudioprocessing_base": {
                    "kind": "static", "out": "/o/device/libaudioprocessing_base/libaudioprocessing_base.a",
                    "link": ["-Wl,--start-group", "/o/device/libsub/libsub.a", "/upstream/boringssl/prefix/lib/libcrypto.so", "-Wl,--end-group"]
                },
                "device/libcrypto": { "kind": "shared", "out": "/upstream/boringssl/prefix/lib/libcrypto.so" },
                "device/libmid": { "kind": "shared", "out": "/o/device/libmid/libmid.so", "needs": ["device/libcrypto"] }
            }
        })
    }

    fn load_json(dir: &Path, json: &serde_json::Value) -> Converter {
        std::fs::write(dir.join("graph.json"), json.to_string()).unwrap();
        Converter::load(dir).unwrap()
    }

    fn fixture(dir: &Path) -> Converter {
        load_json(dir, &fixture_json())
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "atl-bridgegen-converter-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The lines follow the module kinds: rlib -> crate search + its closure's dirs once + its link args verbatim (archives rerun); shared -> -L native, -l, RUNPATH; order as named.
    #[test]
    fn link_lines_by_kind() {
        let dir = tmp("lines");
        let conv = fixture(&dir);
        let lines = conv
            .link_lines(&[
                "device/libsigchain".into(),
                "device/libbinder_rs".into(),
                "device/libdep".into(),
            ])
            .unwrap();
        let graph = dir.join("graph.json");
        let want = [
            format!("cargo::rerun-if-changed={}", graph.display()),
            "cargo::rerun-if-changed=/o/device/libsigchain/libsigchain.so".into(),
            "cargo::rustc-link-search=native=/o/device/libsigchain".into(),
            "cargo::rustc-link-lib=dylib=sigchain".into(),
            "cargo::rustc-link-arg=-Wl,--enable-new-dtags,-rpath,/o/device/libsigchain".into(),
            "cargo::rerun-if-changed=/o/device/libbinder_rs/libbinder.rlib".into(),
            "cargo::rustc-link-search=crate=/o/device/libbinder_rs".into(),
            "cargo::rustc-link-search=dependency=/o/device/libbinder_rs".into(),
            "cargo::rustc-link-search=dependency=/o/device/libbinder_ndk_sys".into(),
            "cargo::rustc-link-search=dependency=/o/host/libzerocopy_derive".into(),
            "cargo::rustc-link-arg=-Wl,--start-group".into(),
            "cargo::rerun-if-changed=/o/device/libcc/libcc.a".into(),
            "cargo::rustc-link-arg=/o/device/libcc/libcc.a".into(),
            "cargo::rustc-link-arg=-Wl,--end-group".into(),
            "cargo::rustc-link-arg=/o/device/libbinder_ndk/libbinder_ndk.so".into(),
            "cargo::rustc-link-arg=-Wl,-rpath,/o/device/libbinder_ndk".into(),
            "cargo::rustc-link-search=native=/o/device/libcc".into(),
            "cargo::rustc-link-search=native=/o/device/libbinder_ndk".into(),
            // libdep's closure dir libbinder_ndk_sys was already searched
            "cargo::rerun-if-changed=/o/device/libdep/libdep.rlib".into(),
            "cargo::rustc-link-search=crate=/o/device/libdep".into(),
            "cargo::rustc-link-search=dependency=/o/device/libdep".into(),
        ];
        assert_eq!(lines, want);
    }

    /// Every refusal fires: a module the graph lacks, a binary named as a link input, a shared object `-l` cannot name, a tool that is no binary.
    #[test]
    fn link_refusals() {
        let dir = tmp("refuse");
        let conv = fixture(&dir);
        let err = |m: &str| format!("{:#}", conv.link_lines(&[m.into()]).unwrap_err());
        assert!(err("device/libnone").contains("no such artifact"));
        assert!(err("host/aidl").contains("links into nothing"));
        assert!(err("device/odd").contains("not lib<name>.so"));
        assert!(
            format!("{:#}", Converter::load(&dir.join("nowhere")).unwrap_err())
                .contains("cargo xtask native")
        );
    }

    /// A static artifact is its archive, then what its final link takes (archives relink on change); a shared artifact searches the provided libraries it needs before its own `-l`, then its RUNPATH.
    #[test]
    fn link_lines_static_and_needs() {
        let dir = tmp("static");
        let conv = fixture(&dir);
        let lines = conv
            .link_lines(&[
                "device/libaudioprocessing_base".into(),
                "device/libmid".into(),
            ])
            .unwrap();
        let graph = dir.join("graph.json");
        let archive = "/o/device/libaudioprocessing_base/libaudioprocessing_base.a";
        let crypto = "/upstream/boringssl/prefix/lib/libcrypto.so";
        let want = [
            format!("cargo::rerun-if-changed={}", graph.display()),
            format!("cargo::rerun-if-changed={archive}"),
            format!("cargo::rustc-link-arg={archive}"),
            "cargo::rustc-link-arg=-Wl,--start-group".into(),
            "cargo::rerun-if-changed=/o/device/libsub/libsub.a".into(),
            "cargo::rustc-link-arg=/o/device/libsub/libsub.a".into(),
            format!("cargo::rustc-link-arg={crypto}"),
            "cargo::rustc-link-arg=-Wl,--end-group".into(),
            "cargo::rerun-if-changed=/o/device/libmid/libmid.so".into(),
            "cargo::rustc-link-search=native=/o/device/libmid".into(),
            format!("cargo::rerun-if-changed={crypto}"),
            "cargo::rustc-link-search=native=/upstream/boringssl/prefix/lib".into(),
            "cargo::rustc-link-lib=dylib=mid".into(),
            "cargo::rustc-link-arg=-Wl,--enable-new-dtags,-rpath,/o/device/libmid".into(),
        ];
        assert_eq!(lines, want);
    }

    /// A provided library a shared module needs must be a shared artifact of the graph: a missing one, or an archive, fails the link, named after the module that needs it.
    #[test]
    fn needs_refusals() {
        // the switchable break: the provided artifact dropped from graph.json
        let mut json = fixture_json();
        json["artifacts"]
            .as_object_mut()
            .unwrap()
            .remove("device/libcrypto");
        let conv = load_json(&tmp("needs-missing"), &json);
        let err = format!(
            "{:#}",
            conv.link_lines(&["device/libmid".into()]).unwrap_err()
        );
        assert!(
            err.contains("device/libmid: needs device/libcrypto")
                && err.contains("device/libcrypto: no such artifact"),
            "{err}"
        );
        let mut json = fixture_json();
        json["artifacts"]["device/libmid"]["needs"] =
            serde_json::json!(["device/libaudioprocessing_base"]);
        let conv = load_json(&tmp("needs-static"), &json);
        let err = format!(
            "{:#}",
            conv.link_lines(&["device/libmid".into()]).unwrap_err()
        );
        assert!(err.contains("which is not a shared library"), "{err}");
    }

    /// The metadata list: read as written; refused when absent, empty, not strings, or not `<device|host>/<name>`.
    #[test]
    fn package_metadata() {
        let ok = "[package]\nname = \"p\"\n[package.metadata.atl]\nconverter = [\"device/libbinder_rs\", \"host/aidl\"]\n";
        assert_eq!(
            package_modules(ok).unwrap(),
            ["device/libbinder_rs", "host/aidl"]
        );
        // `aidl` adds the package's AIDL crate rlib after the list
        let aidl = "[package]\nname = \"p\"\n[package.metadata.atl]\nconverter = [\"device/libbinder_rs\"]\n[package.metadata.atl.aidl]\ninclude_dirs = [\"i\"]\ntypes = [\"a.IFoo\"]\n";
        assert_eq!(
            package_modules(aidl).unwrap(),
            ["device/libbinder_rs", "device/p-stubs-rust"]
        );
        let err = |t: &str| format!("{:#}", package_modules(t).unwrap_err());
        assert!(err("[package]\nname = \"p\"\n").contains("names no converter module"));
        assert!(err("[package.metadata.atl]\nconverter = []\n").contains("empty"));
        assert!(err("[package.metadata.atl]\nconverter = [1]\n").contains("list of strings"));
        for bad in ["libbinder_rs", "guest/x", "device/", "device/a/b"] {
            assert!(
                err(&format!(
                    "[package.metadata.atl]\nconverter = [\"{bad}\"]\n"
                ))
                .contains("not a module directory"),
                "{bad}"
            );
        }
    }
}
