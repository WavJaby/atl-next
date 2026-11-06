// SPDX-License-Identifier: MIT
//! `services.toml`: the Rust system services and the AOSP code each mirrors (column meanings in that file's header).
//! `load` parses and checks what the file alone decides; `check` (step `tables` of `xtask check`) checks rows against their real sources: the names atl-system-services registers, the Rust files,
//! and every mirrored path + declaration in the pinned AOSP commit. `upgrade_diff` (`xtask upgrade-diff`) prints the churn of each mirrored file/declaration between two AOSP revisions.
//! Declaration match is textual (no Java parser): good enough to catch a moved file, a renamed or deleted method, and to size a method's churn; it can be fooled by a declaration inside a comment block.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::git::{git, git_raw};
use crate::sources::{Aosp, is_tree_path};

pub const FILE: &str = "services.toml";
/// The binary that registers every service with servicemanager: the source of the row set.
pub const REGISTRAR: &str = "crates/atl-services/src/bin/system_services.rs";
/// Where a `module::CONST` service name in REGISTRAR is defined: `<dir>/<module>.rs`.
const SERVICES_SRC: &str = "crates/atl-services/src";

/// Row key = servicemanager name.
pub type Table = BTreeMap<String, Row>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub src: Vec<String>,
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One `mirrors` entry: an AOSP tree path and optionally a declared name in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry<'a> {
    pub path: &'a str,
    pub name: Option<&'a str>,
}

impl Entry<'_> {
    /// Upper-case first letter = a type (Java naming convention), else a method.
    fn is_type(&self) -> bool {
        self.name
            .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_uppercase()))
    }
}

pub fn parse_entry(s: &str) -> Result<Entry<'_>> {
    let (path, name) = match s.split_once('#') {
        Some((p, n)) => (p, Some(n)),
        None => (s, None),
    };
    ensure!(is_tree_path(path), "{s}: not an AOSP tree path");
    if let Some(n) = name {
        ensure!(is_ident(n), "{s}: `{n}` after `#` is not a Java identifier");
    }
    Ok(Entry { path, name })
}

fn is_ident(s: &str) -> bool {
    s.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$')
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

pub fn load(root: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

pub fn parse(text: &str) -> Result<Table> {
    let table: Table = toml::from_str(text)?;
    for (name, row) in &table {
        ensure!(!row.src.is_empty(), "[{name}]: `src` is empty");
        ensure!(
            row.mirrors.is_empty() == row.reason.is_some(),
            "[{name}]: `reason` is required exactly when `mirrors` is empty"
        );
        let mut seen = BTreeSet::new();
        for m in &row.mirrors {
            parse_entry(m).with_context(|| format!("[{name}]"))?;
            ensure!(seen.insert(m), "[{name}]: {m} listed twice");
        }
    }
    Ok(table)
}

/// The servicemanager names REGISTRAR registers: the first argument of each `add_service(`, a `const NAME: &str = "..."` of that file or `module::NAME` of SERVICES_SRC/module.rs.
pub fn registered(root: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(root.join(REGISTRAR))
        .with_context(|| format!("reading {REGISTRAR}"))?;
    let mut names = BTreeSet::new();
    for (i, _) in text.match_indices("add_service(") {
        let rest = &text[i + "add_service(".len()..];
        let arg = rest.split(',').next().unwrap_or("").trim();
        let name = match arg.rsplit_once("::") {
            Some((module, konst)) => {
                let file = format!("{SERVICES_SRC}/{module}.rs");
                let src = std::fs::read_to_string(root.join(&file))
                    .with_context(|| format!("reading {file} for {arg}"))?;
                const_str(&src, konst).with_context(|| format!("{arg}: no &str const in {file}"))?
            }
            None => const_str(&text, arg)
                .with_context(|| format!("{REGISTRAR}: `{arg}` is not a &str const there"))?,
        };
        names.insert(name);
    }
    ensure!(!names.is_empty(), "{REGISTRAR}: no add_service call found");
    Ok(names)
}

/// The value of `const <name>: &str = "<value>";` in `src`.
fn const_str(src: &str, name: &str) -> Option<String> {
    let decl = format!("const {name}: &str = \"");
    let at = src.find(&decl)? + decl.len();
    Some(src[at..].split('"').next()?.to_owned())
}

/// The AOSP project holding tree path `path`: the longest `[aosp.projects]` key that is `path` or a directory prefix of it; returns (project, path inside it).
fn project_of<'a>(aosp: &'a Aosp, path: &'a str) -> Option<(&'a str, &'a str)> {
    aosp.projects
        .keys()
        .filter_map(|p| {
            path.strip_prefix(p.as_str())
                .and_then(|r| r.strip_prefix('/'))
                .map(|r| (p.as_str(), r))
        })
        .max_by_key(|(p, _)| p.len())
}

/// Step `tables`: the row set equals the registered names, every `src` exists, every mirror path and declaration exists at the pin. `repo_of` maps a project tree path to its git checkout.
pub fn check(
    root: &Path,
    table: &Table,
    aosp: &Aosp,
    repo_of: &dyn Fn(&str) -> PathBuf,
) -> Result<usize> {
    let names = registered(root)?;
    let rows: BTreeSet<String> = table.keys().cloned().collect();
    let missing: Vec<_> = names.difference(&rows).collect();
    let extra: Vec<_> = rows.difference(&names).collect();
    ensure!(
        missing.is_empty() && extra.is_empty(),
        "{FILE} vs {REGISTRAR}: services without a row {missing:?}, rows naming no registered service {extra:?}"
    );
    let mut errors = Vec::new();
    let mut files: BTreeMap<&str, Result<String, String>> = BTreeMap::new();
    let mut entries = 0;
    for (service, row) in table {
        for s in &row.src {
            if !root.join(s).is_file() {
                errors.push(format!("[{service}] src {s}: no such file"));
            }
        }
        for m in &row.mirrors {
            entries += 1;
            let e = parse_entry(m)?;
            let text = files.entry(e.path).or_insert_with(|| {
                let (project, rel) = project_of(aosp, e.path)
                    .ok_or_else(|| "not in any [aosp.projects] project".to_owned())?;
                let commit = &aosp.projects[project];
                git_raw(&repo_of(project), &["show", &format!("{commit}:{rel}")])
                    .map_err(|err| format!("not at the pin {project}@{commit}: {err:#}"))
            });
            match (text, e.name) {
                (Err(why), _) => errors.push(format!("[{service}] {m}: {why}")),
                (Ok(_), None) => {}
                (Ok(t), Some(n)) => {
                    if declarations(t, n, e.is_type()).is_empty() {
                        let kind = if e.is_type() { "type" } else { "method" };
                        errors.push(format!(
                            "[{service}] {m}: no {kind} `{n}` declared in the file"
                        ));
                    }
                }
            }
        }
    }
    if !errors.is_empty() {
        bail!(
            "{} broken mirrors:\n  {}",
            errors.len(),
            errors.join("\n  ")
        );
    }
    Ok(entries)
}

/// Java keywords that can stand right before `name(` without it being a declaration.
const NOT_A_TYPE: &[&str] = &["return", "new", "throw", "else", "case", "yield", "assert"];

/// Line indices where `name` is declared: a type `class|interface|enum|record name`, or a method `<modifiers/annotations> <type> name(` (call sites, `x.name(` and `= name(` do not count).
pub fn declarations(text: &str, name: &str, is_type: bool) -> Vec<usize> {
    let mut found = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let l = line.trim_start();
        if l.starts_with("//") || l.starts_with('*') || l.starts_with("/*") {
            continue;
        }
        let hit = if is_type {
            ["class ", "interface ", "enum ", "record "]
                .iter()
                .any(|kw| {
                    l.match_indices(&format!("{kw}{name}")).any(|(at, m)| {
                        let before_ok = at == 0 || !is_ident_char(l[..at].chars().last());
                        before_ok && !is_ident_char(l[at + m.len()..].chars().next())
                    })
                })
        } else {
            l.match_indices(&format!("{name}(")).any(|(at, _)| {
                let mut before = &l[..at];
                // a C++ out-of-line definition `<type> Class::Inner::name(`: the qualifiers stand between the type and the name
                while let Some(q) = before.strip_suffix("::") {
                    let class = q.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
                    if class.len() == q.len() {
                        return false;
                    }
                    before = class;
                }
                // the declared name follows whitespace, after a type token that is not a keyword
                if !before.ends_with([' ', '\t']) || before.contains('=') {
                    return false;
                }
                let last = before.split_whitespace().last().unwrap_or("");
                !last.is_empty()
                    && !NOT_A_TYPE.contains(&last)
                    && last
                        .chars()
                        .all(|c| c.is_alphanumeric() || "_$<>[],.?".contains(c))
                    && last.ends_with(|c: char| c.is_alphanumeric() || "_$>]".contains(c))
            })
        };
        if hit {
            found.push(i);
        }
    }
    found
}

fn is_ident_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// The body lines of every declaration of `name` (overloads concatenated): from the declaration line to its closing brace; a declaration ending in `;` (abstract) is its line alone.
pub fn bodies(text: &str, name: &str, is_type: bool) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for start in declarations(text, name, is_type) {
        let mut depth = 0i64;
        let mut opened = false;
        let mut end = start;
        'scan: for (j, line) in lines.iter().enumerate().skip(start) {
            end = j;
            let mut chars = line.chars().peekable();
            let mut quote: Option<char> = None;
            while let Some(c) = chars.next() {
                match (quote, c) {
                    (Some(_), '\\') => {
                        chars.next();
                    }
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(c),
                    (None, '/') if chars.peek() == Some(&'/') => break,
                    (None, '{') => {
                        depth += 1;
                        opened = true;
                    }
                    (None, '}') => {
                        depth -= 1;
                        if opened && depth == 0 {
                            break 'scan;
                        }
                    }
                    (None, ';') if !opened => break 'scan,
                    _ => {}
                }
            }
        }
        out.extend(lines[start..=end].iter().map(|l| (*l).to_owned()));
    }
    out
}

/// Lines added + deleted between `a` and `b` (LCS line diff).
pub fn churn(a: &[String], b: &[String]) -> (usize, usize) {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if a[i] == b[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    let common = dp[0] as usize;
    (m - common, n - common)
}

#[derive(clap::Args)]
pub struct Args {
    /// The old AOSP revision (tag or commit), e.g. android-15.0.0_r1.
    pub old: String,
    /// The new AOSP revision, e.g. android-16.0.0_r1.
    pub new: String,
    /// `<project tree path>=<git dir>`: read that project from another repository holding both revisions (repeatable). Default: the project's checkout under src/aosp, which is shallow and holds only the pinned tag.
    #[arg(long = "repo", value_parser = parse_repo)]
    pub repos: Vec<(String, PathBuf)>,
}

fn parse_repo(s: &str) -> Result<(String, PathBuf), String> {
    let (p, d) = s
        .split_once('=')
        .ok_or_else(|| format!("{s}: expected <tree path>=<git dir>"))?;
    Ok((p.to_owned(), PathBuf::from(d)))
}

/// `xtask upgrade-diff`: churn per mirrored entry, per service, most churn first. Read-only: a revision missing from a repository fails with the fetch command it needs.
pub fn upgrade_diff(
    root: &Path,
    args: &Args,
    default_repo: &dyn Fn(&str) -> PathBuf,
) -> Result<()> {
    let table = load(root)?;
    let sources = crate::sources::load(root)?;
    let overrides: BTreeMap<&str, &Path> = args
        .repos
        .iter()
        .map(|(p, d)| (p.as_str(), d.as_path()))
        .collect();
    let mut checked: BTreeSet<&str> = BTreeSet::new();
    let mut cache: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
    let mut report: Vec<(usize, String, Vec<String>)> = Vec::new();
    for (service, row) in &table {
        let mut lines = Vec::new();
        let mut total = 0;
        for m in &row.mirrors {
            let e = parse_entry(m)?;
            let (project, rel) = project_of(&sources.aosp, e.path)
                .with_context(|| format!("{m}: not in any [aosp.projects] project"))?;
            let repo = overrides
                .get(project)
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| default_repo(project));
            if checked.insert(project) {
                for rev in [&args.old, &args.new] {
                    if git(
                        Some(&repo),
                        &[
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("{rev}^{{commit}}"),
                        ],
                    )
                    .is_err()
                    {
                        bail!(
                            "{rev} is not in {} ({project}). Fetch it there (blobless keeps it small): git -C {} fetch --filter=blob:none --depth=1 origin tag {rev}; or pass --repo {project}=<git dir holding both>",
                            repo.display(),
                            repo.display()
                        );
                    }
                }
            }
            let mut read = |rev: &str| -> Option<String> {
                cache
                    .entry((rev.to_owned(), e.path.to_owned()))
                    .or_insert_with(|| git_raw(&repo, &["show", &format!("{rev}:{rel}")]).ok())
                    .clone()
            };
            let (old, new) = (read(&args.old), read(&args.new));
            let pick = |t: &Option<String>| -> Option<Vec<String>> {
                let t = t.as_ref()?;
                let v = match e.name {
                    None => t.lines().map(str::to_owned).collect(),
                    Some(n) => bodies(t, n, e.is_type()),
                };
                (!v.is_empty()).then_some(v)
            };
            let line = match (pick(&old), pick(&new)) {
                (Some(a), Some(b)) => {
                    let (add, del) = churn(&a, &b);
                    total += add + del;
                    format!(
                        "  +{add:<5} -{del:<5} {:>5} -> {:<5} lines  {m}",
                        a.len(),
                        b.len()
                    )
                }
                (None, Some(b)) => {
                    total += b.len();
                    format!("  new in {}: {} lines  {m}", args.new, b.len())
                }
                (Some(_), None) => format!("  GONE in {}  {m}", args.new),
                (None, None) => format!("  absent in both  {m}"),
            };
            lines.push(line);
        }
        report.push((total, service.clone(), lines));
    }
    report.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    println!(
        "upgrade-diff {} -> {}: churn = lines added + deleted per mirrored entry",
        args.old, args.new
    );
    for (total, service, lines) in report {
        println!("{service}: {total}");
        for l in lines {
            println!("{l}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const JAVA: &str = r#"
class Outer {
    // void commented(int x) {
    @Override
    public final ParceledListSlice<Info> query(String a,
            int b) {
        if (x) { return query2(a); }
        String s = "}";
        return null;
    }
    private static class Inner { }
    abstract void abs(int a);
    void caller() { query(a, b); this.query(a); int y = query(a); return query(a); }
}
"#;

    #[test]
    fn declarations_skip_calls_and_comments() {
        assert_eq!(declarations(JAVA, "query", false), vec![4]);
        assert!(declarations(JAVA, "commented", false).is_empty());
        assert_eq!(declarations(JAVA, "Inner", true), vec![10]);
        assert!(declarations(JAVA, "Inne", true).is_empty());
        assert_eq!(declarations(JAVA, "Outer", true), vec![1]);
    }

    #[test]
    fn cpp_out_of_line_definitions_count() {
        let cpp = "status_t SensorService::SensorEventConnection::enableDisable(\n    return mService->enable(this);\n    err = SensorService::enable(x);\nVector<Sensor> SensorService::getSensorList(const String16& p) {\n";
        assert_eq!(declarations(cpp, "enableDisable", false), vec![0]);
        assert_eq!(declarations(cpp, "getSensorList", false), vec![3]);
        // calls, qualified or not, are not declarations
        assert!(declarations(cpp, "enable", false).is_empty());
    }

    #[test]
    fn bodies_end_at_the_matching_brace() {
        // lines 4..=9: the declaration through `}`; the `}` inside a string does not close it
        assert_eq!(bodies(JAVA, "query", false).len(), 6);
        assert_eq!(
            bodies(JAVA, "abs", false),
            vec!["    abstract void abs(int a);"]
        );
    }

    #[test]
    fn churn_counts_added_and_deleted_lines() {
        let v = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        // one line changed (1 deleted + 1 added), one appended
        assert_eq!(
            churn(&v(&["a", "b", "c"]), &v(&["a", "x", "c", "d"])),
            (2, 1)
        );
    }

    #[test]
    fn rows_need_a_reason_exactly_when_empty() {
        assert!(parse("[a]\nsrc = [\"x\"]\nmirrors = []\n").is_err());
        assert!(parse("[a]\nsrc = [\"x\"]\nmirrors = [\"p/q.java#m\"]\nreason = \"r\"\n").is_err());
        assert!(parse("[a]\nsrc = [\"x\"]\nmirrors = []\nreason = \"r\"\n").is_ok());
        assert!(parse("[a]\nsrc = [\"x\"]\nmirrors = [\"p/q.java#1m\"]\n").is_err());
    }
}
