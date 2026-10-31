// SPDX-License-Identifier: MIT
//! The licence gate after `xtask native`'s build (CLAUDE.md licence rules): every file that reaches a shipped artifact carries a licence licenses.toml allows in it.
//! - Shipped artifacts: the device-side link outputs ninja produced (`ninja -t targets rule link`, under `<out>/device/`), all of class `runtime`. Host outputs are build tools and never ship.
//! - Reached files, per artifact: the objects its link loaded, from `<artifact>.trace` (bp2ninja's link rule runs ld with `-t -t`: each object on the command line and each archive member loaded -- those symbol resolution pulled in, every member of a `--whole-archive`); a member is mapped back to its object through the archive's ninja edge (`ninja -t query`). Members are named by file name, so where two objects of one archive share one, both count.
//!   Then each object's source and headers, from its depfile (`ninja -t deps`).
//! - A file's licence: `notice::resolve` with the `[upstream.<key>]` of the checkout holding it; AOSP projects share key `aosp`, their paths relative to the tree root (`external/dng_sdk/...`).
//!
//! Not checked, only counted: files generated into `<out>` (aidl, protobuf, sysprop, genrule outputs, our crates' generated headers; their generators' inputs are not followed); members of archives built outside ninja (providers.toml upstreams, by their own build); system files under `/usr` and `/lib*` (toolchain, distro headers and libraries: the platform's, not ours to ship).
//! A file anywhere else fails: nothing passes because the gate cannot place it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::licenses::{self, Licenses};
use crate::notice;

/// Toolchain and distro files: the platform's.
const SYSTEM_DIRS: &[&str] = &["/usr", "/lib", "/lib64"];

/// A checkout as the gate places files: its canonical directory, its licence key, and where it sits inside its upstream (an AOSP project's tree path; empty for other upstreams).
#[derive(Debug)]
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
}

impl Report {
    pub fn ok(&self) -> bool {
        self.denied.is_empty() && self.unresolved.is_empty()
    }
}

/// Runs the gate over the build directory `out` (canonical).
pub fn check(out: &Path, roots: &[Root], licenses: &Licenses) -> Result<Report> {
    let device = out.join("device");
    let artifacts: Vec<PathBuf> = lines(&ninja(out, &["-t", "targets", "rule", "link"])?)
        .map(PathBuf::from)
        .filter(|p| p.starts_with(&device) && p.is_file())
        .collect();
    let mut report = Report {
        artifacts: artifacts.len(),
        ..Report::default()
    };
    // object -> first artifact loading it
    let mut objects: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    let mut members: Vec<(PathBuf, String, &Path)> = Vec::new();
    for artifact in &artifacts {
        let trace = trace_of(artifact);
        let text = std::fs::read_to_string(&trace)
            .with_context(|| format!("reading {} (linked without a trace?)", trace.display()))?;
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
    for (archive, member, artifact) in &members {
        let Some(objs) = inputs.get(archive) else {
            report.outside += 1;
            continue;
        };
        let matching: Vec<&PathBuf> = objs
            .iter()
            .filter(|o| o.file_name().is_some_and(|n| *n == **member))
            .collect();
        ensure!(
            !matching.is_empty(),
            "{}: member {member} is none of its ninja inputs",
            archive.display()
        );
        for o in matching {
            objects.entry(o.clone()).or_insert(artifact);
        }
    }
    let deps = parse_deps(&ninja(out, &["-t", "deps"])?);
    // file -> first artifact reaching it
    let mut files: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (object, artifact) in &objects {
        if is_system(object) {
            continue;
        }
        let Some(list) = deps.get(object) else {
            report
                .unresolved
                .push(format!("{}: no depfile record", object.display()));
            continue;
        };
        for f in list {
            files.entry(f.clone()).or_insert(artifact);
        }
    }
    let mut placed: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for (file, artifact) in files {
        let real = std::fs::canonicalize(&file).unwrap_or(file);
        placed.entry(real).or_insert(artifact);
    }
    for (file, artifact) in placed {
        let name = artifact
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match place(&file, roots, out) {
            Place::Upstream { key, rel } => {
                let text = String::from_utf8_lossy(
                    &std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?,
                )
                .into_owned();
                match notice::resolve(&rel, &text, licenses.upstream.get(key)) {
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
    Ok(report)
}

/// The linker trace bp2ninja's link rule writes beside an artifact.
fn trace_of(artifact: &Path) -> PathBuf {
    let mut name = artifact.as_os_str().to_owned();
    name.push(".trace");
    PathBuf::from(name)
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
    Upstream { key: &'a str, rel: String },
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
        return Place::Upstream { key: &r.key, rel };
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

/// GNU ld `-t -t` output: one input per line, `(archive)member` for each archive member it loaded; archives and shared objects themselves are listed too and skipped.
fn parse_trace(text: &str) -> Loaded {
    let mut loaded = Loaded::default();
    for line in lines(text) {
        if let Some(rest) = line.strip_prefix('(')
            && let Some((archive, member)) = rest.split_once(')')
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

/// The roots of every fetched checkout (`checkouts` as sources.toml lists them) plus the AOSP tree itself, for files the manifest's link/copy files put outside any project.
pub fn roots(layout: &atl_layout::Layout, checkouts: &[crate::sources::Checkout]) -> Vec<Root> {
    let aosp = Path::new("aosp");
    let mut roots: Vec<Root> = checkouts
        .iter()
        .filter_map(|c| {
            let dir = std::fs::canonicalize(layout.locate(&c.dir).dir).ok()?;
            let prefix = c
                .dir
                .strip_prefix(aosp)
                .map_or_else(|_| PathBuf::new(), Path::to_path_buf);
            Some(Root {
                dir,
                key: c.licence_key.clone(),
                prefix,
            })
        })
        .collect();
    if let Ok(dir) = std::fs::canonicalize(crate::farm::shared_tree(layout)) {
        roots.push(Root {
            dir,
            key: "aosp".to_owned(),
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
                 rule link\n  command = clang-19 -shared -o $out $in -Wl,-t,-t >$out.trace\n\
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
        let status = Command::new("ninja")
            .arg("-C")
            .arg(&out)
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
            dir: std::fs::canonicalize(&src).unwrap(),
            key: "aosp".into(),
            prefix: PathBuf::new(),
        }];
        let report = check(&std::fs::canonicalize(&out).unwrap(), &roots, &licenses).unwrap();
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

    #[test]
    fn trace_lists_loaded_objects_and_members() {
        // ld -t -t of `clang++ -o m m.o lib.a` where m.o needs a() from lib.a(a.o) but nothing from b.o (binutils 2.44, measured)
        let text = "/lib/x86_64-linux-gnu/Scrt1.o\nm.o\nlib.a\n(lib.a)a.o\n/lib/x86_64-linux-gnu/libc.so.6\n";
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
        let up = |key: &'static str, rel: &str| Place::Upstream {
            key,
            rel: rel.to_owned(),
        };
        assert_eq!(
            place(
                Path::new("/src/aosp/external/dng_sdk/source/a.cpp"),
                &roots,
                out
            ),
            up("aosp", "external/dng_sdk/source/a.cpp")
        );
        assert_eq!(
            place(
                Path::new("/src-wt/t/aosp/external/skia/src/x.cpp"),
                &roots,
                out
            ),
            up("aosp", "external/skia/src/x.cpp")
        );
        assert_eq!(
            place(Path::new("/src/boringssl/crypto/a.cc"), &roots, out),
            up("boringssl", "crypto/a.cc")
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
