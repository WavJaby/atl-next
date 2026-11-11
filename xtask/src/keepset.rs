// SPDX-License-Identifier: MIT
//! `cargo xtask keepset`: each `[aosp.sparse]` keep-set derived from the build graphs, and the rows checked against it.
//!
//! A read is a tree path a graph names: every token of `<out>/build.ninja` (inputs, implicit and order-only inputs, `cp =` bindings such as a javac classpath), the header reads of `ninja -t deps`, the strings of `graph.json` (include dirs), the `Android.bp` of every module a graph or a `cts.toml` row names, and the `<os>/bin` dir of each host row's `soong_prebuilt_os` (`arches.toml`). Reads come from every out dir given (default: each `out/slot-<n>` with a build.ninja).
//! Cone mode keeps whole dirs: a project's keep-set is the minimal cover of its reads by non-nested dirs. A dir read keeps itself, a file read keeps its parent dir; root files are always kept. Dir or file is decided on disk.
//! `--check` (step `keepset` of `xtask check`) fails when a read falls outside its row. A row dir no read needs is listed, not failed. `--write --project P` writes the derived set; a checkout changes only through `xtask fetch --recreate`.
//! Limits: a header read exists only for an edge some deps log has built, so an unbuilt edge contributes its include dirs from graph.json instead. The converter parses every `Android.bp` of the tree; only the ones a graph names are reads here, and the gates build what a dropped one would break.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Index};
use crate::git::git;
use crate::sources::{self, Aosp, Sources};
use crate::{arches, cts_table};

/// Out dirs hold `slot-<n>/` (slot.rs); a graph is one with a build.ninja.
const OUT: &str = "out";
const NINJA: &str = "build.ninja";
const GRAPH: &str = "graph.json";
/// The project of the prebuilt build-only tools: `arches.toml` `soong_prebuilt_os` names `<project>/<os>/bin`.
const BUILD_TOOLS: &str = "prebuilts/build-tools";

#[derive(clap::Args)]
pub struct Args {
    /// Read this out dir, `out/slot-<n>` (repeatable). Default: every slot with a build.ninja.
    #[arg(long = "slot", value_name = "N")]
    slots: Vec<u32>,
    /// Fail (exit 1) when a read falls outside its [aosp.sparse] row. Read-only.
    #[arg(long, conflicts_with = "write")]
    check: bool,
    /// Write the derived keep-set of each --project into sources.toml [aosp.sparse] (creates or replaces the row). Changes no checkout.
    #[arg(long, requires = "projects")]
    write: bool,
    /// Pinned AOSP project (tree path, repeatable): shown in full, or written with --write.
    #[arg(long = "project", value_name = "PATH")]
    projects: Vec<String>,
}

/// One read a project owns. `path` is project-relative ("" = the project root itself); `keep` is the dir its keep-set must hold ("" = none, a root file); `from` names the graph or table that read it.
#[derive(Debug)]
pub struct Need {
    path: String,
    keep: String,
    from: BTreeSet<String>,
}

/// The reads of the graphs, classified by owning pinned project.
pub struct Derived {
    /// Labels of the out dirs read, e.g. `slot-3`.
    graphs: Vec<String>,
    owned: BTreeMap<String, Vec<Need>>,
    /// Tree paths no pinned project owns (an unpinned project, or a file outside every project).
    unowned: Vec<String>,
    /// Reads absent from their project's HEAD tree: a graph older than the pin, or a path a link or copy creates.
    missing: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    for p in &args.projects {
        ensure!(
            sources.aosp.projects.contains_key(p),
            "--project {p}: not a pinned [aosp.projects] path"
        );
    }
    let derived = derive(root, &layout, &sources, &args.slots)?;
    if args.write {
        write(root, &derived, &args.projects)?;
        return Ok(ExitCode::SUCCESS);
    }
    let failures = show(&layout, &sources, &derived, &args.projects);
    Ok(if args.check && failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Step `keepset` of `xtask check`: every row covers the reads of every workspace graph. `Some(why)` (skipped) without one.
pub fn check_workspace(root: &Path) -> Result<Option<String>> {
    let layout = Layout::of(root)?;
    if graphs(layout.workspace(), &[])?.is_empty() {
        return Ok(Some(
            "no out/slot-<n>/build.ninja in the workspace: run `cargo xtask native` first".into(),
        ));
    }
    let sources = sources::load(root)?;
    let derived = derive(root, &layout, &sources, &[])?;
    let bad = uncovered(&sources, &derived);
    ensure!(
        bad.is_empty(),
        "[aosp.sparse] rows miss reads of the build graphs (cargo xtask keepset --check):\n  {}",
        bad.join("\n  ")
    );
    Ok(None)
}

/// `(label, out dir)` of each graph: the `--slot` values, or every `out/slot-<n>` holding a build.ninja.
fn graphs(ws: &Path, slots: &[u32]) -> Result<Vec<(String, PathBuf)>> {
    let out = ws.join(OUT);
    let mut found = Vec::new();
    if slots.is_empty() {
        if !out.is_dir() {
            return Ok(found);
        }
        for entry in fs::read_dir(&out).with_context(|| format!("reading {}", out.display()))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(n) = slot_number(&name)
                && out.join(&name).join(NINJA).is_file()
            {
                found.push((format!("slot-{n}"), out.join(&name)));
            }
        }
        found.sort();
    } else {
        for n in slots {
            let dir = out.join(format!("slot-{n}"));
            ensure!(
                dir.join(NINJA).is_file(),
                "--slot {n}: no {} (build the slot first)",
                dir.join(NINJA).display()
            );
            found.push((format!("slot-{n}"), dir));
        }
    }
    Ok(found)
}

fn slot_number(name: &str) -> Option<u32> {
    name.strip_prefix("slot-")?.parse().ok()
}

/// One independent scan of one graph: [`derive`] runs them all in parallel, each into its own [`Reads`], merged after.
#[derive(Clone, Copy)]
enum Scan {
    Ninja,
    Deps,
    Json,
}

/// Reads of the graphs and tables, classified. Per graph, build.ninja, its deps log and graph.json are scanned in parallel (each single-threaded and independent; serial they were ~3/4 of the step).
fn derive(root: &Path, layout: &Layout, sources: &Sources, slots: &[u32]) -> Result<Derived> {
    let ws = layout.workspace();
    let prefixes = Prefixes::new(ws);
    let outs = graphs(ws, slots)?;
    let units: Vec<(&str, &Path, Scan)> = outs
        .iter()
        .flat_map(|(label, out)| {
            [Scan::Ninja, Scan::Deps, Scan::Json].map(|s| (label.as_str(), out.as_path(), s))
        })
        .collect();
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let scanned = crate::par::map(&units, workers, |&(label, out, scan)| {
        let mut reads = Reads::default();
        let mut modules = BTreeSet::new();
        match scan {
            Scan::Ninja => scan_ninja(
                &out.join(NINJA),
                &prefixes,
                &format!("{label}/{NINJA}"),
                &mut reads,
            )?,
            Scan::Deps => scan_deps(out, &prefixes, &format!("{label}/deps"), &mut reads)?,
            Scan::Json => {
                let text = fs::read_to_string(out.join(GRAPH))
                    .with_context(|| format!("reading {}", out.join(GRAPH).display()))?;
                let json: serde_json::Value = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {GRAPH} of {label}"))?;
                scan_json(&json, &prefixes, &format!("{label}/{GRAPH}"), &mut reads);
                modules = json_modules(&json);
            }
        }
        Ok::<_, anyhow::Error>((reads, modules))
    });
    let mut reads = Reads::default();
    let mut graph_modules = BTreeSet::new();
    for r in scanned {
        let (part, modules) = r?;
        reads.merge(part);
        graph_modules.extend(modules);
    }
    let index = Index::load(root, &sources.aosp)?;
    let projects: Vec<&str> = sources.aosp.projects.keys().map(String::as_str).collect();
    for name in &graph_modules {
        for file in module_files(&index, name, &projects) {
            reads.add(&file, "android.bp (graph module)");
        }
    }
    for (name, row) in cts_table::load(root)? {
        for module in std::iter::once(name).chain(row.apps) {
            for file in module_files(&index, &module, &projects) {
                reads.add(&file, "cts.toml");
            }
        }
    }
    for arch in arches::load(root)?.values() {
        if let Some(os) = arch
            .host
            .as_ref()
            .and_then(|h| h.soong_prebuilt_os.as_deref())
        {
            reads.add(&format!("{BUILD_TOOLS}/{os}/bin"), "arches.toml");
        }
    }
    let mut derived = Derived {
        graphs: outs.iter().map(|(label, _)| label.clone()).collect(),
        owned: BTreeMap::new(),
        unowned: Vec::new(),
        missing: Vec::new(),
    };
    // dir or file, and presence, come from the owning checkout's HEAD tree, so a sparse checkout classifies the same as a full one
    let mut trees = Trees::new(layout.src().join("aosp"));
    let links = manifest_links(root, &sources.aosp)?;
    for (rel, from) in &reads.0 {
        match locate(&mut trees, &projects, &links, rel)? {
            Some((project, path, is_dir)) => {
                let keep = if is_dir { path.as_str() } else { parent(&path) };
                derived.owned.entry(project).or_default().push(Need {
                    keep: keep.to_owned(),
                    path,
                    from: from.clone(),
                });
            }
            None if owner(rel, &projects).is_none() => derived.unowned.push(rel.clone()),
            None => derived.missing.push(rel.clone()),
        }
    }
    Ok(derived)
}

/// Each checkout's HEAD tree, listed once on first use.
struct Trees {
    root: PathBuf,
    cache: BTreeMap<String, BTreeMap<String, bool>>,
}

impl Trees {
    fn new(root: PathBuf) -> Self {
        Trees {
            root,
            cache: BTreeMap::new(),
        }
    }

    fn of(&mut self, project: &str) -> Result<&BTreeMap<String, bool>> {
        if !self.cache.contains_key(project) {
            let listing = project_tree(&self.root.join(project))?;
            self.cache.insert(project.to_owned(), listing);
        }
        self.cache
            .get(project)
            .with_context(|| format!("listing of {project}"))
    }
}

/// Path -> whether it is a dir, for every entry of a checkout's HEAD tree (`ls-tree -r -t`). A blobless partial clone keeps every tree, so a sparse checkout lists what its keep-set omits too. Empty when the checkout is absent.
fn project_tree(dir: &Path) -> Result<BTreeMap<String, bool>> {
    if !dir.join(".git").exists() {
        return Ok(BTreeMap::new());
    }
    let text = git(Some(dir), &["ls-tree", "-r", "-t", "-z", "HEAD"])?;
    let mut out = BTreeMap::new();
    for record in text.split('\0').filter(|r| !r.is_empty()) {
        let (meta, path) = record
            .split_once('\t')
            .with_context(|| format!("ls-tree record without a tab: {record}"))?;
        let kind = meta.split_whitespace().nth(1).unwrap_or_default();
        out.insert(path.to_owned(), kind == "tree");
    }
    Ok(out)
}

/// The manifest's link and copy files as (tree path they create, tree path they stand for): the AOSP manifest puts them outside every project's tree, so a read through one names its target.
fn manifest_links(root: &Path, aosp: &Aosp) -> Result<Vec<(String, String)>> {
    let Some(manifest) = aosp::load(root, aosp)? else {
        return Ok(Vec::new());
    };
    Ok(manifest
        .projects
        .iter()
        .flat_map(|(project, p)| {
            p.links
                .iter()
                .map(move |l| (l.dest.clone(), format!("{project}/{}", l.src)))
        })
        .collect())
}

/// The tree path a read reached through a manifest link names, if it lies under a link's dest.
fn link_target(rel: &str, links: &[(String, String)]) -> Option<String> {
    links.iter().find_map(|(dest, target)| {
        let rest = rel.strip_prefix(dest.as_str())?;
        if rest.is_empty() {
            Some(target.clone())
        } else {
            rest.strip_prefix('/').map(|r| format!("{target}/{r}"))
        }
    })
}

/// Where a read lives in the pinned trees: (owning project, project-relative path, is dir). Follows manifest links (at most two hops). `None`: no pinned tree holds it.
fn locate(
    trees: &mut Trees,
    projects: &[&str],
    links: &[(String, String)],
    rel: &str,
) -> Result<Option<(String, String, bool)>> {
    let mut rel = rel.to_owned();
    for _ in 0..3 {
        let Some(project) = owner(&rel, projects) else {
            return Ok(None);
        };
        let path = rel[project.len()..].trim_start_matches('/');
        if path.is_empty() {
            return Ok(Some((project.to_owned(), String::new(), true)));
        }
        if let Some(&dir) = trees.of(project)?.get(path) {
            return Ok(Some((project.to_owned(), path.to_owned(), dir)));
        }
        match link_target(&rel, links) {
            Some(target) => rel = target,
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Tree-relative path -> the graphs and tables that read it.
#[derive(Default)]
struct Reads(BTreeMap<String, BTreeSet<String>>);

impl Reads {
    fn add(&mut self, rel: &str, from: &str) {
        if let Some(sources) = self.0.get_mut(rel) {
            if !sources.contains(from) {
                sources.insert(from.to_owned());
            }
        } else {
            self.0
                .insert(rel.to_owned(), BTreeSet::from([from.to_owned()]));
        }
    }

    fn merge(&mut self, other: Reads) {
        for (rel, from) in other.0 {
            self.0.entry(rel).or_default().extend(from);
        }
    }
}

/// The shared tree `<ws>/src/aosp/` and task copies `<ws>/src-wt/<task>/aosp/` (a task copy holds the same project paths).
struct Prefixes {
    ws: String,
    shared: String,
    copies: String,
}

impl Prefixes {
    fn new(ws: &Path) -> Self {
        let ws = ws.display().to_string();
        Prefixes {
            shared: format!("{ws}/src/aosp/"),
            copies: format!("{ws}/src-wt/"),
            ws,
        }
    }

    /// Every tree-relative path in a token. Ninja can join two absolute paths into one token (`x.proto/home/...`), so the token is cut at each workspace prefix first.
    fn rels<'a>(&self, token: &'a str) -> Vec<&'a str> {
        let cuts: Vec<usize> = token
            .match_indices(self.ws.as_str())
            .map(|(i, _)| i)
            .collect();
        cuts.iter()
            .enumerate()
            .filter_map(|(k, &start)| {
                let end = cuts.get(k + 1).copied().unwrap_or(token.len());
                self.rel(&token[start..end])
            })
            .collect()
    }

    /// The tree-relative path a token names, if it names one.
    fn rel<'a>(&self, token: &'a str) -> Option<&'a str> {
        if let Some(i) = token.find(&self.shared) {
            return clean(&token[i + self.shared.len()..]);
        }
        let i = token.find(&self.copies)?;
        let (_task, rest) = token[i + self.copies.len()..].split_once('/')?;
        clean(rest.strip_prefix("aosp/")?)
    }
}

/// Ninja's `$` escapes and quoting left at a path token's end are cut.
fn clean(rel: &str) -> Option<&str> {
    let rel = rel.trim_end_matches(['$', ';', '\\', '`']);
    (!rel.is_empty()).then_some(rel)
}

/// Candidate path tokens of a line: split at whitespace, `:` (ninja's `$:` joins a classpath with one), `,` and `=` (flag values), `|` and the shell/ninja brackets.
fn tokens(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ':' | ',' | '=' | '|' | '"' | '\'' | '(' | ')' | '{' | '}'
            )
    })
}

fn scan_ninja(file: &Path, prefixes: &Prefixes, from: &str, reads: &mut Reads) -> Result<()> {
    let f = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    for line in BufReader::new(f).lines() {
        // ninja's `$$` is a literal `$` (a gson file name); `$:` is left for the token split
        let line = line
            .with_context(|| format!("reading {}", file.display()))?
            .replace("$$", "$");
        for token in tokens(&line) {
            for rel in prefixes.rels(token) {
                reads.add(rel, from);
            }
        }
    }
    Ok(())
}

/// The header reads of a graph's deps log: `ninja -t deps` streamed, each dependency line (indented, under a `<target>: #deps ...` header; licence_gate `parse_deps` format) a read.
/// Streamed rather than parsed into a target map: the log lists ~8M dependency lines per graph and only the paths matter here.
fn scan_deps(out: &Path, prefixes: &Prefixes, from: &str, reads: &mut Reads) -> Result<()> {
    let mut child = std::process::Command::new("ninja")
        .arg("-C")
        .arg(out)
        .args(["-t", "deps"])
        .stdout(std::process::Stdio::piped())
        // shown as it comes: a piped stderr read only after stdout ends could fill and stall ninja
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("running ninja -t deps")?;
    let stdout = child.stdout.take().context("ninja -t deps stdout")?;
    let mut in_record = false;
    let mut line = Vec::new();
    let mut r = BufReader::new(stdout);
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)
            .with_context(|| format!("reading ninja -t deps of {}", out.display()))?
            == 0
        {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        if text.starts_with(' ') {
            if in_record {
                for rel in prefixes.rels(text.trim()) {
                    reads.add(rel, from);
                }
            }
        } else {
            in_record = text.contains(": #deps");
        }
    }
    let status = child.wait().context("waiting for ninja -t deps")?;
    ensure!(
        status.success(),
        "ninja -t deps in {} exited with {status} (its stderr above)",
        out.display()
    );
    Ok(())
}

/// Every string of graph.json, split into tokens: the include dirs of compiles among them, which are dir reads (a header search reads the whole dir).
fn scan_json(value: &serde_json::Value, prefixes: &Prefixes, from: &str, reads: &mut Reads) {
    match value {
        serde_json::Value::String(s) => {
            for token in tokens(s) {
                for rel in prefixes.rels(token) {
                    reads.add(rel, from);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scan_json(item, prefixes, from, reads);
            }
        }
        serde_json::Value::Object(fields) => {
            for field in fields.values() {
                scan_json(field, prefixes, from, reads);
            }
        }
        _ => {}
    }
}

/// The module names a graph.json names: its goals, the keys and values of `deps`, and the keys of `artifacts`.
fn json_modules(json: &serde_json::Value) -> BTreeSet<String> {
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        match v {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut names = BTreeSet::new();
    names.extend(
        strings(json.get("goals"))
            .iter()
            .map(|s| module_name(s).to_owned()),
    );
    if let Some(deps) = json.get("deps").and_then(serde_json::Value::as_object) {
        for (module, needs) in deps {
            names.insert(module_name(module).to_owned());
            names.extend(
                strings(Some(needs))
                    .iter()
                    .map(|s| module_name(s).to_owned()),
            );
        }
    }
    if let Some(artifacts) = json.get("artifacts").and_then(serde_json::Value::as_object) {
        names.extend(artifacts.keys().map(|k| module_name(k).to_owned()));
    }
    names
}

/// A graph's module key (`device/libbase`, `host/aapt2`, `nb:x86_64:libc`) as the module name Soong defines (`libbase`, `aapt2`, `libc`).
fn module_name(key: &str) -> &str {
    key.rsplit(['/', ':']).next().unwrap_or(key)
}

/// The `Android.bp` files of `name` the converter can see: the defining project's when Soong resolves the name to one (source over prebuilt), else every definition; only in pinned projects (the converter parses the checkouts, not the manifest).
fn module_files(index: &Index, name: &str, pinned: &[&str]) -> Vec<String> {
    let resolved = index.project_for(name).ok().map(str::to_owned);
    index
        .definitions_of(name)
        .into_iter()
        .filter(|(_, file)| {
            resolved
                .as_deref()
                .is_none_or(|p| file.starts_with(&format!("{p}/")))
        })
        .map(|(_, file)| file.to_owned())
        .filter(|file| owner(file, pinned).is_some())
        .collect()
}

/// The pinned project owning `rel`: the deepest one that is it or its parent (projects nest; an inner one is its own checkout).
fn owner<'a>(rel: &str, projects: &[&'a str]) -> Option<&'a str> {
    projects
        .iter()
        .copied()
        .filter(|p| covers(p, rel))
        .max_by_key(|p| p.len())
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `under` is `dir` or lies below it (a sibling sharing a name prefix is not below).
fn covers(dir: &str, under: &str) -> bool {
    under == dir || under.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// The minimal cover of `dirs` by non-nested dirs: a dir under a kept one is dropped. "" (no dir needed) is skipped.
fn cover<'a>(dirs: impl Iterator<Item = &'a str>) -> Vec<String> {
    let sorted: BTreeSet<&str> = dirs.filter(|d| !d.is_empty()).collect();
    let mut kept: Vec<String> = Vec::new();
    for d in sorted {
        if !kept.iter().any(|k| covers(k, d)) {
            kept.push(d.to_owned());
        }
    }
    kept
}

/// The keep-set a project's reads need.
fn keep_set(needs: &[Need]) -> Vec<String> {
    cover(needs.iter().map(|n| n.keep.as_str()))
}

/// Whether a row keeps what `n` reads: a whole-project read never fits a row; a root file always does.
fn covered(n: &Need, row: &[String]) -> bool {
    !n.path.is_empty() && (n.keep.is_empty() || row.iter().any(|k| covers(k, &n.keep)))
}

/// Every read a row's project does not cover, one line each (empty = every row covers its reads).
fn uncovered(sources: &Sources, derived: &Derived) -> Vec<String> {
    let mut out = Vec::new();
    for (project, row) in &sources.aosp.sparse {
        for n in derived.owned.get(project).into_iter().flatten() {
            if !covered(n, row) {
                let from: Vec<&str> = n.from.iter().map(String::as_str).collect();
                let what = if n.path.is_empty() {
                    "the whole project".to_owned()
                } else {
                    format!("`{}`", n.path)
                };
                out.push(format!(
                    "{project}: {what} read by {} outside keep-set {row:?}",
                    from.join(", ")
                ));
            }
        }
    }
    out
}

/// Prints each project's derived keep-set against its row and returns the uncovered-read count. With `--project`, the named projects are listed in full with apparent sizes.
fn show(layout: &Layout, sources: &Sources, derived: &Derived, only: &[String]) -> usize {
    let tree = layout.src().join("aosp");
    println!(
        "keepset: graphs {}; {} project(s) read; unowned {} tree path(s); absent from the pinned tree {}",
        derived.graphs.join(" "),
        derived.owned.len(),
        derived.unowned.len(),
        derived.missing.len()
    );
    for u in derived.unowned.iter().take(5) {
        println!("  unowned: {u}");
    }
    for m in derived.missing.iter().take(5) {
        println!("  missing: {m}");
    }
    let projects: BTreeSet<&str> = derived
        .owned
        .keys()
        .map(String::as_str)
        .chain(sources.aosp.sparse.keys().map(String::as_str))
        .filter(|p| only.is_empty() || only.iter().any(|o| o == p))
        .collect();
    for project in projects {
        let needs = derived.owned.get(project).map_or(&[][..], Vec::as_slice);
        let dirs = keep_set(needs);
        let row = sources.aosp.sparse.get(project);
        let status = match row {
            None => "no row (whole tree)".to_owned(),
            Some(r) => {
                let miss = needs.iter().filter(|n| !covered(n, r)).count();
                let unused = r
                    .iter()
                    .filter(|k| !needs.iter().any(|n| covers(k, &n.keep)))
                    .count();
                format!(
                    "row keeps {} dir(s), misses {miss} read(s), {unused} unused",
                    r.len()
                )
            }
        };
        println!(
            "{project}: {} read(s), derived keep-set {} dir(s); {status}",
            needs.len(),
            dirs.len()
        );
        if only.iter().any(|o| o == project) {
            println!("  keep: {}", dirs.join(" "));
            if let Some(r) = row {
                println!("  row:  {}", r.join(" "));
            }
            let total = bytes(&tree.join(project));
            let kept: u64 = dirs
                .iter()
                .map(|d| bytes(&tree.join(project).join(d)))
                .sum();
            println!("  apparent bytes: keep-set {kept} of {total} (root files not counted)");
        }
    }
    uncovered(sources, derived).len()
}

/// Apparent size of a tree: regular files under `path`, symlinks not followed.
fn bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries.flatten().map(|e| bytes(&e.path())).sum::<u64>()
    })
}

/// `--write`: the derived keep-set of each named project into its `[aosp.sparse]` row; the rest of sources.toml is untouched.
fn write(root: &Path, derived: &Derived, projects: &[String]) -> Result<()> {
    let mut rows = Vec::new();
    for p in projects {
        let needs = derived.owned.get(p).map_or(&[][..], Vec::as_slice);
        ensure!(
            !needs.is_empty(),
            "{p}: no derived reads (no graph names it): a row would keep nothing"
        );
        ensure!(
            needs.iter().all(|n| !n.path.is_empty()),
            "{p}: a graph reads the whole project; no keep-set row can express that"
        );
        let dirs = keep_set(needs);
        ensure!(
            !dirs.is_empty(),
            "{p}: every read is a root file; nothing to keep in cone mode"
        );
        println!("{p}: writing {} dir(s): {}", dirs.len(), dirs.join(" "));
        rows.push((p.clone(), dirs));
    }
    write_rows(root, &rows)
}

/// Sets each `[aosp.sparse]` row in sources.toml with toml_edit (comments, order and other rows kept), then re-parses the result.
fn write_rows(root: &Path, rows: &[(String, Vec<String>)]) -> Result<()> {
    let file = root.join(sources::FILE);
    let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().context("parsing sources.toml")?;
    let Some(aosp) = doc.get_mut("aosp").and_then(toml_edit::Item::as_table_mut) else {
        bail!("sources.toml has no [aosp] table");
    };
    let sparse = aosp
        .entry("sparse")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .context("[aosp.sparse] is not a table")?;
    for (path, dirs) in rows {
        let mut array = toml_edit::Array::new();
        for d in dirs {
            array.push(d.as_str());
        }
        let mut value = toml_edit::Value::Array(array);
        if let Some(old) = sparse.get(path).and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        sparse.insert(path, toml_edit::Item::Value(value));
    }
    let out = doc.to_string();
    sources::parse(&out).context("the rewritten sources.toml does not parse")?;
    fs::write(&file, out).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/home/u/atl-next";

    /// Expected values come from the token forms in the docstring: a shared-tree path, a flag with the tree prefix, a task copy, a non-tree path, the bare prefix.
    #[test]
    fn tree_paths_in_tokens() {
        let p = Prefixes::new(Path::new(WS));
        let rel = |t: &str| p.rel(t).map(str::to_owned);
        assert_eq!(
            rel("/home/u/atl-next/src/aosp/prebuilts/sdk/36/public/android.jar"),
            Some("prebuilts/sdk/36/public/android.jar".into())
        );
        assert_eq!(
            rel("-I/home/u/atl-next/src/aosp/frameworks/native/include"),
            Some("frameworks/native/include".into())
        );
        let wrapped: Vec<&str> =
            tokens("(/home/u/atl-next/src-wt/hj-egl/aosp/frameworks/base/x.java)")
                .filter_map(|t| p.rel(t))
                .collect();
        assert_eq!(wrapped, ["frameworks/base/x.java"]);
        assert_eq!(
            rel("/home/u/atl-next/src/aosp/a/b.jar$"),
            Some("a/b.jar".into())
        );
        assert_eq!(rel("/home/u/atl-next/out/slot-3/x.o"), None);
        assert_eq!(rel("/home/u/atl-next/src/aosp/"), None);
        let joined =
            p.rels("/home/u/atl-next/src/aosp/a/b.proto/home/u/atl-next/src/aosp/c/d.java");
        assert_eq!(joined, ["a/b.proto", "c/d.java"]);
    }

    /// A build edge with explicit, implicit (`|`) and order-only (`||`) inputs, and a javac classpath binding joined by ninja's `$:`: five tree paths, one each.
    #[test]
    fn ninja_edge_reads() {
        let line = "build out/x.jar: javac /home/u/atl-next/src/aosp/p/A.java | /home/u/atl-next/src/aosp/q/h.h || /home/u/atl-next/src/aosp/r/o";
        let binding = "  cp = -classpath /home/u/atl-next/src/aosp/prebuilts/sdk/28/public/android.jar$:/home/u/atl-next/src/aosp/prebuilts/sdk/tools/core-lambda-stubs.jar";
        let p = Prefixes::new(Path::new(WS));
        let mut got: Vec<&str> = tokens(line)
            .chain(tokens(binding))
            .filter_map(|t| p.rel(t))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "p/A.java",
                "prebuilts/sdk/28/public/android.jar",
                "prebuilts/sdk/tools/core-lambda-stubs.jar",
                "q/h.h",
                "r/o",
            ]
        );
    }

    /// A deps log ninja cannot read (no build.ninja) fails the scan instead of reading as no header reads.
    #[test]
    fn deps_scan_fails_when_ninja_does() {
        let dir = std::env::temp_dir().join(format!("xtask-keepset-deps-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test dir");
        let mut reads = Reads::default();
        let err = scan_deps(&dir, &Prefixes::new(Path::new(WS)), "t", &mut reads)
            .expect_err("no build.ninja");
        assert!(format!("{err:#}").contains("ninja -t deps"), "{err:#}");
        fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    #[test]
    fn module_keys_name_their_module() {
        assert_eq!(module_name("device/libbase"), "libbase");
        assert_eq!(module_name("host/aapt2"), "aapt2");
        assert_eq!(module_name("nb:x86_64:libc"), "libc");
        assert_eq!(module_name("plain"), "plain");
    }

    /// Minimal non-nested cover: an ancestor absorbs its descendants; a sibling sharing a name prefix ("28" vs "280") is not an ancestor.
    #[test]
    fn cover_is_minimal_and_path_aware() {
        assert_eq!(
            cover(["36", "36/public", "35/x", "tools", "", "35"].into_iter()),
            ["35", "36", "tools"]
        );
        assert_eq!(cover(["a/x", "a", "a-b"].into_iter()), ["a", "a-b"]);
        assert!(!covers("28", "280/x"));
        assert!(covers("28", "28/x/y"));
        assert!(covers("28", "28"));
    }

    #[test]
    fn owner_is_the_deepest_pinned_project() {
        let projects = ["frameworks/base", "frameworks/base/core", "external/x"];
        assert_eq!(
            owner("frameworks/base/core/a.java", &projects),
            Some("frameworks/base/core")
        );
        assert_eq!(
            owner("frameworks/base/Makefile", &projects),
            Some("frameworks/base")
        );
        assert_eq!(owner("frameworks/basement", &projects), None);
        assert_eq!(owner("unpinned/z", &projects), None);
    }

    fn need(path: &str, keep: &str) -> Need {
        Need {
            path: path.into(),
            keep: keep.into(),
            from: BTreeSet::from(["slot-3/build.ninja".to_owned()]),
        }
    }

    /// A read is covered by a row dir holding its keep dir; a root file always is; a whole-project read never is. A row dir is unused when no read's keep dir lies under it.
    #[test]
    fn rows_cover_and_unused_dirs() {
        let needs = [
            need("28/public/a.jar", "28/public"),
            need("28/system/b.jar", "28/system"),
            need("README", ""),
        ];
        let row = |dirs: &[&str]| dirs.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>();
        let miss = |r: &[String]| needs.iter().filter(|n| !covered(n, r)).count();
        assert_eq!(miss(&row(&["28/public"])), 1);
        assert_eq!(miss(&row(&["28"])), 0);
        assert_eq!(miss(&row(&["28/public", "30"])), 1);
        assert_eq!(miss(&row(&["280"])), 2);
        assert!(!covered(&need("", "x"), &row(&["x"])));
        let unused = row(&["28/public", "30"])
            .iter()
            .filter(|k| !needs.iter().any(|n| covers(k, &n.keep)))
            .count();
        assert_eq!(unused, 1);
    }

    #[test]
    fn keep_set_is_the_cover_of_the_reads() {
        let needs = [
            need("35/finalized-flags.txt", "35"),
            need("35/public/android.jar", "35/public"),
            need("tools/core-lambda-stubs.jar", "tools"),
            need("LICENSE", ""),
        ];
        assert_eq!(keep_set(&needs), ["35", "tools"]);
    }
}
