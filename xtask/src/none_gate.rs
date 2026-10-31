// SPDX-License-Identifier: MIT
//! The none gate, part of `xtask native`'s table check: a providers.toml `none` row drops every edge to its
//! module, which is valid only while the modules naming it need nothing from it (bionic modules whose API
//! glibc covers) or are compiled out too. A dependent still built that fails to find a header the
//! dropped module would have exported shows the row hiding a user: a table problem, not a build failure.
//!
//! Evidence is the compiler's: a failed module's `'<h>' file not found`, where the module or one in its
//! dependency closure dropped a `none` module (graph.json `dropped`) and `<h>` exists under one of that
//! module's export directories (`none_exports`, tree-relative). So an include the preprocessor skips
//! (`#if defined(__BIONIC__)`) is no finding, and a compile stopping at an earlier fatal error hides a
//! later include until that one is fixed.

use std::collections::BTreeMap;
use std::path::Path;

use crate::triage::{self, Failed, Graph};

/// One `none` row a still-built module needs.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The providers.toml row (module name).
    pub row: String,
    /// The failed module directory.
    pub module: String,
    /// The include name not found.
    pub header: String,
    /// The compiler's line.
    pub cause: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "providers.toml [{}] is none, but {} still compiles a user of its <{}>: {}",
            self.row, self.module, self.header, self.cause
        )
    }
}

/// clang's `fatal error: '<h>' file not found` -> `<h>`.
fn missing_header(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once("error: '")?;
    Some(rest.split_once("' file not found")?.0)
}

/// Every finding over the failed modules, sorted, one per (row, module, header).
pub fn check(graph: &Graph, failed: &BTreeMap<String, Failed>, tree: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for (module, f) in failed {
        let headers: Vec<(&str, &str)> = f
            .errors
            .iter()
            .filter_map(|c| Some((missing_header(&c.line)?, c.line.as_str())))
            .collect();
        if headers.is_empty() {
            continue;
        }
        for dir in triage::closure(graph, module) {
            let side = dir.split('/').next().unwrap_or_default();
            for row in graph.dropped.get(dir).into_iter().flatten() {
                let exports = graph.none_exports.get(&format!("{side}/{row}"));
                for (header, cause) in &headers {
                    if exports
                        .into_iter()
                        .flatten()
                        .any(|d| tree.join(d).join(header).is_file())
                    {
                        out.push(Finding {
                            row: row.clone(),
                            module: module.clone(),
                            header: (*header).to_owned(),
                            cause: (*cause).to_owned(),
                        });
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| (&a.row, &a.module, &a.header) == (&b.row, &b.module, &b.header));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree where `none` module libnone exports `n/include` holding `vnd/linker.h`.
    fn tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("atl-none-gate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("n/include/vnd")).unwrap();
        std::fs::write(dir.join("n/include/vnd/linker.h"), "").unwrap();
        dir
    }

    /// device/libui dropped libnone itself; device/libapp reaches it through device/libmid; device/libother
    /// dropped nothing.
    fn graph() -> Graph {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        Graph {
            goals: s(&["device/libapp"]),
            deps: [
                ("device/libapp", s(&["device/libmid"])),
                ("device/libmid", s(&[])),
                ("device/libui", s(&[])),
                ("device/libother", s(&[])),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
            dropped: [
                ("device/libui".to_owned(), s(&["libnone"])),
                ("device/libmid".to_owned(), s(&["libnone"])),
            ]
            .into(),
            none_exports: [("device/libnone".to_owned(), s(&["n/include"]))].into(),
        }
    }

    #[test]
    fn missing_headers_of_none_rows() {
        let log = "\
FAILED: /o/obj/device/libui/G.cpp.o
cmd
/t/ui/G.cpp:29:10: fatal error: 'vnd/linker.h' file not found
[2/9] next
FAILED: /o/obj/device/libapp/a.cpp.o
cmd
/t/app/a.cpp:3:10: fatal error: 'vnd/linker.h' file not found
[3/9] next
FAILED: /o/obj/device/libother/o.cpp.o
cmd
/t/o.cpp:1:10: fatal error: 'vnd/linker.h' file not found
[4/9] next
FAILED: /o/obj/device/libmid/m.cpp.o
cmd
/t/m.cpp:1:10: fatal error: 'elsewhere.h' file not found
[5/9] next
";
        let tree = tree();
        let failed = triage::failures(log, &["/o/", "/t/"]);
        let found = check(&graph(), &failed, &tree);
        let rows: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|f| (f.row.as_str(), f.module.as_str(), f.header.as_str()))
            .collect();
        // libui dropped it, libapp's closure did; libother never named it; libmid's header is not libnone's
        assert_eq!(
            rows,
            [
                ("libnone", "device/libapp", "vnd/linker.h"),
                ("libnone", "device/libui", "vnd/linker.h"),
            ]
        );
        assert_eq!(
            found[1].to_string(),
            "providers.toml [libnone] is none, but device/libui still compiles a user of its <vnd/linker.h>: ui/G.cpp:29:10: fatal error: 'vnd/linker.h' file not found"
        );
        std::fs::remove_dir_all(&tree).unwrap();
    }
}
