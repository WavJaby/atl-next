// SPDX-License-Identifier: MIT
//! `providers.toml`: Android.bp modules provided by our crates, system libraries or upstreams built
//! by their own build system instead of AOSP source. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::deps::Deps;
use crate::upstream;

pub const FILE: &str = "providers.toml";
/// Provider of a module nothing provides on this host: a dependency on it is dropped.
pub const NONE: &str = "none";

/// Modules whose dependencies the converter drops (`provider = "none"`).
pub fn omitted(providers: &Providers) -> Vec<&str> {
    providers
        .iter()
        .filter(|(_, row)| row.provider == NONE)
        .map(|(module, _)| module.as_str())
        .collect()
}

/// Upstreams the `upstream:<name>` rows name (each built once, whichever modules it provides).
pub fn upstreams(providers: &Providers) -> BTreeSet<&str> {
    providers
        .values()
        .filter_map(|row| row.provider.strip_prefix("upstream:"))
        .collect()
}

/// Modules our crates provide (`provider = "crate:<name>"`): (module, crate).
pub fn crates(providers: &Providers) -> Vec<(&str, &str)> {
    providers
        .iter()
        .filter_map(|(module, row)| {
            let krate = row.provider.strip_prefix("crate:")?;
            Some((module.as_str(), krate))
        })
        .collect()
}

/// One module built outside the tree (`crate:` and `upstream:` rows), as `xtask native` hands it to the converter in `<out>/provided.json` (bp2ninja provided.go `Provided`).
#[derive(Debug, Serialize)]
pub struct Provided {
    /// Absolute path of the shared object or archive dependents link.
    pub lib: PathBuf,
    /// Header dirs dependents get as `-isystem`.
    pub include: Vec<PathBuf>,
    /// `NAME=VALUE` defines the headers depend on, given to dependents as `-D`.
    pub defines: Vec<String>,
}

/// Row key = Android.bp module name.
pub type Providers = BTreeMap<String, Provider>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub provider: String,
    pub reason: String,
}

/// What a provider may name: workspace crates, deps.toml rows, sources.toml git upstreams.
pub struct Targets<'a> {
    pub crates: &'a BTreeSet<String>,
    pub deps: &'a Deps,
    pub upstreams: BTreeSet<&'a str>,
}

impl<'a> Targets<'a> {
    pub fn new(
        crates: &'a BTreeSet<String>,
        deps: &'a Deps,
        sources: &'a crate::sources::Sources,
    ) -> Self {
        let upstreams = sources.git.keys().map(String::as_str).collect();
        Self {
            crates,
            deps,
            upstreams,
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
    let (crates, deps) = (targets.crates, targets.deps);
    ensure!(is_module_name(module), "not an Android.bp module name");
    ensure!(!row.reason.trim().is_empty(), "empty reason");
    if row.provider == NONE {
        return Ok(());
    }
    match row.provider.split_once(':') {
        Some(("crate", name)) => ensure!(crates.contains(name), "no workspace crate `{name}`"),
        Some(("deps", name)) => {
            let Some(dep) = deps.get(name) else {
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
        _ => bail!(
            "provider `{}` is not crate:<name>, deps:<name>, upstream:<name> or {NONE}",
            row.provider
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> (BTreeSet<String>, Deps) {
        let crates = ["atl-nativeloader".to_owned()].into();
        let deps = toml::from_str(
            "[icu]\ndebian = \"libicu-dev\"\npkgconfig = \"icu-uc\"\nscope = [\"build\"]\n\
             used_by = [\"upstream:aosp\"]\n\
             [git]\ndebian = \"git\"\nscope = [\"test\"]\nused_by = [\"xtask:check\"]\n\
             [zlib]\ndebian = \"zlib1g-dev\"\npkgconfig = \"zlib\"\nscope = [\"build\"]\n\
             used_by = [\"xtask:check\"]\n",
        )
        .unwrap();
        (crates, deps)
    }

    /// Parses with the fixtures; sources.toml git upstreams `boringssl` (has a recipe) and `qemu` (has none).
    fn parsed(text: &str) -> Result<Providers> {
        let (crates, deps) = fixtures();
        let targets = Targets {
            crates: &crates,
            deps: &deps,
            upstreams: ["boringssl", "qemu"].into(),
        };
        parse(text, &targets)
    }

    fn err(text: &str) -> String {
        format!("{:#}", parsed(text).expect_err("should be rejected"))
    }

    #[test]
    fn crate_and_deps_providers() {
        let text = "[libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n\
                    [libicuuc]\nprovider = \"deps:icu\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(providers["libicuuc"].provider, "deps:icu");
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
    fn missing_targets() {
        assert!(
            err("[x]\nprovider = \"crate:gone\"\nreason = \"r\"\n").contains("no workspace crate")
        );
        assert!(
            err("[x]\nprovider = \"deps:gone\"\nreason = \"r\"\n").contains("no deps.toml row")
        );
    }

    #[test]
    fn deps_target_must_be_aosp_library() {
        // fixture `git` has no pkgconfig; `zlib` has one but is not used by upstream:aosp
        assert!(err("[x]\nprovider = \"deps:git\"\nreason = \"r\"\n").contains("has no pkgconfig"));
        assert!(
            err("[x]\nprovider = \"deps:zlib\"\nreason = \"r\"\n")
                .contains("lacks `upstream:aosp`")
        );
    }

    #[test]
    fn none_drops_the_dependency() {
        let text = "[libdl_android]\nprovider = \"none\"\nreason = \"bionic linker API\"\n\
                    [libicuuc]\nprovider = \"deps:icu\"\nreason = \"r\"\n";
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
    fn module_names() {
        assert!(is_module_name("libc++"));
        assert!(is_module_name("libnativehelper_jvm"));
        assert!(!is_module_name("lib c"));
        assert!(!is_module_name("//art:libart"));
    }
}
