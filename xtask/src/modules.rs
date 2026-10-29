// SPDX-License-Identifier: MIT
//! `modules.toml`: architecture modules, checked for equality against the real crate graph. Column
//! meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::sources;
use crate::workspace::Workspace;

pub const FILE: &str = "modules.toml";

pub type Modules = BTreeMap<String, Module>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub crates: Vec<String>,
    #[serde(default)]
    pub exports: Vec<String>,
    #[serde(default)]
    pub uses: Vec<String>,
    #[serde(default)]
    pub direct_link: BTreeMap<String, DirectLink>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectLink {
    pub reason: String,
}

pub fn load(root: &Path, workspace: &Workspace) -> Result<Modules> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text, workspace).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str, workspace: &Workspace) -> Result<Modules> {
    let modules: Modules = toml::from_str(text)?;
    let owner = membership(&modules, workspace)?;
    compare_edges(&modules, workspace, &owner)?;
    if let Some(cycle) = find_cycle(&modules) {
        bail!("module cycle: {}", cycle.join(" -> "));
    }
    Ok(modules)
}

/// Crate -> module, after checking every workspace crate is in exactly one module and every
/// declaration names something that exists.
fn membership<'a>(modules: &'a Modules, ws: &Workspace) -> Result<BTreeMap<&'a str, &'a str>> {
    let mut owner = BTreeMap::new();
    for (name, m) in modules {
        ensure!(
            sources::is_name(name),
            "[{name}]: name must be [a-z0-9-], starting alphanumeric"
        );
        for c in &m.crates {
            ensure!(ws.crates.contains(c), "[{name}]: no workspace crate `{c}`");
            if let Some(other) = owner.insert(c.as_str(), name.as_str()) {
                bail!("crate `{c}` is in both [{other}] and [{name}]");
            }
        }
        for e in &m.exports {
            ensure!(
                m.crates.contains(e),
                "[{name}]: export `{e}` is not one of its crates"
            );
        }
        for target in m.uses.iter().chain(m.direct_link.keys()) {
            ensure!(
                target != name && modules.contains_key(target),
                "[{name}]: `{target}` is not another module"
            );
        }
        for (target, link) in &m.direct_link {
            ensure!(
                !link.reason.trim().is_empty(),
                "[{name}.direct_link.{target}]: empty reason"
            );
        }
    }
    let unassigned: Vec<&String> = ws
        .crates
        .iter()
        .filter(|c| !owner.contains_key(c.as_str()))
        .collect();
    ensure!(unassigned.is_empty(), "crates in no module: {unassigned:?}");
    Ok(owner)
}

/// Every cross-module crate edge must be declared, and every declaration must carry an edge.
fn compare_edges(modules: &Modules, ws: &Workspace, owner: &BTreeMap<&str, &str>) -> Result<()> {
    let mut used_uses = BTreeSet::new();
    let mut used_links = BTreeSet::new();
    let mut problems = Vec::new();
    for (from, to) in &ws.edges {
        // membership() has assigned every workspace crate
        let (a, b) = (owner[from.as_str()], owner[to.as_str()]);
        if a == b {
            continue;
        }
        let m = &modules[a];
        if m.uses.iter().any(|u| u == b) && modules[b].exports.contains(to) {
            used_uses.insert((a, b));
        } else if m.direct_link.contains_key(b) {
            used_links.insert((a, b));
        } else {
            problems.push(format!(
                "{from} -> {to}: [{a}] needs `uses = [\"{b}\"]` with `{to}` in [{b}] exports, or a direct_link"
            ));
        }
    }
    for (name, m) in modules {
        for u in &m.uses {
            if !used_uses.contains(&(name.as_str(), u.as_str())) {
                problems.push(format!(
                    "[{name}] uses `{u}`, but no crate edge goes through it"
                ));
            }
        }
        for t in m.direct_link.keys() {
            if !used_links.contains(&(name.as_str(), t.as_str())) {
                problems.push(format!("[{name}] direct_link `{t}` covers no crate edge"));
            }
        }
    }
    ensure!(problems.is_empty(), "{}", problems.join("\n  "));
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    OnStack,
    Done,
}

/// A cycle in the module graph (`uses` + `direct_link`), as `a -> b -> a`.
fn find_cycle(modules: &Modules) -> Option<Vec<String>> {
    fn visit<'a>(
        node: &'a str,
        modules: &'a Modules,
        marks: &mut BTreeMap<&'a str, Mark>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return None,
            Some(Mark::OnStack) => {
                let start = stack.iter().position(|s| *s == node)?;
                let mut cycle: Vec<String> =
                    stack[start..].iter().map(|s| (*s).to_owned()).collect();
                cycle.push(node.to_owned());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(node, Mark::OnStack);
        stack.push(node);
        let m = &modules[node];
        for next in m.uses.iter().chain(m.direct_link.keys()) {
            if let Some(cycle) = visit(next, modules, marks, stack) {
                return Some(cycle);
            }
        }
        stack.pop();
        marks.insert(node, Mark::Done);
        None
    }
    let mut marks = BTreeMap::new();
    modules
        .keys()
        .find_map(|name| visit(name, modules, &mut marks, &mut Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(crates: &[&str], edges: &[(&str, &str)]) -> Workspace {
        Workspace {
            crates: crates.iter().map(|c| (*c).to_owned()).collect(),
            edges: edges
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        }
    }

    fn err(text: &str, ws: &Workspace) -> String {
        format!("{:#}", parse(text, ws).expect_err("should be rejected"))
    }

    const AB: &str =
        "[a]\ncrates = [\"a1\"]\nuses = [\"b\"]\n[b]\ncrates = [\"b1\"]\nexports = [\"b1\"]\n";

    #[test]
    fn declared_edge_through_export() {
        assert!(parse(AB, &ws(&["a1", "b1"], &[("a1", "b1")])).is_ok());
    }

    #[test]
    fn undeclared_edge() {
        let text = "[a]\ncrates = [\"a1\"]\n[b]\ncrates = [\"b1\"]\nexports = [\"b1\"]\n";
        assert!(err(text, &ws(&["a1", "b1"], &[("a1", "b1")])).contains("a1 -> b1"));
    }

    #[test]
    fn non_exported_target_needs_direct_link() {
        let text = "[a]\ncrates = [\"a1\"]\nuses = [\"b\"]\n[b]\ncrates = [\"b1\", \"b2\"]\nexports = [\"b1\"]\n";
        let w = ws(&["a1", "b1", "b2"], &[("a1", "b1"), ("a1", "b2")]);
        assert!(err(text, &w).contains("a1 -> b2"));
        let linked = format!("{text}[a.direct_link.b]\nreason = \"zero-copy\"\n");
        assert!(parse(&linked, &w).is_ok());
    }

    #[test]
    fn stale_declarations() {
        assert!(err(AB, &ws(&["a1", "b1"], &[])).contains("no crate edge goes through it"));
        let text = format!("{AB}[a.direct_link.b]\nreason = \"r\"\n");
        let e = err(&text, &ws(&["a1", "b1"], &[("a1", "b1")]));
        assert!(e.contains("direct_link `b` covers no crate edge"));
    }

    #[test]
    fn membership_rules() {
        assert!(err(AB, &ws(&["a1", "b1", "c1"], &[("a1", "b1")])).contains("crates in no module"));
        assert!(err(AB, &ws(&["a1"], &[])).contains("no workspace crate `b1`"));
        let twice = "[a]\ncrates = [\"x\"]\n[b]\ncrates = [\"x\"]\n";
        assert!(err(twice, &ws(&["x"], &[])).contains("in both [a] and [b]"));
        let bad_export = "[a]\ncrates = [\"a1\"]\nexports = [\"b1\"]\n";
        assert!(err(bad_export, &ws(&["a1"], &[])).contains("not one of its crates"));
    }

    #[test]
    fn edge_inside_a_module_needs_nothing() {
        let text = "[a]\ncrates = [\"a1\", \"a2\"]\n";
        assert!(parse(text, &ws(&["a1", "a2"], &[("a1", "a2")])).is_ok());
    }

    #[test]
    fn module_cycle() {
        // acyclic crate graph a1 -> b1 -> a2, cyclic module graph a -> b -> a
        let text = "[a]\ncrates = [\"a1\", \"a2\"]\nexports = [\"a2\"]\nuses = [\"b\"]\n\
                    [b]\ncrates = [\"b1\"]\nexports = [\"b1\"]\nuses = [\"a\"]\n";
        let w = ws(&["a1", "a2", "b1"], &[("a1", "b1"), ("b1", "a2")]);
        assert!(err(text, &w).contains("module cycle: a -> b -> a"));
    }
}
