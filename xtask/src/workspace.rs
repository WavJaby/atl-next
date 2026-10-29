// SPDX-License-Identifier: MIT
//! The cargo workspace as `cargo metadata` resolves it: member crates and the edges between them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

pub struct Workspace {
    pub crates: BTreeSet<String>,
    /// (dependent, dependency) between member crates, every dependency kind and target.
    pub edges: BTreeSet<(String, String)>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    dependencies: Vec<String>,
}

pub fn load(root: &Path) -> Result<Workspace> {
    let out = crate::cargo_command()
        .args(["metadata", "--offline", "--locked", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    ensure!(
        out.status.success(),
        "cargo metadata: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Metadata = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
    Ok(from_metadata(meta))
}

fn from_metadata(meta: Metadata) -> Workspace {
    let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
    let names: BTreeMap<&str, &str> = meta
        .packages
        .iter()
        .filter(|p| members.contains(p.id.as_str()))
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let edges = meta
        .resolve
        .nodes
        .iter()
        .filter_map(|n| names.get(n.id.as_str()).map(|from| (from, n)))
        .flat_map(|(from, n)| {
            n.dependencies
                .iter()
                .filter_map(|d| names.get(d.as_str()))
                .map(move |to| ((*from).to_owned(), (*to).to_owned()))
        })
        .collect();
    Workspace {
        crates: names.values().map(|n| (*n).to_owned()).collect(),
        edges,
    }
}
