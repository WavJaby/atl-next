// SPDX-License-Identifier: MIT
//! `providers.toml`: Android.bp modules provided by our crates, upstreams built by their own build system, or system libraries instead of AOSP source. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::deps::Deps;
use crate::upstream;

pub const FILE: &str = "providers.toml";
/// Provider of a module nothing provides on this host: a dependency on it is dropped.
pub const NONE: &str = "none";

/// Modules whose dependencies the converter drops (`provider = "none"` for the whole module).
pub fn omitted(providers: &Providers) -> Vec<&str> {
    providers
        .iter()
        .filter(|(_, row)| row.is_none() && row.srcs.is_empty())
        .map(|(module, _)| module.as_str())
        .collect()
}

/// `(module, name)` for each `<kind>:<name>` provider of each row.
fn of_kind<'a>(providers: &'a Providers, kind: &str) -> Vec<(&'a str, &'a str)> {
    providers
        .iter()
        .flat_map(|(module, row)| {
            row.provider.iter().filter_map(move |p| {
                let (k, name) = p.split_once(':')?;
                (k == kind).then_some((module.as_str(), name))
            })
        })
        .collect()
}

/// Rows replacing only some sources of a module (`srcs`): (module, row).
pub fn partial(providers: &Providers) -> Vec<(&str, &Provider)> {
    providers
        .iter()
        .filter(|(_, row)| !row.srcs.is_empty())
        .map(|(module, row)| (module.as_str(), row))
        .collect()
}

/// Upstreams the `upstream:<name>` rows name (each built once, whichever modules it provides).
pub fn upstreams(providers: &Providers) -> BTreeSet<&str> {
    of_kind(providers, "upstream")
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

/// Modules our crates provide (`crate:<name>`): (module, crate).
pub fn crates(providers: &Providers) -> Vec<(&str, &str)> {
    of_kind(providers, "crate")
}

/// Modules system libraries provide (`system:<deps.toml row>`): (module, row).
pub fn systems(providers: &Providers) -> Vec<(&str, &str)> {
    of_kind(providers, "system")
}

/// One module provided outside the tree (`crate:`, `upstream:` and `system:` providers), as `xtask native` hands it to the converter in `<out>/provided.json` (bp2ninja provided.go `Provided`).
#[derive(Debug, Clone, Serialize)]
pub struct Provided {
    /// Absolute paths of the shared objects or archives dependents link, in this order: one per provider of the row.
    pub libs: Vec<PathBuf>,
    /// Header dirs dependents get as `-isystem`.
    pub include: Vec<PathBuf>,
    /// `NAME=VALUE` defines the headers depend on, given to dependents as `-D`.
    pub defines: Vec<String>,
}

impl Provided {
    /// The row's providers together, in the row's order: their libraries, include dirs and defines.
    pub fn join(parts: Vec<Provided>) -> Provided {
        let mut out = Provided {
            libs: Vec::new(),
            include: Vec::new(),
            defines: Vec::new(),
        };
        for p in parts {
            out.libs.extend(p.libs);
            for i in p.include {
                if !out.include.contains(&i) {
                    out.include.push(i);
                }
            }
            for d in p.defines {
                if !out.defines.contains(&d) {
                    out.defines.push(d);
                }
            }
        }
        out
    }
}

/// One `srcs` row as `xtask native` hands it to the converter in `<out>/srcs.json` (bp2ninja provided.go `Partial`): the module is built from the tree without these sources, linking the provider's artifact if it has one.
#[derive(Debug, Serialize)]
pub struct Partial {
    /// As the row lists them: relative to the module's directory.
    pub srcs: Vec<String>,
    /// None for `provider = "none"`: nothing replaces the dropped sources.
    pub provider: Option<Provided>,
}

/// Row key = Android.bp module name.
pub type Providers = BTreeMap<String, Provider>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// One provider, or several linked in this order (written as a string or a list); empty on a `rust_os` row.
    #[serde(default, deserialize_with = "one_or_many")]
    pub provider: Vec<String>,
    pub reason: String,
    /// Empty = the whole module.
    #[serde(default)]
    pub srcs: Vec<String>,
    /// The tree builds the module, as the OS's userspace instead of linux_glibc's (bp2ninja rust.go rustOSSide).
    pub rust_os: Option<RustOs>,
}

/// `rust_os` values: the OS whose Android.bp `target` blocks a device Rust module takes; linux_glibc, every other crate's, is never written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RustOs {
    /// android blocks + `--cfg atl_android_userspace`, for a crate whose Android behaviour this runtime provides.
    Android,
}

/// Modules the converter compiles as Android userspace (`rust_os = "android"`); that each is a device Rust module needs the tree, the converter checks it (bp2ninja rust.go checkRustAndroid).
pub fn rust_android(providers: &Providers) -> Vec<&str> {
    providers
        .iter()
        .filter(|(_, row)| row.rust_os == Some(RustOs::Android))
        .map(|(module, _)| module.as_str())
        .collect()
}

impl Provider {
    /// `provider = "none"`.
    pub fn is_none(&self) -> bool {
        self.provider == [NONE]
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(p) => vec![p],
        OneOrMany::Many(ps) => ps,
    })
}

/// What a provider may name: workspace crates, sources.toml git upstreams, deps.toml rows.
pub struct Targets<'a> {
    pub crates: &'a BTreeSet<String>,
    pub upstreams: BTreeSet<&'a str>,
    pub deps: &'a Deps,
}

impl<'a> Targets<'a> {
    pub fn new(
        crates: &'a BTreeSet<String>,
        sources: &'a crate::sources::Sources,
        deps: &'a Deps,
    ) -> Self {
        let upstreams = sources.git.keys().map(String::as_str).collect();
        Self {
            crates,
            upstreams,
            deps,
        }
    }
}

pub fn load(root: &Path, targets: &Targets) -> Result<Providers> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text, targets).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str, targets: &Targets) -> Result<Providers> {
    let providers: Providers = toml::from_str(text)?;
    for (module, row) in &providers {
        validate(module, row, targets).with_context(|| format!("[{module}]"))?;
    }
    Ok(providers)
}

fn validate(module: &str, row: &Provider, targets: &Targets) -> Result<()> {
    ensure!(is_module_name(module), "not an Android.bp module name");
    ensure!(!row.reason.trim().is_empty(), "empty reason");
    if row.rust_os.is_some() {
        ensure!(
            row.provider.is_empty() && row.srcs.is_empty(),
            "rust_os: the tree builds the module, so the row has no provider or srcs"
        );
        return Ok(());
    }
    // that each names a source of the module at the pin needs the tree: the converter checks it (bp2ninja provided.go checkPartial)
    let mut seen = BTreeSet::new();
    for src in &row.srcs {
        ensure!(
            is_src_path(src),
            "srcs: `{src}` is not a plain path relative to the module's directory"
        );
        ensure!(seen.insert(src), "srcs: `{src}` listed twice");
    }
    if row.is_none() {
        return Ok(());
    }
    ensure!(
        !row.provider.is_empty(),
        "empty provider list (a row without one needs rust_os)"
    );
    let mut seen = BTreeSet::new();
    for p in &row.provider {
        ensure!(seen.insert(p), "provider `{p}` listed twice");
        ensure!(
            p != NONE,
            "`{NONE}` provides nothing, so it is never one of several providers"
        );
        validate_one(module, p, targets)?;
    }
    Ok(())
}

fn validate_one(module: &str, provider: &str, targets: &Targets) -> Result<()> {
    match provider.split_once(':') {
        Some(("crate", name)) => {
            ensure!(targets.crates.contains(name), "no workspace crate `{name}`")
        }
        Some(("upstream", name)) => {
            ensure!(
                targets.upstreams.contains(name),
                "no sources.toml upstream `{name}`"
            );
            let Some(recipe) = upstream::recipe(name) else {
                bail!("xtask has no build recipe for upstream `{name}` (upstream.rs RECIPES)")
            };
            ensure!(
                recipe.modules.contains(&module),
                "upstream `{name}` installs no module `{module}` (its recipe: {})",
                recipe.modules.join(", ")
            );
        }
        Some(("system", name)) => {
            let Some(dep) = targets.deps.get(name) else {
                bail!("no deps.toml row `{name}`")
            };
            ensure!(
                dep.pkgconfig.is_some(),
                "deps.toml row `{name}` has no pkgconfig"
            );
            ensure!(
                dep.used_by.iter().any(|u| u == "upstream:aosp"),
                "deps.toml row `{name}` lacks `upstream:aosp` in used_by"
            );
        }
        _ => bail!(
            "provider `{provider}` is not crate:<name>, upstream:<name>, system:<name> or {NONE}"
        ),
    }
    Ok(())
}

/// Soong module names: letters, digits and `_ . + -` (e.g. `libc++`).
fn is_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// A `srcs` entry: a file path under the module's directory, as Android.bp `srcs` write one; no glob (the check is per file), no module reference, nothing leaving the directory.
fn is_src_path(s: &str) -> bool {
    // an empty component also rejects "", a leading or trailing `/`, and `//`
    !s.starts_with(':')
        && !s.contains(['*', '?', '[', '\\'])
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses with workspace crate `atl-nativeloader`, sources.toml git upstreams `boringssl` (has a recipe) and `qemu` (has none), deps.toml rows `egl` (an AOSP library), `git` (no pkgconfig) and `zlib` (not used by upstream:aosp).
    fn parsed(text: &str) -> Result<Providers> {
        let crates = ["atl-nativeloader".to_owned()].into();
        let deps: Deps = toml::from_str(
            "[egl]\ndebian = \"libegl-dev\"\npkgconfig = \"egl\"\nscope = [\"build\"]\n\
             used_by = [\"upstream:aosp\"]\n\
             [git]\ndebian = \"git\"\nscope = [\"test\"]\nused_by = [\"xtask:check\"]\n\
             [zlib]\ndebian = \"zlib1g-dev\"\npkgconfig = \"zlib\"\nscope = [\"build\"]\n\
             used_by = [\"xtask:check\"]\n",
        )
        .unwrap();
        let targets = Targets {
            crates: &crates,
            upstreams: ["boringssl", "qemu"].into(),
            deps: &deps,
        };
        parse(text, &targets)
    }

    fn err(text: &str) -> String {
        format!("{:#}", parsed(text).expect_err("should be rejected"))
    }

    #[test]
    fn crate_providers() {
        let text = "[libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(
            super::crates(&providers),
            [("libnativeloader", "atl-nativeloader")]
        );
    }

    #[test]
    fn upstream_providers() {
        let text = "[libcrypto]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n\
                    [libssl]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(upstreams(&providers), BTreeSet::from(["boringssl"]));
        assert!(
            err("[x]\nprovider = \"upstream:gone\"\nreason = \"r\"\n")
                .contains("no sources.toml upstream")
        );
        assert!(
            err("[x]\nprovider = \"upstream:qemu\"\nreason = \"r\"\n").contains("no build recipe")
        );
        // the recipe installs libcrypto and libssl only
        assert!(
            err("[libcrypto_static]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n")
                .contains("installs no module `libcrypto_static`")
        );
    }

    #[test]
    fn system_providers() {
        let text = "[libEGL]\nprovider = \"system:egl\"\nreason = \"r\"\n\
                    [libGLESv2]\nprovider = \"system:egl\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(
            systems(&providers),
            [("libEGL", "egl"), ("libGLESv2", "egl")]
        );
        assert!(
            err("[x]\nprovider = \"system:gone\"\nreason = \"r\"\n").contains("no deps.toml row")
        );
        assert!(
            err("[x]\nprovider = \"system:git\"\nreason = \"r\"\n").contains("has no pkgconfig")
        );
        assert!(
            err("[x]\nprovider = \"system:zlib\"\nreason = \"r\"\n")
                .contains("lacks `upstream:aosp`")
        );
    }

    #[test]
    fn several_providers() {
        let text =
            "[libEGL]\nprovider = [\"system:egl\", \"crate:atl-nativeloader\"]\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(systems(&providers), [("libEGL", "egl")]);
        assert_eq!(super::crates(&providers), [("libEGL", "atl-nativeloader")]);
        assert!(super::omitted(&providers).is_empty());
        // each one is checked
        assert!(
            err("[x]\nprovider = [\"system:egl\", \"crate:gone\"]\nreason = \"r\"\n")
                .contains("no workspace crate")
        );
        assert!(
            err("[x]\nprovider = [\"system:egl\", \"system:egl\"]\nreason = \"r\"\n")
                .contains("listed twice")
        );
        assert!(
            err("[x]\nprovider = [\"system:egl\", \"none\"]\nreason = \"r\"\n")
                .contains("never one of several")
        );
        assert!(err("[x]\nprovider = []\nreason = \"r\"\n").contains("empty provider list"));
    }

    #[test]
    fn joined_in_row_order() {
        let p = |lib: &str, include: &[&str], defines: &[&str]| Provided {
            libs: vec![PathBuf::from(lib)],
            include: include.iter().map(PathBuf::from).collect(),
            defines: defines.iter().map(|d| (*d).to_owned()).collect(),
        };
        let joined = Provided::join(vec![
            p("/a/liba.so", &["/inc"], &["A=1"]),
            p("/b/libb.so", &["/inc", "/b/inc"], &["A=1", "B"]),
        ]);
        assert_eq!(
            joined.libs,
            [PathBuf::from("/a/liba.so"), PathBuf::from("/b/libb.so")]
        );
        assert_eq!(
            joined.include,
            [PathBuf::from("/inc"), PathBuf::from("/b/inc")]
        );
        assert_eq!(joined.defines, ["A=1", "B"]);
    }

    #[test]
    fn missing_targets() {
        assert!(
            err("[x]\nprovider = \"crate:gone\"\nreason = \"r\"\n").contains("no workspace crate")
        );
    }

    #[test]
    fn none_drops_the_dependency() {
        let text = "[libdl_android]\nprovider = \"none\"\nreason = \"bionic linker API\"\n\
                    [libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(super::omitted(&providers), ["libdl_android"]);
        assert!(err("[x]\nprovider = \"none\"\nreason = \"\"\n").contains("empty reason"));
    }

    #[test]
    fn unknown_kind_and_empty_reason() {
        assert!(err("[x]\nprovider = \"aosp:x\"\nreason = \"r\"\n").contains("is not crate:"));
        assert!(
            err("[x]\nprovider = \"crate:atl-nativeloader\"\nreason = \" \"\n")
                .contains("empty reason")
        );
    }

    #[test]
    fn rust_os_rows() {
        let text = "[librustutils]\nrust_os = \"android\"\nreason = \"r\"\n\
                    [libdl]\nprovider = \"none\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(rust_android(&providers), ["librustutils"]);
        // built from the tree: provided by nothing, dropped by nothing
        assert_eq!(super::omitted(&providers), ["libdl"]);
        assert!(super::partial(&providers).is_empty());
        assert!(super::crates(&providers).is_empty());
        for bad in [
            "provider = \"none\"\n",
            "provider = \"crate:atl-nativeloader\"\n",
            "srcs = [\"a.rs\"]\n",
        ] {
            assert!(
                err(&format!(
                    "[x]\nrust_os = \"android\"\nreason = \"r\"\n{bad}"
                ))
                .contains("no provider or srcs"),
                "{bad}"
            );
        }
        assert!(err("[x]\nrust_os = \"android\"\nreason = \" \"\n").contains("empty reason"));
        // linux_glibc is every other crate's OS, never written
        assert!(
            err("[x]\nrust_os = \"linux_glibc\"\nreason = \"r\"\n").contains("unknown variant")
        );
        assert!(err("[x]\nreason = \"r\"\n").contains("needs rust_os"));
    }

    #[test]
    fn srcs_rows() {
        let text = "[libskia]\nprovider = \"none\"\nreason = \"r\"\nsrcs = [\"src/codec/SkRawCodec.cpp\"]\n\
                    [libandroid_runtime]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\nsrcs = [\"a.cpp\"]\n\
                    [libdl]\nprovider = \"none\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        // a srcs row keeps its module: only the whole-module `none` row drops dependencies
        assert_eq!(super::omitted(&providers), ["libdl"]);
        let partial: Vec<&str> = super::partial(&providers).iter().map(|(m, _)| *m).collect();
        assert_eq!(partial, ["libandroid_runtime", "libskia"]);
        // its crate is still built
        assert_eq!(
            super::crates(&providers),
            [("libandroid_runtime", "atl-nativeloader")]
        );
        let row = |srcs: &str| format!("[x]\nprovider = \"none\"\nreason = \"r\"\nsrcs = {srcs}\n");
        for bad in [
            "[\"\"]",
            "[\"/abs.cpp\"]",
            "[\"../up.cpp\"]",
            "[\"a//b.cpp\"]",
            "[\"./a.cpp\"]",
            "[\"*.cpp\"]",
            "[\":filegroup\"]",
        ] {
            assert!(err(&row(bad)).contains("not a plain path"), "{bad}");
        }
        assert!(err(&row("[\"a.cpp\", \"a.cpp\"]")).contains("listed twice"));
        // the reason stays required on a srcs row
        assert!(
            err("[x]\nprovider = \"none\"\nreason = \"\"\nsrcs = [\"a.cpp\"]\n")
                .contains("empty reason")
        );
    }

    #[test]
    fn module_names() {
        assert!(is_module_name("libc++"));
        assert!(is_module_name("libnativehelper_jvm"));
        assert!(!is_module_name("lib c"));
        assert!(!is_module_name("//art:libart"));
    }
}
