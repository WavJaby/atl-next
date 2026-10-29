// SPDX-License-Identifier: MIT
//! `licenses.toml`: the licence policy per shipped artifact. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::sources;

pub const FILE: &str = "licenses.toml";
const RUNTIME: &str = "runtime";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Licenses {
    pub artifact: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub upstream: BTreeMap<String, Upstream>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub unlabelled: String,
}

/// One allow entry: an SPDX licence, optionally `WITH` an exception.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Licence<'a> {
    id: &'a str,
    exception: Option<&'a str>,
}

fn parse_licence(entry: &str) -> Result<Licence<'_>> {
    let (id, exception) = match entry.split_once(" WITH ") {
        Some((id, exc)) => (id.trim(), Some(exc.trim())),
        None => (entry.trim(), None),
    };
    let Some(license) = spdx::license_id(id) else {
        bail!("`{id}` is not an SPDX licence id")
    };
    ensure!(!license.is_deprecated(), "`{id}` is a deprecated SPDX id");
    if let Some(exc) = exception {
        ensure!(
            spdx::exception_id(exc).is_some(),
            "`{exc}` is not an SPDX exception id"
        );
    }
    Ok(Licence { id, exception })
}

pub fn load(root: &Path, upstreams: &BTreeSet<String>) -> Result<Licenses> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let licenses = parse(&text, upstreams).with_context(|| format!("in {FILE}"))?;
    let deny = std::fs::read_to_string(root.join("deny.toml")).context("reading deny.toml")?;
    check_deny(&licenses, &deny).context("deny.toml against licenses.toml")?;
    Ok(licenses)
}

fn parse(text: &str, upstreams: &BTreeSet<String>) -> Result<Licenses> {
    let licenses: Licenses = toml::from_str(text)?;
    ensure!(
        licenses.artifact.contains_key(RUNTIME),
        "no [artifact.{RUNTIME}]"
    );
    for (name, artifact) in &licenses.artifact {
        ensure!(
            sources::is_name(name),
            "[artifact.{name}]: name must be [a-z0-9-]"
        );
        let mut seen = BTreeSet::new();
        for entry in &artifact.allow {
            let licence = parse_licence(entry).with_context(|| format!("[artifact.{name}]"))?;
            // CLAUDE.md licence rules: GPL-2.0-only only with a linking exception; no GPL-3 in the runtime
            ensure!(
                !(licence.id == "GPL-2.0-only" && licence.exception.is_none()),
                "[artifact.{name}]: bare GPL-2.0-only; only `GPL-2.0-only WITH <linking exception>`"
            );
            ensure!(
                !(name == RUNTIME
                    && (licence.id.starts_with("GPL-3.0") || licence.id.starts_with("AGPL-3.0"))),
                "[artifact.{RUNTIME}]: `{entry}` -- no GPL-3 family in the runtime"
            );
            ensure!(
                seen.insert(licence),
                "[artifact.{name}]: `{entry}` listed twice"
            );
        }
    }
    for (name, upstream) in &licenses.upstream {
        ensure!(
            upstreams.contains(name),
            "[upstream.{name}]: not a sources.toml upstream"
        );
        let licence =
            parse_licence(&upstream.unlabelled).with_context(|| format!("[upstream.{name}]"))?;
        ensure!(
            licence.exception.is_none(),
            "[upstream.{name}]: unlabelled takes a plain id"
        );
    }
    Ok(licenses)
}

/// Crates link into the runtime, so cargo-deny may allow nothing the runtime does not.
fn check_deny(licenses: &Licenses, deny_text: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Deny {
        licenses: DenyLicenses,
    }
    #[derive(Deserialize)]
    struct DenyLicenses {
        allow: Vec<String>,
    }
    let deny: Deny = toml::from_str(deny_text)?;
    let runtime: BTreeSet<Licence> = licenses.artifact[RUNTIME]
        .allow
        .iter()
        .map(|e| parse_licence(e))
        .collect::<Result<_>>()?;
    for entry in &deny.licenses.allow {
        ensure!(
            runtime.contains(&parse_licence(entry)?),
            "crate licence `{entry}` is not allowed in [artifact.{RUNTIME}]"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstreams() -> BTreeSet<String> {
        ["aosp".to_owned(), "qemu".to_owned()].into()
    }

    fn err(text: &str) -> String {
        format!(
            "{:#}",
            parse(text, &upstreams()).expect_err("should be rejected")
        )
    }

    const RUNTIME_MIT: &str = "[artifact.runtime]\nallow = [\"MIT\"]\n";

    #[test]
    fn runtime_bridge_and_unlabelled() {
        let text = "[artifact.runtime]\nallow = [\"MIT\", \"GPL-2.0-only WITH Classpath-exception-2.0\"]\n\
                    [artifact.qemu-bridge]\nallow = [\"GPL-2.0-or-later\", \"GPL-3.0-or-later\"]\n\
                    [upstream.qemu]\nunlabelled = \"GPL-2.0-or-later\"\n";
        let licenses = parse(text, &upstreams()).unwrap();
        assert_eq!(licenses.upstream["qemu"].unlabelled, "GPL-2.0-or-later");
    }

    #[test]
    fn runtime_required() {
        assert!(
            err("[artifact.qemu-bridge]\nallow = [\"MIT\"]\n").contains("no [artifact.runtime]")
        );
    }

    #[test]
    fn ids_checked_against_spdx_list() {
        assert!(err("[artifact.runtime]\nallow = [\"MITT\"]\n").contains("not an SPDX licence id"));
        // `GPL-2.0` is the deprecated spelling of GPL-2.0-only
        assert!(err("[artifact.runtime]\nallow = [\"GPL-2.0\"]\n").contains("deprecated"));
        let text = "[artifact.runtime]\nallow = [\"MIT WITH No-Such-exception\"]\n";
        assert!(err(text).contains("not an SPDX exception id"));
    }

    #[test]
    fn licence_rules() {
        let bare = "[artifact.qemu-bridge]\nallow = [\"GPL-2.0-only\"]\n";
        assert!(err(&format!("{RUNTIME_MIT}{bare}")).contains("bare GPL-2.0-only"));
        assert!(
            err("[artifact.runtime]\nallow = [\"GPL-3.0-or-later\"]\n").contains("no GPL-3 family")
        );
        assert!(
            err("[artifact.runtime]\nallow = [\"AGPL-3.0-only\"]\n").contains("no GPL-3 family")
        );
    }

    #[test]
    fn duplicates_and_unknown_upstream() {
        assert!(err("[artifact.runtime]\nallow = [\"MIT\", \"MIT\"]\n").contains("listed twice"));
        let text = format!("{RUNTIME_MIT}[upstream.gone]\nunlabelled = \"MIT\"\n");
        assert!(err(&text).contains("not a sources.toml upstream"));
    }

    #[test]
    fn deny_within_runtime() {
        let licenses = parse(RUNTIME_MIT, &upstreams()).unwrap();
        assert!(check_deny(&licenses, "[licenses]\nallow = [\"MIT\"]\n").is_ok());
        let e = format!(
            "{:#}",
            check_deny(&licenses, "[licenses]\nallow = [\"MIT\", \"ISC\"]\n").unwrap_err()
        );
        assert!(e.contains("`ISC` is not allowed"));
    }
}
