// SPDX-License-Identifier: MIT
//! The licence gate over `xtask native`'s build (CLAUDE.md licence rules): every file that reaches a shipped artifact carries a licence licenses.toml allows in it.
//! It is one edge of build.ninja (bp2ninja -licence-gate), so ninja alone decides when it runs: `xtask licence-gate` (hidden, `run_edge`) reads the edge's inputs from its rspfile, writes the report (`$out`, JSON) and,
//! as its depfile, every file it read or reached plus its own binary and tables, so a changed artifact, reached file, licence header, licenses.toml row or gate build reruns it and nothing else does.
//! The edge runs only once every artifact linked, so each is this build's (no stale artifact to tell apart). Findings are the report's, not the edge's status: `native` prints it and fails the run.
//! - Shipped artifacts: the device-side link outputs (bp2ninja emits the edge over those under `<out>/device/`), all of class `runtime`. Host outputs are build tools and never ship.
//! - Reached files, per artifact: the objects its link loaded, from `<artifact>.trace` (bp2ninja's link rule runs lld with `--trace`: each object on the command line and each archive member loaded -- those symbol resolution pulled in, every member of a `--whole-archive`); a member is mapped back to its object through the archive's ninja edge (`ninja -t query`). Members are named by file name, so where two objects of one archive share one, both count.
//!   An archive whose members are none of its inputs but which has a depfile record was compiled whole by its own edge (rustc's generated staticlib): the archive is the object, whichever of its codegen units ld loaded.
//!   Then each object's source and headers, from its deps log record (ninja_deps.rs); a dependency with a record of its own (an rlib, which bp2ninja's rustc rule lists after rustc's dep-info) stands for what it is made of, transitively. So a Rust archive reaches every crate of its closure's sources; the toolchain sysroot's std/core/alloc are rustc's own inputs, in no depfile, and are not reached -- like the C toolchain's crt objects and libc++, the platform's.
//! - A file's licence: `notice::resolve` with the `[upstream.<key>]` of the checkout holding it; AOSP projects share key `aosp`, their paths relative to the tree root (`external/dng_sdk/...`).
//!
//! - A member of an archive no edge of this build makes (built by an upstream's own CMake/ninja build: `xtask runtimes`, providers.toml `upstream:`) is mapped to its object through that build's edges and deps log (`external_build`).
//!
//! Not checked, only counted: files generated into a build dir (aidl, protobuf, sysprop, genrule outputs, our crates' generated headers; their generators' inputs are not followed) -- except staged copies of sources (`Staged`), judged as the source they copy; system files under `/usr` and `/lib*` (toolchain, distro headers and libraries: the platform's, not ours to ship).
//! A file anywhere else fails: nothing passes because the gate cannot place it.
//! Not in the depfile: build.ninja and the deps logs. An archive's members or an object's headers change only through an edit to an input already listed (a source, a header, a blueprint) or a relinked artifact, and a regenerated build.ninja alone would rerun the whole gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::licenses::{self, Licenses};
use crate::notice;

/// Toolchain and distro files: the platform's.
const SYSTEM_DIRS: &[&str] = &["/usr", "/lib", "/lib64"];

/// A checkout as the gate places files: its canonical directory, its licence key, and where it sits inside its upstream (an AOSP project's tree path; empty for other upstreams).
#[derive(Debug, PartialEq)]
pub struct Root {
    pub dir: PathBuf,
    pub key: String,
    pub prefix: PathBuf,
}

/// One gate run; the edge's output (`run_edge`), read back by `native`.
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub artifacts: usize,
    /// Artifacts of `[test_only]` modules, not checked: never shipped.
    pub test_only: usize,
    /// Files whose licence was resolved and checked.
    pub checked: usize,
    pub generated: usize,
    pub system: usize,
    /// Licence expression -> files under it (as resolved: key-relative) -> the first artifact reaching it.
    pub denied: BTreeMap<String, BTreeMap<String, String>>,
    /// Files with no licence the rules can name, or that the gate cannot place.
    pub unresolved: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.denied.is_empty() && self.unresolved.is_empty()
    }
}

/// What a gate run depends on beyond its artifacts, for the edge's depfile: every file read or reached (canonical where it was placed), each blueprint the licence fallback tried.
pub type Inputs = BTreeSet<PathBuf>;

/// `xtask licence-gate`: the gate as build.ninja's edge (bp2ninja -licence-gate); hidden, ninja runs it.
#[derive(clap::Args)]
pub struct EdgeArgs {
    /// The build directory.
    #[arg(long)]
    out: PathBuf,
    /// The edge's rspfile: its inputs, the device link outputs, separated by white space.
    #[arg(long)]
    artifacts: PathBuf,
    /// Where the report goes (JSON `Report`): the edge's output.
    #[arg(long)]
    report: PathBuf,
    /// Where the depfile goes: every input of this run (`Inputs`, the gate's binary and tables).
    #[arg(long)]
    depfile: PathBuf,
}

/// The tables a gate run reads, relative to the checkout.
const TABLES: &[&str] = &[licenses::FILE, "deny.toml", crate::sources::FILE];

/// Runs the gate as an edge: the report always (findings are its content, exit 0), the depfile last. An error (a table that does not load, an unreadable trace dir) fails the edge.
pub fn run_edge(root: &Path, args: &EdgeArgs) -> Result<std::process::ExitCode> {
    let layout = atl_layout::Layout::of(root)?;
    let sources = crate::sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    licenses::check_test_only(&licenses)?;
    let out = std::fs::canonicalize(&args.out)?;
    let roots = roots(&layout, &sources.checkouts(None));
    let test_only = licenses::test_only_outputs(&licenses, &out)?
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let listed = std::fs::read_to_string(&args.artifacts)
        .with_context(|| format!("reading {}", args.artifacts.display()))?;
    let artifacts = shell_words(&listed)
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let mut inputs = Inputs::new();
    inputs.extend(TABLES.iter().map(|t| root.join(t)));
    inputs.extend(licenses.aosp_index.file.clone());
    inputs.insert(std::env::current_exe().context("the gate's own binary")?);
    let report = check(&out, artifacts, &roots, &licenses, &test_only, &mut inputs)?;
    std::fs::write(&args.report, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("writing {}", args.report.display()))?;
    write_depfile(&args.depfile, &args.report, &inputs)?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The paths of ninja's `$in` (an rspfile's content): separated by spaces, each one holding a byte outside ninja's shell-safe set single-quoted, `'` itself as `'\''` (util.cc GetShellEscapedString; measured: `'…/android.hardware.graphics.common@1.0.so'`).
fn shell_words(text: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if !quoted => {
                word.extend(chars.next());
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// A depfile naming `inputs` for `target`: a missing one as its nearest existing ancestor (ninja reruns an edge whose depfile names a missing file every time; a file deleted since the deps log was written reappears in its directory's entries).
/// Escaped as ninja's depfile parser reads it: `\` before space and `#`, `$$` for `$`. Its lexer takes no other byte outside [A-Za-z0-9+,/_:.~(){}%=@[]!-] and >= 0x80: a path holding one (a `*`) would split into a missing path and rerun the edge forever, so it stands as its nearest ancestor that spells (tools/bp2ninja inputs.go depfileSpellable, measured there).
fn write_depfile(path: &Path, target: &Path, inputs: &Inputs) -> Result<()> {
    let esc = |p: &Path| {
        p.display()
            .to_string()
            .replace(' ', "\\ ")
            .replace('#', "\\#")
            .replace('$', "$$")
    };
    let spellable = |p: &Path| {
        p.as_os_str().as_encoded_bytes().iter().all(|&c| {
            c.is_ascii_alphanumeric() || c >= 0x80 || b"+,/_:.~(){}%=@[]!- #$".contains(&c)
        })
    };
    let mut text = format!("{}:", esc(target));
    let mut listed = BTreeSet::new();
    for p in inputs {
        let existing = p
            .ancestors()
            .find(|a| a.exists() && spellable(a))
            .unwrap_or(Path::new("/"));
        if listed.insert(existing) {
            text.push_str(&format!(" \\\n  {}", esc(existing)));
        }
    }
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Runs the gate over the device link outputs `artifacts` of the build directory `out` (canonical), all of this build (the edge runs after their links); what it read and reached goes to `inputs`.
/// Artifacts in `test_only` (licenses::test_only_outputs) are skipped: what they alone reach ships nowhere.
pub fn check(
    out: &Path,
    mut artifacts: Vec<PathBuf>,
    roots: &[Root],
    licenses: &Licenses,
    test_only: &BTreeSet<PathBuf>,
    inputs: &mut Inputs,
) -> Result<Report> {
    let mut report = Report::default();
    artifacts.retain(|a| {
        let keep = !test_only.contains(a);
        report.test_only += usize::from(!keep);
        keep
    });
    report.artifacts = artifacts.len();
    // object -> first artifact loading it
    let mut objects: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    let mut members: Vec<(PathBuf, String, &Path)> = Vec::new();
    for artifact in &artifacts {
        let trace = trace_of(artifact);
        inputs.insert(trace.clone());
        let text = match std::fs::read_to_string(&trace) {
            Ok(t) => t,
            Err(e) => {
                report.unresolved.push(format!(
                    "{}: {e} (linked without a trace?)",
                    trace.display()
                ));
                continue;
            }
        };
        let loaded = parse_trace(&text);
        for o in loaded.objects {
            objects.entry(o).or_insert(artifact);
        }
        members.extend(
            loaded
                .members
                .into_iter()
                .map(|(a, m)| (a, m, artifact.as_path())),
        );
    }
    let archives: BTreeSet<&Path> = members
        .iter()
        .map(|(a, _, _)| a.as_path())
        .filter(|a| a.starts_with(out))
        .collect();
    let mut args = vec!["-t".to_owned(), "query".to_owned()];
    args.extend(archives.iter().map(|a| a.display().to_string()));
    let archive_inputs = if archives.is_empty() {
        BTreeMap::new()
    } else {
        parse_query(&ninja(
            out,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?)
    };
    let deps = crate::ninja_deps::read(out)?.deps();
    // archives no edge of this build makes: external build dir -> (archive, member, artifact)
    let mut external: BTreeMap<PathBuf, Vec<(&Path, &str, &Path)>> = BTreeMap::new();
    for (archive, member, artifact) in &members {
        let Some(objs) = archive_inputs.get(archive) else {
            if is_system(archive) {
                report.system += 1;
            } else if let Some(b) = external_build(archive, out) {
                external
                    .entry(b)
                    .or_default()
                    .push((archive, member, artifact));
            } else {
                report.unresolved.push(format!(
                    "{}: member {member} of an archive no ninja build makes (in {})",
                    archive.display(),
                    artifact.display()
                ));
            }
            continue;
        };
        let matching: Vec<&PathBuf> = objs
            .iter()
            .filter(|o| o.file_name().is_some_and(|n| *n == **member))
            .collect();
        if !matching.is_empty() {
            for o in matching {
                objects.entry(o.clone()).or_insert(artifact);
            }
        } else if deps.contains_key(archive) {
            // compiled whole by its own edge (rustc's staticlib): its members are codegen units, its depfile says what they are made of
            objects.entry(archive.clone()).or_insert(artifact);
        } else {
            report.unresolved.push(format!(
                "{}: member {member} is none of its ninja inputs (in {})",
                archive.display(),
                artifact.display()
            ));
        }
    }
    // file -> first artifact reaching it
    let mut files: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (object, artifact) in &objects {
        if is_system(object) {
            continue;
        }
        if !deps.contains_key(object) {
            report
                .unresolved
                .push(format!("{}: no depfile record", object.display()));
            continue;
        }
        for f in made_of(object, &deps) {
            files.entry(f.to_path_buf()).or_insert(artifact);
        }
    }
    judge_files(files, roots, out, licenses, &mut report, inputs)?;
    for (build, loaded) in external {
        check_external(
            &build,
            out,
            Reach::Members(&loaded),
            &[],
            roots,
            licenses,
            &mut report,
            inputs,
        )?;
    }
    Ok(report)
}

/// What of an external ninja build reaches a shipped artifact.
enum Reach<'a> {
    /// Archive members a converter link loaded (`--trace`): (archive, member, artifact). Mapped to objects by archive file name (an install copies the archive under the same name).
    Members(&'a [(&'a Path, &'a str, &'a Path)]),
    /// Every object the build compiled (its depfile records), each its own artifact: for a build whose archives later links load (the runtimes), a superset, so a pass holds for any link.
    All,
}

/// One external ninja build: what reaches shipped artifacts (`reach`) -> objects through the build's own edges -> each object's deps log record -> staged copies mapped to
/// their sources (`staged`) -> judged; files under `out` (the dir holding the build) count as generated.
#[allow(clippy::too_many_arguments)]
fn check_external(
    build: &Path,
    out: &Path,
    reach: Reach,
    staged: &[Staged],
    roots: &[Root],
    licenses: &Licenses,
    report: &mut Report,
    inputs: &mut Inputs,
) -> Result<()> {
    let deps = crate::ninja_deps::read(build)?.deps();
    let objects: BTreeMap<PathBuf, &Path> = match reach {
        Reach::Members(loaded) => member_objects(build, loaded, report)?,
        Reach::All => {
            report.artifacts += deps.len();
            deps.keys().map(|o| (o.clone(), o.as_path())).collect()
        }
    };
    let abs = |f: &Path| {
        let p = if f.is_absolute() {
            f.to_path_buf()
        } else {
            build.join(f)
        };
        std::fs::canonicalize(&p).unwrap_or(p)
    };
    let mut placed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (object, artifact) in &objects {
        if !deps.contains_key(object) {
            report.unresolved.push(format!(
                "{}: no depfile record in {}",
                object.display(),
                build.display()
            ));
            continue;
        }
        for f in made_of(object, &deps) {
            placed.entry(abs(f)).or_insert(artifact);
        }
    }
    inputs.extend(placed.keys().cloned());
    let (placed, configured) = unstage(placed, staged);
    judge(placed, roots, out, licenses, report, inputs)?;
    for (file, s, artifact) in configured {
        let text = String::from_utf8_lossy(
            &std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?,
        )
        .into_owned();
        let rel = file.display().to_string();
        let own = || {
            Err(anyhow::anyhow!(
                "{rel}: staged but no identical source, and no own notice"
            ))
        };
        match notice::resolve(&rel, &text, licenses.upstream.get(&s.key), own) {
            Ok(expr) => {
                report.checked += 1;
                if !licenses.allows(licenses::RUNTIME, &expr)? {
                    let name = artifact
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    report.denied.entry(expr).or_default().insert(rel, name);
                }
            }
            Err(e) => report.unresolved.push(format!("{e:#}")),
        }
    }
    Ok(())
}

/// `Reach::Members`: object (as the build's edges name it) -> first artifact loading it.
fn member_objects<'a>(
    build: &Path,
    loaded: &[(&Path, &str, &'a Path)],
    report: &mut Report,
) -> Result<BTreeMap<PathBuf, &'a Path>> {
    let targets: Vec<PathBuf> = lines(&ninja(build, &["-t", "targets", "all"])?)
        .filter_map(|l| l.rsplit_once(": ").map(|(t, _)| build.join(t)))
        .collect();
    let mut objects = BTreeMap::new();
    for (archive, member, artifact) in loaded {
        let named: Vec<&PathBuf> = targets
            .iter()
            .filter(|t| t.file_name() == archive.file_name())
            .collect();
        let [made] = named[..] else {
            report.unresolved.push(format!(
                "{}: {} edges of {} make an archive of this name (in {})",
                archive.display(),
                named.len(),
                build.display(),
                artifact.display()
            ));
            continue;
        };
        let query = ninja(build, &["-t", "query", &made.display().to_string()])?;
        let objs: Vec<PathBuf> = parse_query(&query).into_values().flatten().collect();
        let Some(object) = objs
            .into_iter()
            .find(|o| o.file_name().is_some_and(|n| n == *member))
        else {
            report.unresolved.push(format!(
                "{}: member {member} is none of its inputs in {} (in {})",
                archive.display(),
                build.display(),
                artifact.display()
            ));
            continue;
        };
        objects.entry(object).or_insert(*artifact);
    }
    Ok(objects)
}

/// `judge` over reached files (file -> first artifact reaching it), canonicalized first; each reached path, as named and as placed, is an input.
fn judge_files(
    files: BTreeMap<PathBuf, &Path>,
    roots: &[Root],
    out: &Path,
    licenses: &Licenses,
    report: &mut Report,
    inputs: &mut Inputs,
) -> Result<()> {
    let mut placed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (file, artifact) in files {
        let real = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        inputs.insert(file);
        placed.entry(real).or_insert(artifact);
    }
    inputs.extend(placed.keys().cloned());
    judge(placed, roots, out, licenses, report, inputs)
}

/// The CMake/ninja build an archive outside this build's edges came from: the nearest ancestor below `out` holding `build.ninja` (the archive sits in its build dir, as `xtask runtimes` leaves it) or `build/build.ninja` (an install prefix beside its build dir, as `upstream::build` leaves it).
fn external_build(archive: &Path, out: &Path) -> Option<PathBuf> {
    archive
        .ancestors()
        .skip(1)
        .take_while(|a| *a != out)
        .flat_map(|a| [a.to_path_buf(), a.join("build")])
        .find(|d| d.join("build.ninja").is_file())
}

/// A directory a build copies source files into (CMake header staging: libc++'s `include/c++/v1`), and where they come from.
/// A file there byte-identical to `<from>/<its relative path>` is judged as that source; any other (a `configure_file` output) on its own notice under `key`, as `<dir>/<rest>` with no package fallback: a staged file the gate cannot trace to a source must name its own licence.
pub struct Staged {
    /// Absolute, canonical.
    pub dir: PathBuf,
    /// Source directories, canonical, searched in order.
    pub from: Vec<PathBuf>,
    /// licenses.toml upstream key of the configured files.
    pub key: String,
}

/// Replaces each staged file of `placed` by its source; returns the configured ones (no identical source).
#[allow(clippy::type_complexity)]
fn unstage<'a>(
    placed: BTreeMap<PathBuf, &'a Path>,
    staged: &'a [Staged],
) -> (
    BTreeMap<PathBuf, &'a Path>,
    Vec<(PathBuf, &'a Staged, &'a Path)>,
) {
    let mut kept = BTreeMap::new();
    let mut configured = Vec::new();
    for (file, artifact) in placed {
        let Some((s, rest)) = staged
            .iter()
            .find_map(|s| Some((s, file.strip_prefix(&s.dir).ok()?.to_path_buf())))
        else {
            kept.entry(file).or_insert(artifact);
            continue;
        };
        let bytes = std::fs::read(&file).ok();
        match s
            .from
            .iter()
            .map(|d| d.join(&rest))
            .find(|src| bytes.is_some() && std::fs::read(src).ok() == bytes)
        {
            Some(src) => {
                kept.entry(src).or_insert(artifact);
            }
            None => configured.push((file, s, artifact)),
        }
    }
    (kept, configured)
}

/// The licence gate over the CMake-generated ninja build in `build` (canonical; the runtimes, `xtask runtimes`): `Reach::All` of [`check_external`], counting each object as an artifact.
pub fn check_build(
    build: &Path,
    staged: &[Staged],
    roots: &[Root],
    licenses: &Licenses,
) -> Result<Report> {
    let mut report = Report::default();
    // not an edge (runtimes.rs gates its own builds after each run): no depfile
    check_external(
        build,
        build,
        Reach::All,
        staged,
        roots,
        licenses,
        &mut report,
        &mut Inputs::new(),
    )?;
    Ok(report)
}

/// One upstream CMake build the gate judges: its canonical dir and the copies it stages.
pub struct Build {
    pub name: String,
    pub dir: PathBuf,
    pub staged: Vec<Staged>,
}

/// The licence gate over each build (`check_build`): a line per build with its denied and unresolved files, then `<label>: verdict`. True when every build passes.
/// The one route for an upstream CMake build (runtimes), so every such build is judged by the same rules.
pub fn gate_builds(root: &Path, label: &str, builds: &[Build]) -> Result<bool> {
    let layout = atl_layout::Layout::of(root)?;
    let sources = crate::sources::load(root)?;
    let checkouts = sources.checkouts(None);
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let roots = roots(&layout, &checkouts);
    let mut ok = true;
    for b in builds {
        let r = check_build(&b.dir, &b.staged, &roots, &licenses)?;
        println!(
            "{label}: licence {:<20} {} objects, {} files checked, {} generated, {} system",
            b.name, r.artifacts, r.checked, r.generated, r.system
        );
        for (expr, files) in &r.denied {
            println!(
                "{label}: licence DENIED {expr}: {} files, e.g. {}",
                files.len(),
                files.keys().next().map_or("", |k| k)
            );
        }
        for u in r.unresolved.iter().take(5) {
            println!("{label}: licence UNRESOLVED {u}");
        }
        ok &= r.ok();
    }
    println!("{label}: verdict {}", if ok { "PASS" } else { "FAIL" });
    Ok(ok)
}

/// Resolves and checks the licence of each placed file (canonical path -> the first artifact reaching it); files under `out` count as generated.
fn judge(
    placed: BTreeMap<PathBuf, &Path>,
    roots: &[Root],
    out: &Path,
    licenses: &Licenses,
    report: &mut Report,
    inputs: &mut Inputs,
) -> Result<()> {
    for (file, artifact) in placed {
        let name = artifact
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match place(&file, roots, out) {
            Place::Upstream { root, rel } => {
                let text = String::from_utf8_lossy(
                    &std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?,
                )
                .into_owned();
                // only a notice-less file reads its package's Android.bp files
                let at = file
                    .strip_prefix(&root.dir)
                    .ok()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new(""));
                let fallback = || {
                    let mut seen = |p: &Path| {
                        inputs.insert(p.to_path_buf());
                    };
                    licenses.unlabelled(&root.key, &root.prefix, &root.dir, at, &mut seen)
                };
                match notice::resolve(&rel, &text, licenses.upstream.get(&root.key), fallback) {
                    Ok(expr) => {
                        report.checked += 1;
                        if !licenses.allows(licenses::RUNTIME, &expr)? {
                            report.denied.entry(expr).or_default().insert(rel, name);
                        }
                    }
                    Err(e) => report.unresolved.push(format!("{e:#} (in {name})")),
                }
            }
            Place::Generated => report.generated += 1,
            Place::System => report.system += 1,
            Place::Unknown => report.unresolved.push(format!(
                "{}: in no checkout, the build dir or a system dir (in {name})",
                file.display()
            )),
        }
    }
    Ok(())
}

/// The linker trace bp2ninja's link rule writes beside an artifact.
fn trace_of(artifact: &Path) -> PathBuf {
    let mut name = artifact.as_os_str().to_owned();
    name.push(".trace");
    PathBuf::from(name)
}

/// `ninja -C out <args>`'s stdout: manifest tools only (`-t query`, `-t targets`), which load no log and so are safe inside a running build.
pub(crate) fn ninja(out: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("ninja")
        .arg("-C")
        .arg(out)
        .args(args)
        .output()
        .context("running ninja")?;
    ensure!(
        output.status.success(),
        "ninja {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty())
}

fn is_system(path: &Path) -> bool {
    SYSTEM_DIRS.iter().any(|d| path.starts_with(d))
}

#[derive(Debug, PartialEq)]
enum Place<'a> {
    Upstream { root: &'a Root, rel: String },
    Generated,
    System,
    Unknown,
}

/// Where a reached file lives; the innermost checkout wins (AOSP projects nest).
fn place<'a>(path: &Path, roots: &'a [Root], out: &Path) -> Place<'a> {
    let root = roots
        .iter()
        .filter(|r| path.starts_with(&r.dir))
        .max_by_key(|r| r.dir.components().count());
    if let Some(r) = root
        && let Ok(rest) = path.strip_prefix(&r.dir)
    {
        let rel = r.prefix.join(rest).display().to_string();
        return Place::Upstream { root: r, rel };
    }
    if path.starts_with(out) {
        Place::Generated
    } else if is_system(path) {
        Place::System
    } else {
        Place::Unknown
    }
}

/// What one link loaded.
#[derive(Debug, Default, PartialEq)]
struct Loaded {
    objects: Vec<PathBuf>,
    /// (archive, member file name)
    members: Vec<(PathBuf, String)>,
}

/// lld `--trace` output: one loaded input per line, `archive(member)` for each archive member it loaded (archives themselves are not listed); shared objects are listed too and skipped.
fn parse_trace(text: &str) -> Loaded {
    let mut loaded = Loaded::default();
    for line in lines(text) {
        if let Some(rest) = line.strip_suffix(')')
            && let Some((archive, member)) = rest.rsplit_once('(')
        {
            loaded.members.push((archive.into(), member.to_owned()));
        } else if line.ends_with(".o") {
            loaded.objects.push(line.into());
        }
    }
    loaded
}

/// `ninja -t query <archive>...`: archive -> the explicit inputs of its edge (`input: <rule>` section; implicit `| x` and order-only `|| x` entries skipped).
fn parse_query(text: &str) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut out: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let (mut target, mut inputs) = (None::<PathBuf>, false);
    for line in text.lines() {
        if !line.starts_with(' ') {
            target = line.strip_suffix(':').map(PathBuf::from);
            inputs = false;
        } else if let Some(section) = line.strip_prefix("  ")
            && !section.starts_with(' ')
        {
            inputs = section.starts_with("input:");
        } else if inputs && let Some(t) = &target {
            let input = line.trim();
            if !input.starts_with('|') {
                out.entry(t.clone()).or_default().push(input.into());
            }
        }
    }
    out
}

/// The files `output` is made of: its depfile record, where an entry with a record of its own (a build output: an rlib a Rust edge links) stands for what that one is made of, transitively.
fn made_of<'a>(output: &'a Path, deps: &'a BTreeMap<PathBuf, Vec<PathBuf>>) -> BTreeSet<&'a Path> {
    let (mut files, mut seen, mut todo) = (BTreeSet::new(), BTreeSet::from([output]), vec![output]);
    while let Some(o) = todo.pop() {
        for f in deps.get(o).into_iter().flatten() {
            if !deps.contains_key(f) {
                files.insert(f.as_path());
            } else if seen.insert(f.as_path()) {
                todo.push(f);
            }
        }
    }
    files
}

/// The roots of every fetched checkout (`checkouts` as sources.toml lists them) plus the AOSP tree itself, for files the manifest's link/copy files put outside any project.
pub fn roots(layout: &atl_layout::Layout, checkouts: &[crate::sources::Checkout]) -> Vec<Root> {
    let mut roots: Vec<Root> = checkouts
        .iter()
        .flat_map(|c| {
            let located = layout.locate(&c.dir);
            // A task copy is bound over its shared-tree place in the build namespace, so depfiles also name files by the shared path -> both places are this project
            let shared = located.task.as_ref().map(|_| layout.src().join(&c.dir));
            [Some(located.dir), shared]
                .into_iter()
                .flatten()
                .filter_map(|dir| std::fs::canonicalize(dir).ok())
                .map(|dir| Root {
                    dir,
                    key: c.licence_key.clone(),
                    prefix: c.licence_prefix(),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    if let Ok(dir) = std::fs::canonicalize(crate::tree::shared_tree(layout)) {
        roots.push(Root {
            dir,
            key: licenses::AOSP.to_owned(),
            prefix: PathBuf::new(),
        });
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ninja build shaped like bp2ninja's (cc with depfile, ar, link writing `$out.trace`): `device/libt.so` links `main.o` and the archive `libz.a` = {used.o, unused.o}, of which only used.o's symbol is referenced.
    /// The gate must deny used.c's licence and its header's, not see unused.c (GPL-3.0-only, not linked), pass main.c (MIT), and fail a file in no checkout.
    #[test]
    fn gate_checks_the_linked_set() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (src, out, elsewhere) = (tmp.join("src"), tmp.join("out"), tmp.join("elsewhere"));
        for d in [&src, &out, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let write = |p: &Path, s: &str| std::fs::write(p, s).unwrap();
        write(
            &src.join("main.c"),
            "// SPDX-License-Identifier: MIT\n#include \"../elsewhere/stray.h\"\nint used(void);\nint entry(void) { return used(); }\n",
        );
        write(
            &src.join("used.c"),
            "// SPDX-License-Identifier: LicenseRef-Bad\n#include \"used.h\"\nint used(void) { return USED; }\n",
        );
        write(
            &src.join("used.h"),
            "// SPDX-License-Identifier: LicenseRef-Bad\n#define USED 1\n",
        );
        write(
            &src.join("unused.c"),
            "// SPDX-License-Identifier: GPL-3.0-only\nint unused(void) { return 2; }\n",
        );
        write(&elsewhere.join("stray.h"), "/* no notice */\n");
        let o = |n: &str| out.join(format!("obj/{n}.c.o")).display().to_string();
        let s = |n: &str| src.join(format!("{n}.c")).display().to_string();
        let lib = out.join("device/libt/libt.so");
        let archive = out.join("device/libz/libz.a");
        write(
            &out.join("build.ninja"),
            &format!(
                "rule cc\n  command = clang-19 -fPIC -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n\
                 rule ar\n  command = rm -f $out && llvm-ar-19 crsD $out $in\n\
                 rule link\n  command = clang-19 -fuse-ld=lld -shared -o $out $in -Wl,--trace >$out.trace\n\
                 build {m}: cc {ms}\nbuild {u}: cc {us}\nbuild {x}: cc {xs}\n\
                 build {a}: ar {u} {x}\nbuild {l}: link {m} {a}\n",
                m = o("main"),
                ms = s("main"),
                u = o("used"),
                us = s("used"),
                x = o("unused"),
                xs = s("unused"),
                a = archive.display(),
                l = lib.display(),
            ),
        );
        let (report, inputs) = build_and_check(&src, &out);
        let canon = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        let (main, used, used_h, unused, stray) = (
            canon(src.join("main.c")),
            canon(src.join("used.c")),
            canon(src.join("used.h")),
            canon(src.join("unused.c")),
            canon(elsewhere.join("stray.h")),
        );
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(report.artifacts, 1);
        let denied: Vec<(&str, Vec<&str>)> = report
            .denied
            .iter()
            .map(|(e, f)| (e.as_str(), f.keys().map(String::as_str).collect()))
            .collect();
        // unused.c's GPL-3.0-only never reaches libt.so: only the member symbol resolution loaded counts
        assert_eq!(denied, [("LicenseRef-Bad", vec!["used.c", "used.h"])]);
        assert_eq!(report.checked, 3);
        assert_eq!(report.unresolved.len(), 1, "{:?}", report.unresolved);
        assert!(report.unresolved[0].contains("stray.h: in no checkout"));
        assert!(!report.ok());
        // the edge's depfile: every reached file (judged or not) and the trace, never the unloaded member's source
        for f in [&main, &used, &used_h, &stray] {
            assert!(
                inputs.contains(f),
                "{} not an input: {inputs:?}",
                f.display()
            );
        }
        assert!(inputs.iter().any(|p| p.ends_with("libt.so.trace")));
        assert!(!inputs.contains(&unused));
    }

    /// `$in` as ninja writes it into an rspfile, read back by real ninja (the oracle): an edge over a plain path, one with `@` (quoted), one with `'` (escaped inside quotes), one with a space.
    #[test]
    fn rspfile_paths_round_trip() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-rsp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let names = ["plain.so", "hal@2.0.so", "it's.so", "a b.so"];
        for n in names {
            std::fs::write(tmp.join(n), "").unwrap();
        }
        let ins: Vec<String> = names.iter().map(|n| n.replace(' ', "$ ")).collect();
        std::fs::write(
            tmp.join("build.ninja"),
            format!(
                "rule keep\n  command = cp $out.rsp $out\n  rspfile = $out.rsp\n  rspfile_content = $in\nbuild list: keep {}\n",
                ins.join(" ")
            ),
        )
        .unwrap();
        let ok = Command::new("ninja")
            .arg("-C")
            .arg(&tmp)
            .status()
            .unwrap()
            .success();
        let text = std::fs::read_to_string(tmp.join("list")).unwrap_or_default();
        std::fs::remove_dir_all(&tmp).unwrap();
        assert!(ok);
        assert_eq!(shell_words(&text), names);
    }

    /// The depfile names a missing input by its nearest existing ancestor (a named missing file reruns the edge on every build) and escapes what ninja's depfile parser reads specially.
    #[test]
    fn depfile_lists_existing_paths_escaped() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-dep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("a b#c");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.h"), "").unwrap();
        std::fs::create_dir_all(dir.join("**")).unwrap();
        std::fs::write(dir.join("**/s.h"), "").unwrap();
        let inputs = Inputs::from([
            dir.join("f.h"),
            dir.join("gone/x.h"),
            dir.join("gone/y.h"),
            dir.join("**/s.h"),
        ]);
        let dep = tmp.join("r.json.d");
        write_depfile(&dep, Path::new("r.json"), &inputs).unwrap();
        let text = std::fs::read_to_string(&dep).unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
        let d = dir
            .display()
            .to_string()
            .replace(' ', "\\ ")
            .replace('#', "\\#");
        // inputs in order: **/s.h -> the dir (`*` does not spell), f.h, gone/x.h -> the dir again (listed once, gone/y.h too)
        assert_eq!(text, format!("r.json: \\\n  {d} \\\n  {d}/f.h\n"));
    }

    /// Runs ninja in `out`, then the gate with `src` as the one checkout (key `aosp`) and only MIT allowed, over every device link output.
    fn build_and_check(src: &Path, out: &Path) -> (Report, Inputs) {
        let status = Command::new("ninja")
            .arg("-C")
            .arg(out)
            .arg("-v")
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stdout)
        );
        let licenses = licenses::parse(
            "[artifact.runtime]\nallow = [\"MIT\"]\n",
            &["aosp".to_owned()].into(),
        )
        .unwrap();
        let roots = [Root {
            dir: std::fs::canonicalize(src).unwrap(),
            key: "aosp".into(),
            prefix: PathBuf::new(),
        }];
        let out = std::fs::canonicalize(out).unwrap();
        let artifacts = crate::runpath_gate::link_outputs(&out, &[out.join("device")]).unwrap();
        let mut inputs = Inputs::new();
        let report = check(
            &out,
            artifacts,
            &roots,
            &licenses,
            &BTreeSet::new(),
            &mut inputs,
        )
        .unwrap();
        (report, inputs)
    }

    /// bp2ninja's Rust shape (rustc rule with dep-info depfile + `$crates`, a generated staticlib of `extern crate` roots): `device/libr.so` links `main.cpp.o` and `librustlibs.a`, built by rustc from root.rs (MIT) over the rlib `libdep.rlib` from dep.rs (LicenseRef-Bad) + its module dep_mod.rs (MIT).
    /// The staticlib's members are codegen units, none of its inputs: the gate must take the archive whole and reach both crates' sources, denying dep.rs; the sysroot's std is reached by no depfile.
    #[test]
    fn gate_follows_rust_crates() {
        let tmp =
            std::env::temp_dir().join(format!("xtask-licence-gate-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (src, out) = (tmp.join("src"), tmp.join("out"));
        for d in [&src, &out] {
            std::fs::create_dir_all(d).unwrap();
        }
        let write = |p: &Path, s: &str| std::fs::write(p, s).unwrap();
        write(
            &src.join("main.cpp"),
            "// SPDX-License-Identifier: MIT\nextern \"C\" int rust_used();\nint entry() { return rust_used(); }\n",
        );
        write(
            &src.join("dep.rs"),
            "// SPDX-License-Identifier: LicenseRef-Bad\nmod dep_mod;\n#[unsafe(no_mangle)]\npub extern \"C\" fn rust_used() -> i32 { dep_mod::VALUE }\n",
        );
        write(
            &src.join("dep_mod.rs"),
            "// SPDX-License-Identifier: MIT\npub const VALUE: i32 = 1;\n",
        );
        write(
            &src.join("root.rs"),
            "// SPDX-License-Identifier: MIT\nextern crate dep;\n",
        );
        let rlib = out.join("device/libdep/libdep.rlib");
        let staticlib = out.join("device/libr/generated_rust_staticlib/librustlibs.a");
        let lib = out.join("device/libr/libr.so");
        let main = out.join("obj/main.cpp.o");
        write(
            &out.join("build.ninja"),
            &format!(
                "rule cc\n  command = clang++-19 -fPIC -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n\
                 rule rustc\n  command = rustc $flags --emit link -o $out --emit dep-info=$out.d.raw $in $libs && line=$$(grep ^$out: $out.d.raw) && printf '%s ' \"$$line\" $crates > $out.d\n  depfile = $out.d\n  deps = gcc\n\
                 rule link\n  command = clang++-19 -fuse-ld=lld -shared -o $out $in -Wl,--trace >$out.trace\n\
                 build {m}: cc {ms}\n\
                 build {d}: rustc {ds}\n  flags = --crate-type=rlib --crate-name=dep -C panic=abort --edition=2021\n\
                 build {s}: rustc {rs} | {d}\n  flags = --crate-type=staticlib --crate-name=generated_rust_staticlib -C panic=abort --edition=2021\n  libs = --extern dep={d} -L dependency={dd}\n  crates = {d}\n\
                 build {l}: link {m} {s}\n",
                m = main.display(),
                ms = src.join("main.cpp").display(),
                d = rlib.display(),
                ds = src.join("dep.rs").display(),
                dd = rlib.parent().unwrap().display(),
                s = staticlib.display(),
                rs = src.join("root.rs").display(),
                l = lib.display(),
            ),
        );
        let (report, _) = build_and_check(&src, &out);
        std::fs::remove_dir_all(&tmp).unwrap();
        let denied: Vec<(&str, Vec<&str>)> = report
            .denied
            .iter()
            .map(|(e, f)| (e.as_str(), f.keys().map(String::as_str).collect()))
            .collect();
        assert_eq!(denied, [("LicenseRef-Bad", vec!["dep.rs"])]);
        // main.cpp, root.rs, dep.rs, dep_mod.rs
        assert_eq!(report.checked, 4);
        assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    }

    #[test]
    fn trace_lists_loaded_objects_and_members() {
        // lld --trace of `clang++ -fuse-ld=lld -o m m.o lib.a` where m.o needs a() from lib.a(a.o) but nothing from b.o (lld 19.1.7, measured)
        let text =
            "/lib/x86_64-linux-gnu/Scrt1.o\nm.o\nlib.a(a.o)\n/lib/x86_64-linux-gnu/libc.so.6\n";
        assert_eq!(
            parse_trace(text),
            Loaded {
                objects: vec!["/lib/x86_64-linux-gnu/Scrt1.o".into(), "m.o".into()],
                members: vec![("lib.a".into(), "a.o".to_owned())],
            }
        );
    }

    #[test]
    fn query_inputs_of_archives() {
        // ninja 1.12 `-t query` layout
        let text = "/o/libx.a:\n  input: ar\n    /o/obj/a.cpp.o\n    | /o/implicit\n  outputs:\n    /o/libuser.so\n\
                    /o/liby.a:\n  input: ar\n    /o/obj/b.c.o\n";
        let q = parse_query(text);
        assert_eq!(q[Path::new("/o/libx.a")], [PathBuf::from("/o/obj/a.cpp.o")]);
        assert_eq!(q[Path::new("/o/liby.a")], [PathBuf::from("/o/obj/b.c.o")]);
    }

    /// A staged copy identical to a source of the second `from` dir is that source; a changed one (configure_file output) stays, to be judged on its own notice.
    #[test]
    fn staged_copies_map_to_their_source() {
        let tmp = std::env::temp_dir().join(format!("xtask-staged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (cxx, abi, dir) = (
            tmp.join("libcxx"),
            tmp.join("libcxxabi"),
            tmp.join("build/v1"),
        );
        for d in [&cxx, &abi, &dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(abi.join("cxxabi.h"), "abi").unwrap();
        std::fs::write(dir.join("cxxabi.h"), "abi").unwrap();
        std::fs::write(cxx.join("__config_site"), "template").unwrap();
        std::fs::write(dir.join("__config_site"), "configured").unwrap();
        let staged = [Staged {
            dir: dir.clone(),
            from: vec![cxx.clone(), abi.clone()],
            key: "llvm".into(),
        }];
        let o = Path::new("x.o");
        let placed = BTreeMap::from([
            (dir.join("cxxabi.h"), o),
            (dir.join("__config_site"), o),
            (tmp.join("other.c"), o),
        ]);
        let (kept, configured) = unstage(placed, &staged);
        assert_eq!(
            kept.keys().cloned().collect::<Vec<_>>(),
            vec![abi.join("cxxabi.h"), tmp.join("other.c")]
        );
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].0, dir.join("__config_site"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An archive in a CMake build dir, or in an install prefix beside one, belongs to that build; the main build's own dir never counts.
    #[test]
    fn external_build_by_layout() {
        let tmp = std::env::temp_dir().join(format!("xtask-external-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let out = tmp.join("out");
        let rt = out.join("upstream/llvm/x86_64/libunwind/build");
        let up = out.join("upstream/ssl");
        for d in [
            rt.join("lib"),
            up.join("build"),
            up.join("prefix/lib"),
            out.join("device"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in [
            out.join("build.ninja"),
            rt.join("build.ninja"),
            up.join("build/build.ninja"),
        ] {
            std::fs::write(f, "").unwrap();
        }
        assert_eq!(
            external_build(&rt.join("lib/libunwind.a"), &out),
            Some(rt.clone())
        );
        assert_eq!(
            external_build(&up.join("prefix/lib/libssl.a"), &out),
            Some(up.join("build"))
        );
        assert_eq!(external_build(&out.join("device/liba.a"), &out), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn files_placed_by_innermost_checkout() {
        let roots = [
            Root {
                dir: "/src/aosp".into(),
                key: "aosp".into(),
                prefix: PathBuf::new(),
            },
            // a task copy of an AOSP project lives outside the shared tree
            Root {
                dir: "/src-wt/t/aosp/external/skia".into(),
                key: "aosp".into(),
                prefix: "external/skia".into(),
            },
            Root {
                dir: "/src/boringssl".into(),
                key: "boringssl".into(),
                prefix: PathBuf::new(),
            },
        ];
        let out = Path::new("/out/n");
        let up = |root: usize, rel: &str| Place::Upstream {
            root: &roots[root],
            rel: rel.to_owned(),
        };
        assert_eq!(
            place(
                Path::new("/src/aosp/external/dng_sdk/source/a.cpp"),
                &roots,
                out
            ),
            up(0, "external/dng_sdk/source/a.cpp")
        );
        assert_eq!(
            place(
                Path::new("/src-wt/t/aosp/external/skia/src/x.cpp"),
                &roots,
                out
            ),
            up(1, "external/skia/src/x.cpp")
        );
        assert_eq!(
            place(Path::new("/src/boringssl/crypto/a.cc"), &roots, out),
            up(2, "crypto/a.cc")
        );
        assert_eq!(
            place(Path::new("/out/n/gen/x.h"), &roots, out),
            Place::Generated
        );
        assert_eq!(
            place(Path::new("/usr/include/stdio.h"), &roots, out),
            Place::System
        );
        // `/usr` is a component, not a string prefix
        assert_eq!(place(Path::new("/usrx/a.h"), &roots, out), Place::Unknown);
        assert_eq!(
            place(Path::new("/home/me/a.h"), &roots, out),
            Place::Unknown
        );
    }
}
