// SPDX-License-Identifier: MIT
//! `aosp-prune.toml`: directories of the AOSP tree the converter must not scan, and the marker that hides
//! them. `xtask native` places the markers before every round. Column meanings live in that file's header.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::sources::{self, Sources};

pub const FILE: &str = "aosp-prune.toml";
/// A directory holding this file is skipped with everything below it: bp2ninja's tree scan (`tools/bp2ninja/bp.go` discover) and Soong's finder (`ui/build/finder.go`).
pub const MARKER: &str = ".find-ignore";
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
            sources.aosp.projects.keys().any(|p| is_under(dir, p)),
            "[{dir}]: not inside a pinned AOSP project"
        );
        if let Some((project, kept)) = dropped_by(dir, sources) {
            bail!(
                "[{dir}]: sources.toml [aosp.sparse] `{project}` drops it (keeps {}), so no checkout holds it: delete the row or widen the keep-set",
                kept.join(", ")
            );
        }
        ensure!(!row.reason.trim().is_empty(), "[{dir}]: empty reason");
    }
    Ok(prune)
}

/// The sparse project and keep-set that leave `dir` out of the checkout, if any. Owner = deepest pinned project: AOSP projects nest, and an inner one is its own checkout whatever the outer keeps.
/// A dir above a kept one survives (cone mode keeps the path down to it), so both directions count as kept.
fn dropped_by<'a>(dir: &str, sources: &'a Sources) -> Option<(&'a str, &'a [String])> {
    let (project, _) = sources
        .aosp
        .projects
        .iter()
        .filter(|(p, _)| is_under(dir, p))
        .max_by_key(|(p, _)| p.len())?;
    let kept = sources.aosp.sparse.get(project)?;
    let inside = dir.strip_prefix(&format!("{project}/"))?;
    let held = kept
        .iter()
        .any(|k| is_under(inside, k) || is_under(k, inside));
    (!held).then_some((project.as_str(), kept.as_slice()))
}

/// `path` is `dir` or below it, by whole path components.
fn is_under(path: &str, dir: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
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

    /// The fa0a06d class: `guest` is what the Virtualization keep-set drops, the row that once reached `native` before failing.
    #[test]
    fn rows_the_keep_set_drops_are_refused() {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let sources: Sources = toml::from_str(&format!(
            "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\nmanifest = \"{SHA}\"\n\
             [aosp.projects]\n\"m/Virt\" = \"{SHA}\"\n\"m/Virt/guest/inner\" = \"{SHA}\"\n\
             [aosp.sparse]\n\"m/Virt\" = [\"build\", \"libs/a\"]\n"
        ))
        .unwrap();
        let row = |dir: &str| parse(&format!("[\"{dir}\"]\nreason = \"r\"\n"), &sources);
        let refused = format!("{:#}", row("m/Virt/guest").expect_err("dropped dir"));
        assert!(refused.contains("[m/Virt/guest]"), "{refused}");
        assert!(
            refused.contains("[aosp.sparse] `m/Virt` drops it"),
            "{refused}"
        );
        // a sibling sharing a kept dir's name prefix is not inside it
        assert!(row("m/Virt/build2").is_err());
        for kept in [
            "m/Virt",             // the project root is always there
            "m/Virt/build",       // a kept dir
            "m/Virt/build/micro", // below one
            "m/Virt/libs",        // above one
            "m/Virt/guest/inner", // a nested project: its own checkout
        ] {
            assert!(row(kept).is_ok(), "{kept}");
        }
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
