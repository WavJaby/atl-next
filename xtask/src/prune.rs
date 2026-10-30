// SPDX-License-Identifier: MIT
//! `aosp-prune.toml`: directories of the AOSP tree Soong must not see, and Soong's own marker that hides
//! them. Column meanings live in that file's header.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::sources::{self, Sources};

pub const FILE: &str = "aosp-prune.toml";
/// Soong's finder skips a directory holding this file, with everything below it (`ui/build/finder.go`).
const MARKER: &str = ".find-ignore";
/// Tree-root list of the markers `apply` placed, so one whose row is gone gets removed.
const PLACED: &str = ".atl-pruned";

/// Row key = tree path of the pruned directory.
pub type Prune = BTreeMap<String, Row>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub reason: String,
}

pub fn load(root: &Path, sources: &Sources) -> Result<Prune> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text, sources).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str, sources: &Sources) -> Result<Prune> {
    let prune: Prune = toml::from_str(text)?;
    for (dir, row) in &prune {
        ensure!(
            sources::is_tree_path(dir),
            "[{dir}]: not a relative tree path"
        );
        ensure!(
            sources
                .aosp
                .projects
                .keys()
                .any(|p| dir == p || dir.starts_with(&format!("{p}/"))),
            "[{dir}]: not inside a pinned AOSP project"
        );
        ensure!(!row.reason.trim().is_empty(), "[{dir}]: empty reason");
    }
    Ok(prune)
}

/// Put a marker in every pruned directory of `tree`, remove the ones whose row is gone. A pruned
/// directory missing from the checkout is an error: the row no longer matches its upstream.
pub fn apply(tree: &Path, prune: &Prune) -> Result<()> {
    let placed_file = tree.join(PLACED);
    let placed = std::fs::read_to_string(&placed_file).unwrap_or_default();
    for old in placed.lines().filter(|d| !prune.contains_key(*d)) {
        let marker = tree.join(old).join(MARKER);
        if marker.exists() {
            std::fs::remove_file(&marker).with_context(|| marker.display().to_string())?;
        }
    }
    for dir in prune.keys() {
        let path = tree.join(dir);
        ensure!(
            path.is_dir(),
            "{FILE} [{dir}]: no such directory in the checkout"
        );
        let marker = path.join(MARKER);
        if !marker.exists() {
            std::fs::write(&marker, "").with_context(|| marker.display().to_string())?;
        }
    }
    let now: String = prune.keys().map(|d| format!("{d}\n")).collect();
    std::fs::write(&placed_file, now).with_context(|| placed_file.display().to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        toml::from_str(
            "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\n\
             manifest = \"0123456789abcdef0123456789abcdef01234567\"\n\
             [aosp.projects]\n\"system/apex\" = \"0123456789abcdef0123456789abcdef01234567\"\n",
        )
        .unwrap()
    }

    fn err(text: &str) -> String {
        format!(
            "{:#}",
            parse(text, &sources()).expect_err("should be rejected")
        )
    }

    #[test]
    fn rows_inside_pinned_projects() {
        let prune = parse(
            "[\"system/apex/tests\"]\nreason = \"test apexes\"\n",
            &sources(),
        )
        .unwrap();
        assert_eq!(prune["system/apex/tests"].reason, "test apexes");
        assert!(err("[\"system/apexd\"]\nreason = \"r\"\n").contains("not inside a pinned"));
        assert!(err("[\"../x\"]\nreason = \"r\"\n").contains("not a relative tree path"));
        assert!(err("[\"system/apex/tests\"]\nreason = \" \"\n").contains("empty reason"));
    }

    #[test]
    fn markers_follow_the_table() {
        let tree = std::env::temp_dir().join(format!("xtask-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(tree.join("a/tests")).unwrap();
        std::fs::create_dir_all(tree.join("b/tests")).unwrap();
        let row = || Row { reason: "r".into() };
        apply(
            &tree,
            &Prune::from([("a/tests".into(), row()), ("b/tests".into(), row())]),
        )
        .unwrap();
        assert!(tree.join("a/tests/.find-ignore").exists());
        apply(&tree, &Prune::from([("b/tests".into(), row())])).unwrap();
        assert!(!tree.join("a/tests/.find-ignore").exists());
        assert!(tree.join("b/tests/.find-ignore").exists());
        assert!(apply(&tree, &Prune::from([("gone".into(), row())])).is_err());
        std::fs::remove_dir_all(&tree).unwrap();
    }
}
