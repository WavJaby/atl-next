// SPDX-License-Identifier: MIT
//! Workspace packages built after the converter (`xtask native`, `xtask test native-load`): a package naming converter modules in its Cargo.toml (`[package.metadata.atl] converter`, graph.json module directories) is built with its feature `converter` once the build's graph.json `artifacts` hold every one of them; its build script links them from `$ATL_NATIVE_OUT` (atl-bridgegen converter.rs).
//! `[package.metadata.atl.aidl]` asks for an AIDL crate: `include_dirs` (tree-relative import roots), `types` (qualified AIDL names compiled with their import closure), optional `parcelables` (a workspace package with `rust_library`: the crate giving unstructured parcelables and Java-only builtins their Rust types).
//! The converter defines an unstable aidl_interface `<package>-stubs` over them (bp2ninja `-aidl-crates`, aidlcrates.go; a missing dir or type = converter error), every `xtask native` adds its `-rust` rlib and the package's other modules to the goals, and the package links `device/<package>-stubs-rust` (crate `<package>_stubs`) like a `converter` module.
//! `[package.metadata.atl.rust_library]` (`rustlibs`, optional `aconfig`) makes the converter build the package's `src/lib.rs` as rust_library `lib<crate>` with those rustlibs and the package's edition (Soong's `additional_rustlibs` crate of an aidl_interface), the flag dumps of the `aconfig` declarations in its OUT_DIR, and our lints (`lint_flags`, through clippy-driver, never capped); cargo builds it only as an empty library (`#![cfg(...)]`-gated), so it needs no feature. Only an aidl crate's `parcelables` reaches it.
//! The build is the converter's way of linking: cargo's linker is graph.json `link_driver` (rlib link arguments are its arguments), `panic = "abort"` as the converter's device crates (a crate of the other strategy fails the link), release, `target_dir(out)` (a no-op when current).
//! Then clippy, warnings denied, on the same packages and feature: `xtask check` lints without it, so this is where the converter-linked code is linted. `xtask check` step converter-packages runs that clippy plus the packages' unit tests (`check`) against an existing out dir: step test runs without the feature too.
//! Checked after: each executable's DT_NEEDED starts with the package's shared modules in the order named. libsigchain leading the launcher's is what makes ld.so's breadth-first global lookup find its `sigaction` / `signal` before libc's for every library loaded later, as for the converter's dalvikvm.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::{arches, provided_gate};

/// The package feature that links converter modules (atl-bridgegen converter.rs `FEATURE`).
const FEATURE: &str = "converter";
/// The environment variable naming the converter's build directory (atl-bridgegen converter.rs `NATIVE_OUT`).
const NATIVE_OUT: &str = "ATL_NATIVE_OUT";
/// Response file in the cargo target dir holding the stage's `-rpath-link` list (`Selection::cargo`).
const RPATH_LINK_FILE: &str = "rpath-link.rsp";
/// Deliberate break: set -> the list goes back into rustflags, so a device-set change must grow a new unit group.
const FLAGS_BREAK: &str = "ATL_CARGO_FLAGS_BREAK";

/// The suffix of a package's AIDL crate interface (`<package>-stubs`; atl-bridgegen converter.rs `AIDL_SUFFIX`): its crate `<package>_stubs` cannot clash with the package's own.
const AIDL_SUFFIX: &str = "-stubs";

/// The converter's aidl_interface name for `package`'s `[package.metadata.atl] aidl` sources (bp2ninja aidlcrates.go); its rlib is `device/<name>-rust`.
fn aidl_interface(package: &str) -> String {
    format!("{package}{AIDL_SUFFIX}")
}

/// A workspace package naming converter modules.
#[derive(Debug, PartialEq)]
pub struct Package {
    pub name: String,
    /// the converter modules it links: `converter`, then its AIDL crate's rlib when it has `aidl`
    pub modules: Vec<String>,
    /// `[package.metadata.atl.aidl]`: its AIDL crate
    pub aidl: Option<AidlSpec>,
    /// its executables (cargo bin targets)
    pub bins: Vec<String>,
}

/// `[package.metadata.atl.aidl]`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AidlSpec {
    pub include_dirs: Vec<String>,
    pub types: Vec<String>,
    /// the workspace package (with `rust_library`) giving parcelables their Rust types
    #[serde(default)]
    pub parcelables: Option<String>,
}

/// A package with `[package.metadata.atl] rust_library`, as bp2ninja `-aidl-crates` takes it (aidlcrates.go AidlParcelables).
#[derive(Debug, PartialEq, Clone, serde::Serialize)]
pub struct RustLibrary {
    #[serde(skip)]
    pub package: String,
    pub module: String,
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub dir: PathBuf,
    pub edition: String,
    pub rustlibs: Vec<String>,
    /// aconfig_declarations whose flag dumps go into its OUT_DIR
    pub aconfig: Vec<String>,
    /// lint flags: `-D warnings` and the workspace's `[workspace.lints]` (`lint_flags`), as cargo would apply them
    pub lints: Vec<String>,
}

/// `[package.metadata.atl.rust_library]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RustLibrarySpec {
    rustlibs: Vec<String>,
    #[serde(default)]
    aconfig: Vec<String>,
}

/// One bp2ninja `-aidl-crates` entry (aidlcrates.go AidlCrate).
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AidlCrate {
    pub include_dirs: Vec<String>,
    pub types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parcelables: Option<RustLibrary>,
}

/// What `cargo metadata --no-deps` says of a package (the fields read here).
#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    features: BTreeMap<String, serde_json::Value>,
    metadata: Option<serde_json::Value>,
    targets: Vec<MetaTarget>,
    #[serde(default)]
    edition: String,
    #[serde(default)]
    manifest_path: PathBuf,
}

#[derive(Deserialize)]
struct MetaTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
}

/// The packages naming converter modules in `cargo metadata` output; one naming them without a `converter` feature is refused (nothing would ever link them).
fn parse_packages(json: &str) -> Result<Vec<Package>> {
    let meta: Metadata = serde_json::from_str(json).context("parsing cargo metadata")?;
    let mut out = Vec::new();
    for p in meta.packages {
        let Some(list) = p
            .metadata
            .as_ref()
            .and_then(|m| m.get("atl"))
            .and_then(|a| a.get(FEATURE))
        else {
            continue;
        };
        let mut modules: Vec<String> = serde_json::from_value(list.clone()).with_context(|| {
            format!(
                "{}: [package.metadata.atl] converter is not a list of strings",
                p.name
            )
        })?;
        ensure!(
            !modules.is_empty(),
            "{}: [package.metadata.atl] converter is empty",
            p.name
        );
        let aidl: Option<AidlSpec> = match p.metadata.as_ref().and_then(|m| m["atl"].get("aidl")) {
            None => None,
            Some(table) => {
                let aidl: AidlSpec = serde_json::from_value(table.clone()).with_context(|| {
                    format!(
                        "{}: [package.metadata.atl.aidl] is not {{ include_dirs, types, parcelables? }}",
                        p.name
                    )
                })?;
                ensure!(
                    !aidl.include_dirs.is_empty() && !aidl.types.is_empty(),
                    "{}: [package.metadata.atl.aidl] include_dirs and types must not be empty",
                    p.name
                );
                modules.push(format!("device/{}-rust", aidl_interface(&p.name)));
                Some(aidl)
            }
        };
        ensure!(
            p.features.contains_key(FEATURE),
            "{}: names converter modules but has no feature `{FEATURE}` to link them under",
            p.name
        );
        let bins = p
            .targets
            .into_iter()
            .filter(|t| t.kind.iter().any(|k| k == "bin"))
            .map(|t| t.name)
            .collect();
        out.push(Package {
            name: p.name,
            modules,
            aidl,
            bins,
        });
    }
    Ok(out)
}

/// rustc / clippy-driver flags of the workspace's lints: `-D warnings` (xtask native's clippy denies them too), then each `[workspace.lints.<tool>]` entry by its level (`deny` -> `-D tool::name`; rustc's own without a prefix).
fn lint_flags(workspace_toml: &str) -> Result<Vec<String>> {
    let t: toml::Table =
        toml::from_str(workspace_toml).context("parsing the workspace Cargo.toml")?;
    let mut out = vec!["-D".to_owned(), "warnings".to_owned()];
    let Some(lints) = t
        .get("workspace")
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.as_table())
    else {
        return Ok(out);
    };
    for (tool, table) in lints {
        for (name, level) in table
            .as_table()
            .context("[workspace.lints.<tool>] is a table")?
        {
            let level = level
                .as_str()
                .or_else(|| level.get("level").and_then(|l| l.as_str()))
                .with_context(|| format!("[workspace.lints.{tool}] {name}: no level"))?;
            let flag = match level {
                "forbid" => "-F",
                "deny" => "-D",
                "warn" => "-W",
                "allow" => "-A",
                other => anyhow::bail!("[workspace.lints.{tool}] {name}: level {other}"),
            };
            let lint = if tool == "rust" {
                name.clone()
            } else {
                format!("{tool}::{name}")
            };
            out.extend([flag.to_owned(), lint]);
        }
    }
    Ok(out)
}

/// The packages with `[package.metadata.atl.rust_library]` (non-empty rustlibs), each compiled with `lints`; one without a lib target is refused.
fn parse_rust_libraries(json: &str, lints: &[String]) -> Result<Vec<RustLibrary>> {
    let meta: Metadata = serde_json::from_str(json).context("parsing cargo metadata")?;
    let mut out = Vec::new();
    for p in meta.packages {
        let Some(list) = p
            .metadata
            .as_ref()
            .and_then(|m| m.get("atl"))
            .and_then(|a| a.get("rust_library"))
        else {
            continue;
        };
        let spec: RustLibrarySpec = serde_json::from_value(list.clone()).with_context(|| {
            format!(
                "{}: [package.metadata.atl.rust_library] is not {{ rustlibs, aconfig? }}",
                p.name
            )
        })?;
        ensure!(
            !spec.rustlibs.is_empty(),
            "{}: [package.metadata.atl.rust_library] rustlibs is empty",
            p.name
        );
        let lib = p
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| k == "lib"))
            .with_context(|| format!("{}: rust_library but no lib target", p.name))?;
        out.push(RustLibrary {
            module: format!("lib{}", lib.name),
            crate_name: lib.name.clone(),
            dir: p
                .manifest_path
                .parent()
                .context("manifest path has a dir")?
                .to_path_buf(),
            edition: p.edition,
            rustlibs: spec.rustlibs,
            aconfig: spec.aconfig,
            lints: lints.to_vec(),
            package: p.name,
        });
    }
    Ok(out)
}

/// The packages' AIDL crates as bp2ninja `-aidl-crates` takes them (interface name -> crate), and converter goals: their rlibs and the package's other modules, added to every build, so a package with `aidl` links whatever the goals
/// (a stable HAL interface's own `-V<n>-rust` backend is such a module: no goal of the stubs' closure reaches it).
/// A `parcelables` naming no package with `rust_library` is refused.
pub fn aidl_crates(
    packages: &[Package],
    libraries: &[RustLibrary],
) -> Result<(BTreeMap<String, AidlCrate>, Vec<String>)> {
    let mut crates = BTreeMap::new();
    let mut goals: Vec<String> = Vec::new();
    for p in packages {
        // every converter module a package names is a goal of every build, AIDL package or not: the package links it, and the graph.json the gates and `xtask check` read holds it
        for m in &p.modules {
            let goal = m.strip_prefix("device/").unwrap_or(m);
            if !goals.iter().any(|g| g == goal) {
                goals.push(goal.to_owned());
            }
        }
        let Some(a) = &p.aidl else { continue };
        let parcelables = match &a.parcelables {
            None => None,
            Some(name) => Some(
                libraries
                    .iter()
                    .find(|l| &l.package == name)
                    .with_context(|| {
                        format!(
                            "{}: aidl.parcelables {name} is no package with [package.metadata.atl] rust_library",
                            p.name
                        )
                    })?
                    .clone(),
            ),
        };
        crates.insert(
            aidl_interface(&p.name),
            AidlCrate {
                include_dirs: a.include_dirs.clone(),
                types: a.types.clone(),
                parcelables,
            },
        );
    }
    Ok((crates, goals))
}

/// `cargo metadata --no-deps` of checkout `root`.
fn workspace_metadata(root: &Path) -> Result<String> {
    let out = crate::cargo_command()
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .output()
        .context("running cargo metadata")?;
    ensure!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// The workspace packages of checkout `root` naming converter modules.
pub fn workspace_packages(root: &Path) -> Result<Vec<Package>> {
    parse_packages(&workspace_metadata(root)?)
}

/// The AIDL crates of checkout `root`'s packages and their goals (`aidl_crates`).
pub fn workspace_aidl_crates(root: &Path) -> Result<(BTreeMap<String, AidlCrate>, Vec<String>)> {
    let json = workspace_metadata(root)?;
    let toml_path = root.join("Cargo.toml");
    let lints = lint_flags(
        &std::fs::read_to_string(&toml_path)
            .with_context(|| format!("reading {}", toml_path.display()))?,
    )?;
    aidl_crates(
        &parse_packages(&json)?,
        &parse_rust_libraries(&json, &lints)?,
    )
}

/// The cargo target dir of the converter's builds (`Selection::cargo`), read back by parcel.rs, compositor.rs.
/// Not `<out>/cargo`: the link driver (`--config target.<triple>.linker`) is checked per unit (`ConfigSettingsChanged`) but is not in the unit hash, so a plain build sharing the dir overwrites the other's fingerprints and every alternation recompiles (measured: 26-38 units per run).
pub fn target_dir(out: &Path) -> PathBuf {
    out.join("cargo-converter")
}

/// graph.json, the fields read here (bp2ninja graph.go).
#[derive(Deserialize)]
struct Graph {
    link_driver: String,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    kind: String,
    out: PathBuf,
}

/// The packages whose modules `graph` holds, and for each other one the first module it lacks.
fn select<'a>(
    packages: &'a [Package],
    graph: &Graph,
) -> (Vec<&'a Package>, Vec<(&'a Package, &'a str)>) {
    let mut built = Vec::new();
    let mut skipped = Vec::new();
    for p in packages {
        match p
            .modules
            .iter()
            .find(|m| !graph.artifacts.contains_key(m.as_str()))
        {
            None => built.push(p),
            Some(m) => skipped.push((p, m.as_str())),
        }
    }
    (built, skipped)
}

/// The sonames DT_NEEDED of `package`'s executables must start with: its shared modules' file names, in the order named.
fn leading_needed(package: &Package, graph: &Graph) -> Result<Vec<String>> {
    package
        .modules
        .iter()
        .filter_map(|m| graph.artifacts.get(m).filter(|a| a.kind == "shared"))
        .map(|a| {
            a.out
                .file_name()
                .and_then(|f| f.to_str())
                .map(str::to_owned)
                .with_context(|| format!("{}: no file name", a.out.display()))
        })
        .collect()
}

/// `needed` (DT_NEEDED in order) starts with `leading`.
fn check_needed(bin: &Path, needed: &[String], leading: &[String]) -> Result<()> {
    ensure!(
        needed.starts_with(leading),
        "{}: DT_NEEDED {needed:?} does not start with {leading:?}",
        bin.display()
    );
    Ok(())
}

/// One package built: its executables.
pub struct Built {
    pub package: String,
    pub bins: Vec<PathBuf>,
}

/// The converter packages of `out`'s graph.json, and the cargo invocation that builds and lints them: one shape for `build` and `lint`, so clippy sees the code as built (feature on, the converter's rlibs found).
struct Selection<'a> {
    root: &'a Path,
    out: &'a Path,
    /// cargo's target dir: `out/cargo`, the one `xtask native` builds in
    target: PathBuf,
    graph: Graph,
    selected: Vec<&'a Package>,
    /// each package the graph lacks a module of, with that module
    skipped: Vec<(&'a Package, &'a str)>,
    /// the build machine's Rust target triple (cargo's linker config key)
    triple: String,
}

impl<'a> Selection<'a> {
    fn new(root: &'a Path, out: &'a Path, packages: &'a [Package]) -> Result<Self> {
        let graph_file = out.join("graph.json");
        let graph: Graph = serde_json::from_str(
            &std::fs::read_to_string(&graph_file)
                .with_context(|| format!("reading {}", graph_file.display()))?,
        )
        .with_context(|| format!("parsing {}", graph_file.display()))?;
        let (selected, skipped) = select(packages, &graph);
        let arches = arches::load(root)?;
        let (_, machine) = arches::build_machine(root, &arches)?;
        let triple = machine
            .host
            .as_ref()
            .context("the build machine's row is a [host] row")?
            .rust
            .clone();
        Ok(Self {
            root,
            out,
            target: target_dir(out),
            graph,
            selected,
            skipped,
            triple,
        })
    }

    fn names(&self) -> String {
        self.selected
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// cargo `sub` on the selected packages
    /// Links resolve the needs of converter libraries through the stage's dirs (binder::lib_env) as `-rpath-link`, link time only: those libraries carry no runpath (runpath_gate), and ld.bfd, unlike the converter's lld, wants every needed library's own needs found.
    /// One global flag, not per-module build-script lines: `rustc-link-arg` reaches only its own package's links, never a dependent's.
    /// The list goes through a response file the link driver expands: rustflags enter every unit's hash, so the list itself in them kept a whole new unit tree per device set.
    /// cargo does not track the file, on purpose: -rpath-link only resolves needs at link time, nothing of it reaches the output, and a changed linked module reruns the build script (`link_lines`), so that relink reads the list written here.
    fn cargo(&self, sub: &str) -> Result<std::process::Command> {
        let stage = crate::binder::lib_env(self.out)?
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
            .join(":");
        let rpath_link = format!("-Wl,-rpath-link,{stage}");
        let rsp = self.target.join(RPATH_LINK_FILE);
        std::fs::create_dir_all(&self.target).with_context(|| self.target.display().to_string())?;
        std::fs::write(&rsp, &rpath_link).with_context(|| rsp.display().to_string())?;
        let link_arg = if std::env::var_os(FLAGS_BREAK).is_some() {
            rpath_link
        } else {
            format!("@{}", rsp.display())
        };
        let mut cmd = crate::cargo_command();
        cmd.current_dir(self.root)
            .args([sub, "--release", "--locked"])
            .arg("--target-dir")
            .arg(&self.target);
        for p in &self.selected {
            cmd.args([
                "-p",
                &p.name,
                "--features",
                &format!("{}/{FEATURE}", p.name),
            ]);
        }
        cmd.args(["--config", "profile.release.panic=\"abort\""])
            .args([
                "--config",
                &format!("target.{}.linker={:?}", self.triple, self.graph.link_driver),
            ])
            .args([
                "--config",
                &format!(
                    "target.{}.rustflags=[{:?}]",
                    self.triple,
                    format!("-Clink-arg={link_arg}")
                ),
            ])
            .env(NATIVE_OUT, self.out);
        Ok(cmd)
    }

    /// The code behind feature `converter` is linted where it is built (`xtask check` step clippy runs without the feature).
    fn lint(&self) -> Result<()> {
        let status = crate::unit_use::status(
            self.cargo("clippy")?
                .args(["--lib", "--bins", "--", "-D", "warnings"]),
        )
        .context("running cargo clippy (converter packages)")?;
        ensure!(status.success(), "clippy on {} failed", self.names());
        Ok(())
    }

    /// Unit tests with feature `converter` on: `xtask check` step test runs without it, so `#[cfg(test)]` code in converter-gated modules (atl-services `package`, ...) runs only here.
    /// Same target dir and flags as `build`. The converter's rlibs are device crates, built panic=abort, and stable libtest links only unwinding crates, so tests run with
    /// `-Z panic-abort-tests` as Soong builds device rust tests (build/soong/rust/test.go `compilerFlags`, under its global RUSTC_BOOTSTRAP). The bootstrap is set for this test build only:
    /// `build` and the clippy steps stay on stable, so an unstable feature in our code still fails there.
    /// Test binaries run in the stage's library environment (binder::lib_env), as every runtime process: converter device libraries carry no runpath (runpath_gate), so their own needs are found only there.
    fn test(&self) -> Result<()> {
        let status = crate::unit_use::status(
            self.cargo("test")?
                .args(["-Z", "panic-abort-tests", "--lib", "--bins"])
                .env("RUSTC_BOOTSTRAP", "1")
                .envs(crate::binder::lib_env(self.out)?),
        )
        .context("running cargo test (converter packages)")?;
        ensure!(status.success(), "tests of {} failed", self.names());
        Ok(())
    }
}

/// Builds every package of `root` whose converter modules `out`'s graph.json holds (printing the others with the module they lack) and checks their executables; what was built.
pub fn build(root: &Path, out: &Path) -> Result<Vec<Built>> {
    let packages = workspace_packages(root)?;
    let sel = Selection::new(root, out, &packages)?;
    for (p, m) in &sel.skipped {
        println!(
            "native: package {} not built: the goals' closure lacks {m}",
            p.name
        );
    }
    if sel.selected.is_empty() {
        return Ok(Vec::new());
    }
    let status = crate::unit_use::status(&mut sel.cargo("build")?)
        .context("running cargo build (converter packages)")?;
    ensure!(status.success(), "building {} failed", sel.names());
    sel.lint()?;
    let mut built = Vec::new();
    for p in sel.selected.iter().copied() {
        let leading = leading_needed(p, &sel.graph)?;
        let mut bins = Vec::new();
        for b in &p.bins {
            let bin = sel.target.join("release").join(b);
            check_needed(&bin, &provided_gate::needed(&bin)?, &leading)?;
            bins.push(bin);
        }
        built.push(Built {
            package: p.name.clone(),
            bins,
        });
    }
    Ok(built)
}

/// `xtask check` step converter-packages: clippy (as `build` lints them) then unit tests on the converter packages of `out`'s graph.json. Returns the packages it could not check, each with the
/// module the graph lacks (a slot behind the checkout): the converter feature links converter outputs, so without them nothing compiles those modules, and `check` names
/// the step skipped rather than reporting it passed.
pub fn check(root: &Path, out: &Path) -> Result<Vec<String>> {
    let packages = workspace_packages(root)?;
    let sel = Selection::new(root, out, &packages)?;
    let unlinted = sel
        .skipped
        .iter()
        .map(|(p, m)| format!("{} (the graph lacks {m})", p.name))
        .collect();
    if !sel.selected.is_empty() {
        sel.lint()?;
        sel.test()?;
    }
    Ok(unlinted)
}

/// Executable `bin` of `package`, built as `build` builds it; fails when the graph lacks the package's modules.
pub fn bin(root: &Path, out: &Path, package: &str, bin: &str) -> Result<PathBuf> {
    build(root, out)?
        .into_iter()
        .find(|b| b.package == package)
        .with_context(|| {
            format!(
                "{package} not built: its converter modules are not in {}",
                out.display()
            )
        })?
        .bins
        .into_iter()
        .find(|b| b.file_name().is_some_and(|f| f == bin))
        .with_context(|| format!("{package} has no executable {bin}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cargo metadata's package shape (format-version 1), the fields read; `features` / `targets` as cargo prints them.
    fn meta(packages: &[serde_json::Value]) -> String {
        serde_json::json!({ "packages": packages, "version": 1 }).to_string()
    }

    fn pkg(
        name: &str,
        metadata: serde_json::Value,
        features: &[&str],
        bins: &[&str],
    ) -> serde_json::Value {
        let mut targets: Vec<serde_json::Value> =
            vec![serde_json::json!({ "name": name, "kind": ["lib"] })];
        targets.extend(
            bins.iter()
                .map(|b| serde_json::json!({ "name": b, "kind": ["bin"] })),
        );
        serde_json::json!({
            "name": name,
            "features": features.iter().map(|f| (f.to_string(), serde_json::json!([]))).collect::<BTreeMap<_, _>>(),
            "metadata": metadata,
            "targets": targets,
        })
    }

    /// Packages with the list are found with their executables; others ignored; a list without the feature, an empty list or one of non-strings is refused.
    #[test]
    fn packages_from_metadata() {
        let list = serde_json::json!({ "atl": { "converter": ["device/libsigchain"] } });
        let json = meta(&[
            pkg("launcher", list.clone(), &["converter"], &["launcher"]),
            pkg("plain", serde_json::Value::Null, &[], &["plain"]),
        ]);
        assert_eq!(
            parse_packages(&json).unwrap(),
            [Package {
                name: "launcher".into(),
                modules: vec!["device/libsigchain".into()],
                aidl: None,
                bins: vec!["launcher".into()]
            }]
        );
        // `aidl` adds the package's AIDL crate: a module to link and an -aidl-crates entry + goal; `parcelables` resolves to the rust_library package
        let with_aidl = serde_json::json!({ "atl": { "converter": ["device/libbinder_rs"], "aidl": { "include_dirs": ["i"], "types": ["a.IFoo"], "parcelables": "lib-p" } } });
        let mut lib_p = pkg(
            "lib-p",
            serde_json::json!({ "atl": { "rust_library": { "rustlibs": ["libbinder_rs"], "aconfig": ["f-aconfig"] } } }),
            &[],
            &[],
        );
        lib_p["targets"][0]["name"] = "lib_p".into();
        lib_p["edition"] = "2024".into();
        lib_p["manifest_path"] = "/w/crates/lib-p/Cargo.toml".into();
        let json = meta(&[pkg("p", with_aidl, &["converter"], &[]), lib_p]);
        let parsed = parse_packages(&json).unwrap();
        assert_eq!(
            parsed[0].modules,
            ["device/libbinder_rs", "device/p-stubs-rust"]
        );
        let lints = vec!["-D".to_owned(), "warnings".to_owned()];
        let libs = parse_rust_libraries(&json, &lints).unwrap();
        assert_eq!(
            (libs[0].aconfig.as_slice(), libs[0].lints.as_slice()),
            (["f-aconfig".to_owned()].as_slice(), lints.as_slice())
        );
        let (crates, goals) = aidl_crates(&parsed, &libs).unwrap();
        let p = crates["p-stubs"].parcelables.as_ref().unwrap();
        assert_eq!(
            (
                p.module.as_str(),
                p.crate_name.as_str(),
                p.dir.as_path(),
                p.edition.as_str()
            ),
            ("liblib_p", "lib_p", Path::new("/w/crates/lib-p"), "2024")
        );
        assert_eq!(crates["p-stubs"].types, ["a.IFoo"]);
        assert_eq!(goals, ["libbinder_rs", "p-stubs-rust"]);
        // parcelables naming a package without rust_library
        let e = aidl_crates(&parsed, &[]).unwrap_err();
        assert!(format!("{e:#}").contains("no package with [package.metadata.atl] rust_library"));
        let rl = |v: serde_json::Value| {
            format!(
                "{:#}",
                parse_rust_libraries(
                    &meta(&[pkg(
                        "x",
                        serde_json::json!({ "atl": { "rust_library": v } }),
                        &[],
                        &[]
                    )]),
                    &lints
                )
                .unwrap_err()
            )
        };
        assert!(rl(serde_json::json!({ "rustlibs": [] })).contains("rustlibs is empty"));
        assert!(rl(serde_json::json!(["libbinder_rs"])).contains("is not { rustlibs"));
        assert!(
            rl(serde_json::json!({ "rustlibs": ["a"], "typo": 1 })).contains("is not { rustlibs")
        );
        // lint flags: -D warnings first, then each workspace lint by level and tool
        let flags = lint_flags("[workspace.lints.clippy]\nunwrap_used = \"deny\"\n[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = { level = \"warn\" }\n").unwrap();
        assert_eq!(
            flags,
            [
                "-D",
                "warnings",
                "-D",
                "clippy::unwrap_used",
                "-W",
                "unsafe_op_in_unsafe_fn"
            ]
        );
        let err = |p: serde_json::Value| format!("{:#}", parse_packages(&meta(&[p])).unwrap_err());
        assert!(err(pkg("x", list, &[], &[])).contains("no feature `converter`"));
        assert!(
            err(pkg(
                "x",
                serde_json::json!({ "atl": { "converter": [] } }),
                &["converter"],
                &[]
            ))
            .contains("empty")
        );
        assert!(
            err(pkg(
                "x",
                serde_json::json!({ "atl": { "converter": [1] } }),
                &["converter"],
                &[]
            ))
            .contains("list of strings")
        );
        assert!(
            err(pkg(
                "x",
                serde_json::json!({ "atl": { "converter": ["device/a"], "aidl": { "include_dirs": [], "types": ["a.B"] } } }),
                &["converter"],
                &[]
            ))
            .contains("must not be empty")
        );
        assert!(
            err(pkg(
                "x",
                serde_json::json!({ "atl": { "converter": ["device/a"], "aidl": [":libbinder_aidl"] } }),
                &["converter"],
                &[]
            ))
            .contains("is not { include_dirs")
        );
    }

    fn graph() -> Graph {
        serde_json::from_value(serde_json::json!({
            "link_driver": "clang++-19",
            "artifacts": {
                "device/libsigchain": { "kind": "shared", "out": "/o/device/libsigchain/libsigchain.so" },
                "device/libbinder_rs": { "kind": "rlib", "out": "/o/device/libbinder_rs/libbinder.rlib", "crate": "binder" }
            }
        }))
        .unwrap()
    }

    /// Built only when the graph holds every module named; the leading DT_NEEDED entries are the shared modules only, in order.
    #[test]
    fn selection_and_leading_needed() {
        let p = |name: &str, modules: &[&str]| Package {
            name: name.into(),
            modules: modules.iter().map(|m| (*m).to_owned()).collect(),
            aidl: None,
            bins: Vec::new(),
        };
        let packages = [
            p("a", &["device/libsigchain", "device/libbinder_rs"]),
            p("b", &["device/libbinder_rs", "device/libgone"]),
        ];
        let g = graph();
        let (built, skipped) = select(&packages, &g);
        assert_eq!(
            built.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(
            skipped
                .iter()
                .map(|(p, m)| (p.name.as_str(), *m))
                .collect::<Vec<_>>(),
            [("b", "device/libgone")]
        );
        assert_eq!(
            leading_needed(&packages[0], &g).unwrap(),
            ["libsigchain.so"]
        );
    }

    /// The DT_NEEDED check fires when the shared module is not first (as when it is linked by `link-arg`, after std's -lc).
    #[test]
    fn needed_order() {
        let bin = Path::new("/b");
        let lead = ["libsigchain.so".to_owned()];
        assert!(check_needed(bin, &["libsigchain.so".into(), "libc.so.6".into()], &lead).is_ok());
        let err =
            check_needed(bin, &["libc.so.6".into(), "libsigchain.so".into()], &lead).unwrap_err();
        assert!(format!("{err:#}").contains("does not start with"));
        assert!(check_needed(bin, &["libc.so.6".into()], &[]).is_ok());
    }
}
