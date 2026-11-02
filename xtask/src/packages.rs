// SPDX-License-Identifier: MIT
//! Workspace packages built after the converter (`xtask native`, `xtask test native-load`): a package naming converter modules in its Cargo.toml (`[package.metadata.atl] converter`, graph.json module directories) is built with its feature `converter` once the build's graph.json `artifacts` hold every one of them; its build script links them from `$ATL_NATIVE_OUT` (atl-bridgegen converter.rs).
//! The build is the converter's way of linking: cargo's linker is graph.json `link_driver` (rlib link arguments are its arguments), `panic = "abort"` as the converter's device crates (a crate of the other strategy fails the link), release, `<out>/cargo` (a no-op when current).
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

/// A workspace package naming converter modules.
#[derive(Debug, PartialEq)]
pub struct Package {
    pub name: String,
    pub modules: Vec<String>,
    /// its executables (cargo bin targets)
    pub bins: Vec<String>,
}

/// What `cargo metadata --no-deps` says of a package (the fields read here).
#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    features: BTreeMap<String, serde_json::Value>,
    metadata: Option<serde_json::Value>,
    targets: Vec<MetaTarget>,
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
        let modules: Vec<String> = serde_json::from_value(list.clone()).with_context(|| {
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
            bins,
        });
    }
    Ok(out)
}

/// The workspace packages of checkout `root` naming converter modules.
pub fn workspace_packages(root: &Path) -> Result<Vec<Package>> {
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
    parse_packages(&String::from_utf8(out.stdout)?)
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

/// Builds every package of `root` whose converter modules `out`'s graph.json holds (printing the others with the module they lack) and checks their executables; what was built.
pub fn build(root: &Path, out: &Path) -> Result<Vec<Built>> {
    let graph_file = out.join("graph.json");
    let graph: Graph = serde_json::from_str(
        &std::fs::read_to_string(&graph_file)
            .with_context(|| format!("reading {}", graph_file.display()))?,
    )
    .with_context(|| format!("parsing {}", graph_file.display()))?;
    let packages = workspace_packages(root)?;
    let (selected, skipped) = select(&packages, &graph);
    for (p, m) in skipped {
        println!(
            "native: package {} not built: the goals' closure lacks {m}",
            p.name
        );
    }
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let arches = arches::load(root)?;
    let (_, machine) = arches::build_machine(root, &arches)?;
    let triple = &machine
        .host
        .as_ref()
        .context("the build machine's row is a [host] row")?
        .rust;
    let target = out.join("cargo");
    let mut cmd = crate::cargo_command();
    cmd.current_dir(root)
        .args(["build", "--release", "--locked"])
        .arg("--target-dir")
        .arg(&target);
    for p in &selected {
        cmd.args([
            "-p",
            &p.name,
            "--features",
            &format!("{}/{FEATURE}", p.name),
        ]);
    }
    let status = cmd
        .args(["--config", "profile.release.panic=\"abort\""])
        .args([
            "--config",
            &format!("target.{triple}.linker={:?}", graph.link_driver),
        ])
        .env(NATIVE_OUT, out)
        .status()
        .context("running cargo build (converter packages)")?;
    ensure!(
        status.success(),
        "building {} failed",
        selected
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut built = Vec::new();
    for p in selected {
        let leading = leading_needed(p, &graph)?;
        let mut bins = Vec::new();
        for b in &p.bins {
            let bin = target.join("release").join(b);
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
                bins: vec!["launcher".into()]
            }]
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
