// SPDX-License-Identifier: MIT
//! The cargo workspace as `cargo metadata` resolves it: member crates, the edges between them, and which members run a build script.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::unit_use::{self, Echo, Units};

#[derive(Default)]
pub struct Workspace {
    pub crates: BTreeSet<String>,
    /// (dependent, dependency) between member crates, every dependency kind and target.
    pub edges: BTreeSet<(String, String)>,
    /// The subset of [`Self::edges`] a build of the dependent compiles: normal or build dependencies, not dev-only.
    pub build_edges: BTreeSet<(String, String)>,
    /// Members with a build script (`custom-build` target).
    pub build_scripts: BTreeSet<String>,
}

impl Workspace {
    /// Members a build of `root` compiles, `root` included: the closure over [`Self::build_edges`].
    pub fn build_closure(&self, root: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::from([root.to_owned()]);
        let mut todo = vec![root.to_owned()];
        while let Some(from) = todo.pop() {
            let deps: Vec<String> = self
                .build_edges
                .range((from.clone(), String::new())..)
                .take_while(|(f, _)| *f == from)
                .map(|(_, to)| to.clone())
                .collect();
            for to in deps {
                if seen.insert(to.clone()) {
                    todo.push(to);
                }
            }
        }
        seen
    }
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
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
struct NodeDep {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    /// null = normal, "dev", "build"
    kind: Option<String>,
}

pub fn load(root: &Path) -> Result<Workspace> {
    let mut cmd = crate::cargo_command();
    cmd.args(["metadata", "--offline", "--locked", "--format-version", "1"])
        .current_dir(root);
    let out =
        unit_use::run(&mut cmd, Units::Plain, Echo::Nothing).context("running cargo metadata")?;
    ensure!(out.status.success(), "cargo metadata: {}", out.stderr);
    let meta: Metadata =
        serde_json::from_slice(out.stdout.as_bytes()).context("parsing cargo metadata")?;
    Ok(from_metadata(meta))
}

fn from_metadata(meta: Metadata) -> Workspace {
    let members: BTreeSet<&str> = meta.workspace_members.iter().map(String::as_str).collect();
    let member_pkgs: Vec<&Package> = meta
        .packages
        .iter()
        .filter(|p| members.contains(p.id.as_str()))
        .collect();
    let names: BTreeMap<&str, &str> = member_pkgs
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let mut ws = Workspace {
        crates: names.values().map(|n| (*n).to_owned()).collect(),
        build_scripts: member_pkgs
            .iter()
            .filter(|p| {
                p.targets
                    .iter()
                    .any(|t| t.kind.iter().any(|k| k == "custom-build"))
            })
            .map(|p| p.name.clone())
            .collect(),
        ..Workspace::default()
    };
    for n in &meta.resolve.nodes {
        let Some(from) = names.get(n.id.as_str()) else {
            continue;
        };
        for d in &n.deps {
            let Some(to) = names.get(d.pkg.as_str()) else {
                continue;
            };
            let edge = ((*from).to_owned(), (*to).to_owned());
            if d.dep_kinds.iter().any(|k| k.kind.as_deref() != Some("dev")) {
                ws.build_edges.insert(edge.clone());
            }
            ws.edges.insert(edge);
        }
    }
    ws
}
