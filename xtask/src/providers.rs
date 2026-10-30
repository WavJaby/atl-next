// SPDX-License-Identifier: MIT
//! `providers.toml`: Android.bp modules provided by our crates or system libraries instead of AOSP
//! source. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::deps::Deps;

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

/// Row key = Android.bp module name.
pub type Providers = BTreeMap<String, Provider>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub provider: String,
    pub reason: String,
}

pub fn load(root: &Path, crates: &BTreeSet<String>, deps: &Deps) -> Result<Providers> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text, crates, deps).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str, crates: &BTreeSet<String>, deps: &Deps) -> Result<Providers> {
    let providers: Providers = toml::from_str(text)?;
    for (module, row) in &providers {
        validate(module, row, crates, deps).with_context(|| format!("[{module}]"))?;
    }
    Ok(providers)
}

fn validate(module: &str, row: &Provider, crates: &BTreeSet<String>, deps: &Deps) -> Result<()> {
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
        _ => bail!(
            "provider `{}` is not crate:<name>, deps:<name> or {NONE}",
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

    fn err(text: &str) -> String {
        let (crates, deps) = fixtures();
        format!(
            "{:#}",
            parse(text, &crates, &deps).expect_err("should be rejected")
        )
    }

    #[test]
    fn crate_and_deps_providers() {
        let (crates, deps) = fixtures();
        let text = "[libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n\
                    [libicuuc]\nprovider = \"deps:icu\"\nreason = \"r\"\n";
        let providers = parse(text, &crates, &deps).unwrap();
        assert_eq!(providers["libicuuc"].provider, "deps:icu");
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
        let (crates, deps) = fixtures();
        let text = "[libdl_android]\nprovider = \"none\"\nreason = \"bionic linker API\"\n\
                    [libicuuc]\nprovider = \"deps:icu\"\nreason = \"r\"\n";
        let providers = parse(text, &crates, &deps).unwrap();
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
