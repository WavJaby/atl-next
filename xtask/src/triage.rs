// SPDX-License-Identifier: MIT
//! Failure triage for `xtask native`: ninja's failed steps grouped by module directory (`<side>/<module>`), each with its cause (compiler `error:` lines, a link's undefined symbols),
//! ranked against the converter's `graph.json` (tools/bp2ninja graph.go): which failed modules each goal reaches, which have no failed dependency (fixing one lets it finish), how many closure modules wait on each.

use std::collections::{BTreeMap, BTreeSet};

/// `graph.json`: goal module directories and each module directory's step dependencies.
#[derive(serde::Deserialize)]
pub struct Graph {
    pub goals: Vec<String>,
    pub deps: BTreeMap<String, Vec<String>>,
}

/// One failed module: its failed steps and their causes.
#[derive(Default, Debug, PartialEq)]
pub struct Failed {
    pub steps: usize,
    /// distinct error messages, most steps first, then first seen
    pub errors: Vec<Cause>,
    /// undefined symbols its links report
    pub undefined: BTreeSet<String>,
}

/// One error message: location-free key, its first line as printed, the steps reporting it.
#[derive(Debug, PartialEq)]
pub struct Cause {
    key: String,
    pub line: String,
    pub steps: usize,
}

/// lld's line per undefined symbol; the `>>> referenced by` lines after it are not causes.
const UNDEFINED: &str = "error: undefined symbol: ";
/// GNU ld's (clang's default linker on Debian): `<obj>: in function `f': … undefined reference to `sym'`.
const UNDEFINED_REF: &str = "undefined reference to `";
/// GNU ld's other errors, which carry no `error:`; the key starts at the phrase.
const LD_ERRORS: &[&str] = &["multiple definition of `", "ld: cannot find "];
/// clang's driver line after any link error: the cause is the linker's own lines.
const LINK_FAILED: &str = "linker command failed";

/// Failed ninja steps by module directory, from `ninja.log` (stdout of `ninja -k 0`: `FAILED: <outputs>`, the command line, its output, then the next `[n/m]` status line).
/// `prefixes` (build dir, tree, each with a trailing `/`) are cut from the module path and from cause lines.
pub fn failures(log: &str, prefixes: &[&str]) -> BTreeMap<String, Failed> {
    let mut by_module: BTreeMap<String, Failed> = BTreeMap::new();
    let mut step: Option<(String, Vec<&str>)> = None;
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        let failed = line.strip_prefix("FAILED: ");
        if (failed.is_some() || is_status(line))
            && let Some((module, out)) = step.take()
        {
            add_step(by_module.entry(module).or_default(), &out, prefixes);
        }
        if let Some(outputs) = failed {
            // ninja >= 1.13 writes `FAILED: [code=N] <outputs>`
            let outputs = match outputs.strip_prefix("[code=") {
                Some(rest) => rest.split_once("] ").map_or(rest, |(_, o)| o),
                None => outputs,
            };
            let first = outputs.split(' ').next().unwrap_or(outputs);
            // the command line, never a cause
            lines.next();
            step = Some((module_dir(strip(first, prefixes)), Vec::new()));
        } else if let Some((_, out)) = &mut step
            && !is_status(line)
        {
            out.push(line);
        }
    }
    if let Some((module, out)) = step {
        add_step(by_module.entry(module).or_default(), &out, prefixes);
    }
    for f in by_module.values_mut() {
        // stable: equal counts keep first-seen order
        f.errors.sort_by_key(|c| std::cmp::Reverse(c.steps));
    }
    by_module
}

/// ninja's `[n/m] description` status line.
fn is_status(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|r| r.split_once("] "))
        .and_then(|(c, _)| c.split_once('/'))
        .is_some_and(|(a, b)| {
            !a.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
        })
}

fn strip<'a>(path: &'a str, prefixes: &[&str]) -> &'a str {
    prefixes
        .iter()
        .find_map(|p| path.strip_prefix(p))
        .unwrap_or(path)
}

/// `obj/<side>/<module>/…`, `gen/<side>/<module>/…`, `<side>/<module>/<artifact>` -> `<side>/<module>`.
fn module_dir(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["obj" | "gen", side, module, ..] | [side, module, ..] => format!("{side}/{module}"),
        _ => rel.to_owned(),
    }
}

/// One step's output into its module: undefined symbols, else `error:` lines (the driver's link line only when nothing else), else the last output line (a script's own message).
fn add_step(f: &mut Failed, out: &[&str], prefixes: &[&str]) {
    f.steps += 1;
    let mut errors: Vec<&str> = Vec::new();
    let mut undefined = false;
    for l in out {
        if let Some((_, sym)) = l.split_once(UNDEFINED) {
            f.undefined.insert(sym.trim().to_owned());
            undefined = true;
        } else if let Some((_, rest)) = l.split_once(UNDEFINED_REF) {
            f.undefined
                .insert(rest.rsplit_once('\'').map_or(rest, |(s, _)| s).to_owned());
            undefined = true;
        } else if l.contains("error:") || LD_ERRORS.iter().any(|p| l.contains(p)) {
            errors.push(l.trim());
        }
    }
    if errors.iter().any(|e| !e.contains(LINK_FAILED)) || undefined {
        errors.retain(|e| !e.contains(LINK_FAILED));
    }
    if errors.is_empty() && !undefined {
        errors.extend(out.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()));
    }
    let mut seen = BTreeSet::new();
    for e in errors {
        let line = prefixes.iter().fold(e.to_owned(), |l, p| l.replace(p, ""));
        // `file:line:col: error: msg` and `file: error: msg` -> `error: msg`: one message in many files is one cause
        let key = std::iter::once("error:")
            .chain(LD_ERRORS.iter().copied())
            .find_map(|p| line.find(p))
            .map_or(line.as_str(), |i| &line[i..])
            .to_owned();
        if !seen.insert(key.clone()) {
            continue;
        }
        match f.errors.iter_mut().find(|c| c.key == key) {
            Some(c) => c.steps += 1,
            None => f.errors.push(Cause {
                key,
                line,
                steps: 1,
            }),
        }
    }
}

/// A failed module placed against the goals.
#[derive(Debug, PartialEq)]
pub struct Ranked {
    pub module: String,
    /// closure modules that depend on it, transitively
    pub blocks: usize,
    /// failed modules among its own dependencies
    pub waits_on: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct GoalPath {
    pub goal: String,
    pub closure: usize,
    pub failed: usize,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub goals: Vec<GoalPath>,
    /// failed modules on a goal path, those with no failed dependency first, then by modules blocked
    pub ranked: Vec<Ranked>,
    /// failed modules no goal reaches (or the graph does not know)
    pub off_path: Vec<String>,
}

fn closure<'a>(graph: &'a Graph, from: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(m) = stack.pop() {
        if seen.insert(m) {
            stack.extend(graph.deps.get(m).into_iter().flatten().map(String::as_str));
        }
    }
    seen
}

pub fn rank(graph: &Graph, failed: &BTreeMap<String, Failed>) -> Report {
    let mut on_path: BTreeSet<&str> = BTreeSet::new();
    let goals = graph
        .goals
        .iter()
        .map(|g| {
            let c = closure(graph, g);
            on_path.extend(&c);
            GoalPath {
                goal: g.clone(),
                closure: c.len(),
                failed: failed.keys().filter(|f| c.contains(f.as_str())).count(),
            }
        })
        .collect();
    let closures: BTreeMap<&str, BTreeSet<&str>> =
        on_path.iter().map(|&m| (m, closure(graph, m))).collect();
    let mut ranked: Vec<Ranked> = failed
        .keys()
        .filter_map(|f| {
            let own = closures.get(f.as_str())?;
            Some(Ranked {
                module: f.clone(),
                blocks: closures
                    .iter()
                    .filter(|(m, c)| **m != f.as_str() && c.contains(f.as_str()))
                    .count(),
                waits_on: own
                    .iter()
                    .filter(|d| **d != f.as_str() && failed.contains_key(**d))
                    .map(|d| (*d).to_owned())
                    .collect(),
            })
        })
        .collect();
    ranked.sort_by(|a, b| {
        (
            !a.waits_on.is_empty(),
            std::cmp::Reverse(a.blocks),
            &a.module,
        )
            .cmp(&(
                !b.waits_on.is_empty(),
                std::cmp::Reverse(b.blocks),
                &b.module,
            ))
    });
    let off_path = failed
        .keys()
        .filter(|f| !on_path.contains(f.as_str()))
        .cloned()
        .collect();
    Report {
        goals,
        ranked,
        off_path,
    }
}

/// Names and cause lines printed per module by default: one screen for a few dozen failed modules; `--verbose` prints all.
const ERRORS_PER_MODULE: usize = 2;
const NAMES_SHOWN: usize = 6;

/// The summary lines, without the `native: ` prefix.
pub fn render(report: &Report, failed: &BTreeMap<String, Failed>, verbose: bool) -> Vec<String> {
    let cap = |n: usize| if verbose { usize::MAX } else { n };
    let names = |list: &mut dyn ExactSizeIterator<Item = &str>| {
        let total = list.len();
        let shown: Vec<&str> = list.take(cap(NAMES_SHOWN)).collect();
        match total - shown.len() {
            0 => shown.join(", "),
            more => format!("{}, +{more} more", shown.join(", ")),
        }
    };
    let mut out = Vec::new();
    for g in &report.goals {
        out.push(format!(
            "goal {}: {} failed modules on its path (closure {} modules)",
            g.goal, g.failed, g.closure
        ));
    }
    let module = |out: &mut Vec<String>, m: &str, head: String| {
        let f = &failed[m];
        out.push(format!("  {m}: {} failed{head}", f.steps));
        if !f.undefined.is_empty() {
            out.push(format!(
                "    undefined symbols ({}): {}",
                f.undefined.len(),
                names(&mut f.undefined.iter().map(String::as_str))
            ));
        }
        for c in f.errors.iter().take(cap(ERRORS_PER_MODULE)) {
            let steps = if c.steps > 1 {
                format!(" [{} steps]", c.steps)
            } else {
                String::new()
            };
            out.push(format!("    {}{steps}", c.line));
        }
        if f.errors.len() > cap(ERRORS_PER_MODULE) {
            out.push(format!(
                "    +{} more messages (--verbose)",
                f.errors.len() - ERRORS_PER_MODULE
            ));
        }
    };
    let (first, then): (Vec<&Ranked>, Vec<&Ranked>) =
        report.ranked.iter().partition(|r| r.waits_on.is_empty());
    if !first.is_empty() {
        out.push("fix first (no failed dependency), most blocking first:".to_owned());
        for r in first {
            module(&mut out, &r.module, format!(", blocks {}", r.blocks));
        }
    }
    if !then.is_empty() {
        out.push("then (a failed dependency too):".to_owned());
        for r in then {
            let waits = names(&mut r.waits_on.iter().map(String::as_str));
            module(
                &mut out,
                &r.module,
                format!(", blocks {}, waits on {waits}", r.blocks),
            );
        }
    }
    if !report.off_path.is_empty() {
        out.push("on no goal path:".to_owned());
        for m in &report.off_path {
            module(&mut out, m, String::new());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
[1/9] cc a.cpp
FAILED: /o/obj/device/liblog/shared/x/a.cpp.o
clang++ -c /t/x/a.cpp -o /o/obj/device/liblog/shared/x/a.cpp.o
In file included from /t/x/a.cpp:1:
/t/x/h.h:3:2: error: unknown type name 'size_t'
    3 |  size_t n;
1 error generated.
[2/9] cc b.cpp
FAILED: /o/obj/device/liblog/static/x/b.cpp.o
clang++ -c /t/x/b.cpp
/t/x/b.cpp:9:1: error: unknown type name 'size_t'
/t/x/b.cpp:12:1: error: use of undeclared identifier 'ioctl'
FAILED: [code=1] /o/device/libbinder/libbinder.so
clang++ -shared -o /o/device/libbinder/libbinder.so
ld.lld: error: undefined symbol: f()
>>> referenced by p.cpp
ld.lld: error: undefined symbol: g
ld.lld: error: undefined symbol: f()
clang++: error: linker command failed with exit code 1 (use -v to see invocation)
FAILED: /o/gen/host/stats/out.h /o/gen/host/stats/out.cpp
cd /t && /bin/bash /o/gen/host/stats/genrule.sh
Traceback (most recent call last):
ModuleNotFoundError: No module named 'x'
[3/9] link c
ninja: build stopped: subcommands failed.
";

    #[test]
    fn causes_by_module() {
        let f = failures(LOG, &["/o/", "/t/"]);
        assert_eq!(
            f.keys().map(String::as_str).collect::<Vec<_>>(),
            ["device/libbinder", "device/liblog", "host/stats"]
        );
        let log = &f["device/liblog"];
        assert_eq!(log.steps, 2);
        // one message in two files and two steps is one cause; the build prefixes are cut
        assert_eq!(
            log.errors,
            [
                Cause {
                    key: "error: unknown type name 'size_t'".to_owned(),
                    line: "x/h.h:3:2: error: unknown type name 'size_t'".to_owned(),
                    steps: 2
                },
                Cause {
                    key: "error: use of undeclared identifier 'ioctl'".to_owned(),
                    line: "x/b.cpp:12:1: error: use of undeclared identifier 'ioctl'".to_owned(),
                    steps: 1
                },
            ]
        );
        // a link: its symbols, not the driver's line
        let link = &f["device/libbinder"];
        assert_eq!(
            link.undefined,
            BTreeSet::from(["f()".to_owned(), "g".to_owned()])
        );
        assert!(link.errors.is_empty());
        // no `error:` line: the script's last line; ninja's own closing line is not step output
        assert_eq!(
            f["host/stats"].errors[0].line,
            "ModuleNotFoundError: No module named 'x'"
        );
    }

    /// GNU ld's wording, as in the libz link of 2026-10-31 (`--no-undefined`, Debian's default linker).
    #[test]
    fn gnu_ld_causes() {
        let log = "\
FAILED: /o/device/libz/libz.so
clang++ -shared -o /o/device/libz/libz.so
/usr/bin/ld: /o/obj/device/libz/shared/adler32.c.o: in function `adler32_z':
adler32.c:(.text+0xea): undefined reference to `cpu_check_features'
/usr/bin/ld: /o/obj/device/libz/shared/b.o:(.bss+0x0): multiple definition of `x'; /o/obj/device/libz/shared/a.o:(.bss+0x0): first defined here
/usr/bin/ld: cannot find -lfoo: No such file or directory
clang++: error: linker command failed with exit code 1 (use -v to see invocation)
";
        let f = &failures(log, &["/o/"])["device/libz"];
        assert_eq!(
            f.undefined,
            BTreeSet::from(["cpu_check_features".to_owned()])
        );
        assert_eq!(
            f.errors.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
            [
                "multiple definition of `x'; obj/device/libz/shared/a.o:(.bss+0x0): first defined here",
                "ld: cannot find -lfoo: No such file or directory"
            ]
        );
    }

    #[test]
    fn driver_line_when_alone() {
        let log = "FAILED: /o/device/a/a.so\ncmd\nclang++: error: linker command failed with exit code 1\n";
        assert_eq!(
            failures(log, &["/o/"])["device/a"].errors[0].line,
            "clang++: error: linker command failed with exit code 1"
        );
    }

    fn graph(goals: &[&str], deps: &[(&str, &[&str])]) -> Graph {
        Graph {
            goals: goals.iter().map(|g| (*g).to_owned()).collect(),
            deps: deps
                .iter()
                .map(|(m, d)| ((*m).to_owned(), d.iter().map(|x| (*x).to_owned()).collect()))
                .collect(),
        }
    }

    fn failed(names: &[&str]) -> BTreeMap<String, Failed> {
        names
            .iter()
            .map(|n| {
                (
                    (*n).to_owned(),
                    Failed {
                        steps: 1,
                        ..Failed::default()
                    },
                )
            })
            .collect()
    }

    /// goal -> mid -> {leaf, tool}; goal -> other -> leaf; stray is in no closure.
    #[test]
    fn ranked_against_the_goal() {
        let g = graph(
            &["d/goal"],
            &[
                ("d/goal", &["d/mid", "d/other"]),
                ("d/mid", &["d/leaf", "h/tool"]),
                ("d/other", &["d/leaf"]),
                ("d/leaf", &[]),
                ("h/tool", &[]),
                ("d/stray", &["d/leaf"]),
            ],
        );
        let r = rank(
            &g,
            &failed(&["d/mid", "d/leaf", "h/tool", "d/stray", "x/java"]),
        );
        assert_eq!(
            r.goals,
            [GoalPath {
                goal: "d/goal".to_owned(),
                closure: 5,
                failed: 3
            }]
        );
        // leaf: mid, other, goal wait on it (stray is outside the closure); tool: mid, goal
        assert_eq!(
            r.ranked,
            [
                Ranked {
                    module: "d/leaf".to_owned(),
                    blocks: 3,
                    waits_on: vec![]
                },
                Ranked {
                    module: "h/tool".to_owned(),
                    blocks: 2,
                    waits_on: vec![]
                },
                Ranked {
                    module: "d/mid".to_owned(),
                    blocks: 1,
                    waits_on: vec!["d/leaf".to_owned(), "h/tool".to_owned()]
                },
            ]
        );
        assert_eq!(r.off_path, ["d/stray", "x/java"]);
    }

    #[test]
    fn render_caps_lists() {
        let mut f = failures(LOG, &["/o/", "/t/"]);
        let syms = f.get_mut("device/libbinder").expect("in LOG");
        syms.undefined = (0..NAMES_SHOWN + 2).map(|i| format!("s{i}")).collect();
        let g = graph(
            &["device/libbinder"],
            &[("device/libbinder", &["device/liblog"])],
        );
        let r = rank(&g, &f);
        let text = render(&r, &f, false);
        assert_eq!(
            text[..5],
            [
                "goal device/libbinder: 2 failed modules on its path (closure 2 modules)",
                "fix first (no failed dependency), most blocking first:",
                "  device/liblog: 2 failed, blocks 1",
                "    x/h.h:3:2: error: unknown type name 'size_t' [2 steps]",
                "    x/b.cpp:12:1: error: use of undeclared identifier 'ioctl'",
            ]
        );
        assert_eq!(
            text[5..8],
            [
                "then (a failed dependency too):",
                "  device/libbinder: 1 failed, blocks 0, waits on device/liblog",
                "    undefined symbols (8): s0, s1, s2, s3, s4, s5, +2 more",
            ]
        );
        assert_eq!(
            text[8..],
            [
                "on no goal path:",
                "  host/stats: 1 failed",
                "    ModuleNotFoundError: No module named 'x'"
            ]
        );
        let all = render(&r, &f, true);
        assert!(
            all.contains(&"    undefined symbols (8): s0, s1, s2, s3, s4, s5, s6, s7".to_owned())
        );
    }
}
