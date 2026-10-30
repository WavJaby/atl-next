// SPDX-License-Identifier: MIT
//! What Soong itself recorded as missing for the goals: the per-variant missing dependencies of its ninja
//! error rules, limited to the goals' dependency closure in its `json-module-graph`. Exact where scraping
//! a build log is not: a header library's dropped dependency surfaces in its users only as a missing
//! header file, yet Soong recorded it on the header library (`libart_headers` -> `dlmalloc`), and a
//! module outside the closure (`libarttools_binder_utils` -> `libbinder_ndk`) is not pulled in.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One module variant of `json-module-graph` (other fields ignored).
#[derive(Deserialize)]
struct Node {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Variant")]
    variant: String,
    #[serde(rename = "Deps", default)]
    deps: Vec<Edge>,
}

#[derive(Deserialize)]
struct Edge {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Variant")]
    variant: String,
}

type Key = (String, String);

/// Which variants of a module a goal builds: Soong's phony `<name>` covers all, `<name>-host` the
/// build-OS ones (`linux_*`; not `windows_*`, which is host-cross), `<name>-target` the device ones.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    All,
    Host,
    Target,
}

impl Side {
    fn admits(self, variant: &str) -> bool {
        match self {
            Side::All => true,
            Side::Host => variant.starts_with("linux_"),
            Side::Target => variant.starts_with("android"),
        }
    }
}

/// Module name and side of a goal as `xtask soong` takes it; a name the graph knows as is wins over
/// reading a `-host`/`-target` suffix (modules like `conscrypt-host` exist).
fn goal_module<'a>(goal: &'a str, known: &HashSet<&str>) -> (&'a str, Side) {
    if let Some((_, name)) = goal.strip_prefix("//").and_then(|r| r.split_once(':')) {
        return (name, Side::All);
    }
    if known.contains(goal) {
        return (goal, Side::All);
    }
    if let Some(name) = goal.strip_suffix("-host") {
        return (name, Side::Host);
    }
    if let Some(name) = goal.strip_suffix("-target") {
        return (name, Side::Target);
    }
    (goal, Side::All)
}

/// Module variants reachable from the goals.
fn closure(nodes: &[Node], goals: &[String]) -> HashSet<Key> {
    let by_key: HashMap<(&str, &str), &Node> = nodes
        .iter()
        .map(|n| ((n.name.as_str(), n.variant.as_str()), n))
        .collect();
    let known: HashSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    let mut seen: HashSet<Key> = HashSet::new();
    let mut todo: VecDeque<&Node> = VecDeque::new();
    for goal in goals {
        let (name, side) = goal_module(goal, &known);
        for n in nodes
            .iter()
            .filter(|n| n.name == name && side.admits(&n.variant))
        {
            if seen.insert((n.name.clone(), n.variant.clone())) {
                todo.push_back(n);
            }
        }
    }
    while let Some(n) = todo.pop_front() {
        for e in &n.deps {
            if let Some(dep) = by_key.get(&(e.name.as_str(), e.variant.as_str()))
                && seen.insert((e.name.clone(), e.variant.clone()))
            {
                todo.push_back(dep);
            }
        }
    }
    seen
}

/// `(module, variant) -> missing list` from the error rules of Soong's ninja files. A rule reads
///     description = ${m.<module>_<variant>.moduleDesc}…
///     tags = module_name=<module>;…
///     error = module <module> missing dependencies: <a, b>$
/// (the trailing `$` escapes the message's newline). A namespaced module's `m.` name carries a prefix.
fn error_rules(ninja: &str) -> BTreeMap<Key, String> {
    let mut out = BTreeMap::new();
    let mut desc = "";
    for line in ninja.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("description = ${m.") {
            desc = rest.split_once(".moduleDesc}").map_or("", |(m, _)| m);
        } else if let Some(rest) = line.strip_prefix("error = module ")
            && let Some((module, list)) = rest.split_once(" missing dependencies: ")
        {
            let variant = desc
                .find(&format!("{module}_"))
                .map_or("", |i| &desc[i + module.len() + 1..]);
            out.insert(
                (module.to_owned(), variant.to_owned()),
                list.trim_end_matches('$').trim().to_owned(),
            );
        }
    }
    out
}

/// `module X missing dependencies: …` lines (the form a ninja error rule prints) for every module
/// variant in the goals' closure that Soong recorded as missing something.
pub fn missing_lines(
    graph_json: &Path,
    soong_out: &Path,
    product: &str,
    goals: &[String],
) -> Result<Vec<String>> {
    let text = std::fs::read(graph_json).with_context(|| graph_json.display().to_string())?;
    let nodes: Vec<Node> =
        serde_json::from_slice(&text).with_context(|| graph_json.display().to_string())?;
    let reach = closure(&nodes, goals);
    let prefix = format!("build.{product}");
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(soong_out)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !(name.starts_with(&prefix) && name.ends_with(".ninja")) {
            continue;
        }
        let ninja = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        for ((module, variant), list) in error_rules(&ninja) {
            if reach.contains(&(module.clone(), variant.clone())) {
                lines.push(format!(
                    "module {module} ({variant}) missing dependencies: {list}"
                ));
            }
        }
    }
    lines.sort();
    lines.dedup();
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, variant: &str, deps: &[(&str, &str)]) -> Node {
        Node {
            name: name.into(),
            variant: variant.into(),
            deps: deps
                .iter()
                .map(|(n, v)| Edge {
                    name: (*n).into(),
                    variant: (*v).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn closure_follows_variants_and_sides() {
        // measured shape: libart-runtime host static -> libart_headers host; the header library lost
        // dlmalloc; libarttools_binder_utils is outside the closure
        let nodes = vec![
            node(
                "dalvikvm",
                "linux_glibc_x86_64",
                &[("libart-runtime", "linux_glibc_x86_64_static")],
            ),
            node(
                "dalvikvm",
                "windows_x86_64",
                &[("libwin", "windows_x86_64")],
            ),
            node(
                "dalvikvm",
                "android_x86_64",
                &[("libdev", "android_x86_64")],
            ),
            node(
                "libart-runtime",
                "linux_glibc_x86_64_static",
                &[("libart_headers", "linux_glibc_x86_64")],
            ),
            node("libart_headers", "linux_glibc_x86_64", &[]),
            node("libarttools_binder_utils", "linux_glibc_x86_64", &[]),
            node("conscrypt-host", "linux_glibc_common", &[]),
        ];
        let reach = closure(&nodes, &["dalvikvm-host".into()]);
        let has = |n: &str, v: &str| reach.contains(&(n.to_owned(), v.to_owned()));
        assert!(has("libart_headers", "linux_glibc_x86_64"));
        assert!(!has("libarttools_binder_utils", "linux_glibc_x86_64"));
        assert!(!has("dalvikvm", "windows_x86_64") && !has("dalvikvm", "android_x86_64"));
        // a module whose name ends in -host is a name, not a side
        assert!(
            closure(&nodes, &["conscrypt-host".into()])
                .contains(&("conscrypt-host".into(), "linux_glibc_common".into()))
        );
        assert!(
            closure(&nodes, &["dalvikvm-target".into()])
                .contains(&("dalvikvm".into(), "android_x86_64".into()))
        );
    }

    #[test]
    fn error_rules_with_variant() {
        // as written in build.<product>.N.ninja (namespaced module: `m.1-…` prefix)
        let ninja = "\
build /o/x.a: g.android.Error
    description = ${m.libart_headers_linux_glibc_x86_64.moduleDesc}static link libart_headers.a${m.libart_headers_linux_glibc_x86_64.moduleDescSuffix}
    tags = module_name=libart_headers;module_type=cc_library_headers;rule_name=Error
    error = module libart_headers missing dependencies: dlmalloc$
build /o/y: g.android.Error
    description = ${m.1-libnb_android_native_bridge_arm64_armv8-a_shared.moduleDesc}link
    error = module libnb missing dependencies: libscudo, gwp_asan$
";
        let rules = error_rules(ninja);
        assert_eq!(
            rules[&("libart_headers".into(), "linux_glibc_x86_64".into())],
            "dlmalloc"
        );
        assert_eq!(
            rules[&(
                "libnb".into(),
                "android_native_bridge_arm64_armv8-a_shared".into()
            )],
            "libscudo, gwp_asan"
        );
    }
}
