// SPDX-License-Identifier: MIT
//! The licence gate after `xtask native`'s build (CLAUDE.md licence rules): every file that reaches a shipped artifact carries a licence licenses.toml allows in it.
//! - Shipped artifacts: the device-side link outputs ninja produced (`ninja -t targets rule link`, under `<out>/device/`), all of class `runtime`. Host outputs are build tools and never ship.
//! - Reached files, per artifact: the objects its link loaded, from `<artifact>.trace` (bp2ninja's link rule runs lld with `--trace`: each object on the command line and each archive member loaded -- those symbol resolution pulled in, every member of a `--whole-archive`); a member is mapped back to its object through the archive's ninja edge (`ninja -t query`). Members are named by file name, so where two objects of one archive share one, both count.
//!   An archive whose members are none of its inputs but which has a depfile record was compiled whole by its own edge (rustc's generated staticlib): the archive is the object, whichever of its codegen units ld loaded.
//!   Then each object's source and headers, from its depfile (`ninja -t deps`); a depfile entry with a record of its own (an rlib, which bp2ninja's rustc rule lists after rustc's dep-info) stands for what it is made of, transitively. So a Rust archive reaches every crate of its closure's sources; the toolchain sysroot's std/core/alloc are rustc's own inputs, in no depfile, and are not reached -- like the C toolchain's crt objects and libc++, the platform's.
//! - A file's licence: `notice::resolve` with the `[upstream.<key>]` of the checkout holding it; AOSP projects share key `aosp`, their paths relative to the tree root (`external/dng_sdk/...`).
//!
//! Not checked, only counted: files generated into `<out>` (aidl, protobuf, sysprop, genrule outputs, our crates' generated headers; their generators' inputs are not followed); members of archives built outside ninja (providers.toml upstreams, by their own build); system files under `/usr` and `/lib*` (toolchain, distro headers and libraries: the platform's, not ours to ship).
//! A file anywhere else fails: nothing passes because the gate cannot place it.
//! Stale artifacts fail unread: after a failed build an artifact whose relink did not run is an earlier link's, and its trace may name archive members the current graph lacks (`relinked`).

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

/// One gate run.
#[derive(Debug, Default)]
pub struct Report {
    pub artifacts: usize,
    /// Files whose licence was resolved and checked.
    pub checked: usize,
    pub generated: usize,
    pub system: usize,
    /// Archive members of libraries built outside ninja.
    pub outside: usize,
    /// Licence expression -> files under it (as resolved: key-relative) -> the first artifact reaching it.
    pub denied: BTreeMap<String, BTreeMap<String, String>>,
    /// Files with no licence the rules can name, or that the gate cannot place.
    pub unresolved: Vec<String>,
    /// Link outputs ninja would link again (their link or one of its inputs failed): file and trace are an earlier link's, the trace may name members their archives no longer have, so neither is read. Never a pass.
    pub stale: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.denied.is_empty() && self.unresolved.is_empty() && self.stale.is_empty()
    }
}

/// Runs the gate over the build directory `out` (canonical). It runs after failed builds too, to report them: a state the build left behind is a report line, never an error.
pub fn check(out: &Path, roots: &[Root], licenses: &Licenses) -> Result<Report> {
    let device = out.join("device");
    let mut artifacts: Vec<PathBuf> = lines(&ninja(out, &["-t", "targets", "rule", "link"])?)
        .map(PathBuf::from)
        .filter(|p| p.starts_with(&device) && p.is_file())
        .collect();
    let stale = relinked(out, &artifacts);
    let mut report = Report::default();
    artifacts.retain(|a| {
        let keep = !stale.contains(a);
        if !keep {
            report.stale.push(a.display().to_string());
        }
        keep
    });
    report.artifacts = artifacts.len();
    // object -> first artifact loading it
    let mut objects: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    let mut members: Vec<(PathBuf, String, &Path)> = Vec::new();
    for artifact in &artifacts {
        let trace = trace_of(artifact);
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
    let inputs = if archives.is_empty() {
        BTreeMap::new()
    } else {
        parse_query(&ninja(
            out,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?)
    };
    let deps = parse_deps(&ninja(out, &["-t", "deps"])?);
    for (archive, member, artifact) in &members {
        let Some(objs) = inputs.get(archive) else {
            report.outside += 1;
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
    let mut placed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (file, artifact) in files {
        let real = std::fs::canonicalize(&file).unwrap_or(file);
        placed.entry(real).or_insert(artifact);
    }
    judge(placed, roots, out, licenses, &mut report)?;
    Ok(report)
}

/// Every file an object of the CMake-generated ninja build in `build` (canonical) was compiled from, by its depfiles: what the runtimes build (`xtask runtimes`) can put into an archive.
/// All objects of the build, not only the archive members a later link loads: a superset, so a pass here holds for any link of its archives.
pub fn check_build(build: &Path, roots: &[Root], licenses: &Licenses) -> Result<Report> {
    let deps = parse_deps(&ninja(build, &["-t", "deps"])?);
    let mut report = Report {
        artifacts: deps.len(),
        ..Report::default()
    };
    let mut placed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (object, files) in &deps {
        for f in files {
            let abs = if f.is_absolute() {
                f.clone()
            } else {
                build.join(f)
            };
            let real = std::fs::canonicalize(&abs).unwrap_or(abs);
            placed.entry(real).or_insert(object.as_path());
        }
    }
    judge(placed, roots, build, licenses, &mut report)?;
    Ok(report)
}

/// Resolves and checks the licence of each placed file (canonical path -> the first artifact reaching it); files under `out` count as generated.
fn judge(
    placed: BTreeMap<PathBuf, &Path>,
    roots: &[Root],
    out: &Path,
    licenses: &Licenses,
    report: &mut Report,
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
                let fallback = || licenses.unlabelled(&root.key, &root.prefix, &root.dir, at);
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

/// The artifacts whose link edge ninja would run (dry run, `-k 0`: past edges that would fail): those a failed build left as an earlier link's output.
/// Read from the dry run's `[i/n] link <out>` lines, bp2ninja's link rule description. A dry run that cannot start leaves every artifact stale: what it would have told is unknown.
fn relinked(out: &Path, artifacts: &[PathBuf]) -> BTreeSet<PathBuf> {
    if artifacts.is_empty() {
        return BTreeSet::new();
    }
    // its exit status is not read: a dry run fails where a missing input has no rule, and still prints every edge it would run
    let Ok(output) = Command::new("ninja")
        .arg("-C")
        .arg(out)
        .args(["-n", "-k", "0"])
        .args(artifacts)
        .output()
    else {
        return artifacts.iter().cloned().collect();
    };
    lines(&String::from_utf8_lossy(&output.stdout))
        .filter_map(|l| {
            let (_, rest) = l.split_once("] link ")?;
            l.starts_with('[').then(|| PathBuf::from(rest))
        })
        .collect()
}

fn ninja(out: &Path, args: &[&str]) -> Result<String> {
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

fn lines(text: &str) -> impl Iterator<Item = &str> {
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

/// `ninja -t deps`: target -> the dependencies its depfile recorded (an object's source first, then its headers).
fn parse_deps(text: &str) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let mut out: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut target = None::<PathBuf>;
    for line in text.lines() {
        if line.starts_with(' ') {
            if let Some(t) = &target {
                out.entry(t.clone()).or_default().push(line.trim().into());
            }
        } else {
            target = line.rsplit_once(": #deps").map(|(t, _)| PathBuf::from(t));
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
        .filter_map(|c| {
            let dir = std::fs::canonicalize(layout.locate(&c.dir).dir).ok()?;
            Some(Root {
                dir,
                key: c.licence_key.clone(),
                prefix: c.licence_prefix(),
            })
        })
        .collect();
    if let Ok(dir) = std::fs::canonicalize(crate::farm::shared_tree(layout)) {
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
        let report = build_and_check(&src, &out);
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
    }

    /// After a failed build: libt.so linked once from main.o + libz.a(used.o); then libz.a's input is renamed (used.o -> renamed.o) and main.c breaks, so its relink never runs.
    /// libt.so and its trace stay from the first link and name libz.a(used.o), a member no longer among libz.a's ninja inputs: the gate must report libt.so stale and read neither, not fail.
    #[test]
    fn gate_reports_a_stale_artifact() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (src, out) = (tmp.join("src"), tmp.join("out"));
        for d in [&src, &out] {
            std::fs::create_dir_all(d).unwrap();
        }
        let write = |p: &Path, s: &str| std::fs::write(p, s).unwrap();
        write(
            &src.join("main.c"),
            "// SPDX-License-Identifier: MIT\nint used(void);\nint entry(void) { return used(); }\n",
        );
        for n in ["used", "renamed"] {
            write(
                &src.join(format!("{n}.c")),
                "// SPDX-License-Identifier: MIT\nint used(void) { return 1; }\n",
            );
        }
        let lib = out.join("device/libt/libt.so");
        let ninja = |member: &str| {
            format!(
                "rule cc\n  command = clang-19 -fPIC -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n\
                 rule ar\n  command = rm -f $out && llvm-ar-19 crsD $out $in\n\
                 rule link\n  command = clang-19 -fuse-ld=lld -shared -o $out $in -Wl,--trace >$out.trace\n  description = link $out\n\
                 build {o}/main.o: cc {s}/main.c\nbuild {o}/{member}.o: cc {s}/{member}.c\n\
                 build {o}/libz.a: ar {o}/{member}.o\nbuild {l}: link {o}/main.o {o}/libz.a\n",
                o = out.display(),
                s = src.display(),
                l = lib.display(),
            )
        };
        write(&out.join("build.ninja"), &ninja("used"));
        let report = build_and_check(&src, &out);
        assert!(report.ok() && report.artifacts == 1, "{report:?}");
        write(&out.join("build.ninja"), &ninja("renamed"));
        write(&src.join("main.c"), "int broken(\n");
        let failed = Command::new("ninja").arg("-C").arg(&out).output().unwrap();
        assert!(!failed.status.success() && lib.is_file());
        let roots = [Root {
            dir: std::fs::canonicalize(&src).unwrap(),
            key: "aosp".into(),
            prefix: PathBuf::new(),
        }];
        let licenses = licenses::parse(
            "[artifact.runtime]\nallow = [\"MIT\"]\n",
            &["aosp".to_owned()].into(),
        )
        .unwrap();
        let report = check(&std::fs::canonicalize(&out).unwrap(), &roots, &licenses);
        std::fs::remove_dir_all(&tmp).unwrap();
        let report = report.expect("the gate reports a stale artifact, it does not fail");
        assert_eq!(report.stale, [lib.display().to_string()]);
        assert_eq!((report.artifacts, report.checked), (0, 0));
        assert!(!report.ok());
    }

    /// Runs ninja in `out`, then the gate with `src` as the one checkout (key `aosp`) and only MIT allowed.
    fn build_and_check(src: &Path, out: &Path) -> Report {
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
        check(&std::fs::canonicalize(out).unwrap(), &roots, &licenses).unwrap()
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
        let report = build_and_check(&src, &out);
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

    #[test]
    fn deps_by_object() {
        let text = "/o/a.o: #deps 2, deps mtime 1 (VALID)\n    /t/a.cpp\n    /t/a.h\n\n/o/b.o: #deps 1, deps mtime 1 (STALE)\n    /t/b.c\n\n";
        let d = parse_deps(text);
        assert_eq!(
            d[Path::new("/o/a.o")],
            [PathBuf::from("/t/a.cpp"), PathBuf::from("/t/a.h")]
        );
        assert_eq!(d[Path::new("/o/b.o")], [PathBuf::from("/t/b.c")]);
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
