// SPDX-License-Identifier: MIT
//! `xtask native`: AOSP native modules built for this Linux host by our Blueprint converter
//! (`tools/bp2ninja`, plan D7-2): builds the converter with the distro Go against the pinned
//! `build/blueprint` (through a generated go.work: that path differs per checkout), builds the
//! providers.toml modules provided outside the tree (`crate:` with cargo-c, `upstream:` by their
//! recipe), converts the goals' closure into ninja, builds it with `ninja -k 0`, summarises the
//! failures by module and runs the provided gate (provided_gate.rs).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Index, Manifest, Prebuilts};
use crate::providers::Provided;
use crate::sources::Pin;
use crate::{
    arches, farm, fetch, patch, provided_gate, providers, sources, triage, upstream, workspace,
};

#[derive(clap::Args)]
pub struct Args {
    /// Android.bp module names to build (with their dependency closure).
    #[arg(required = true)]
    goals: Vec<String>,
    /// The host arch (an arches.toml row with `[host]`); also Soong's arch name.
    #[arg(long)]
    host: String,
    /// C compiler (deps.toml row `clang`).
    #[arg(long, default_value = "clang-19")]
    cc: String,
    /// C++ compiler and link driver (deps.toml row `clang`).
    #[arg(long, default_value = "clang++-19")]
    cxx: String,
    /// Archiver (deps.toml row `llvm`).
    #[arg(long, default_value = "llvm-ar-19")]
    ar: String,
    /// Before building, convert again and again: pin and fetch the AOSP projects behind the modules and
    /// tree paths the converter reports missing (`missing.txt`, `missing-paths.txt`), until a round finds nothing new to fetch. Ninja
    /// runs once, after that.
    #[arg(long)]
    fetch_missing: bool,
    /// Build directory (default `<workspace>/out/native/<host>`): concurrent checkouts keep their outputs apart.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// Failure summary: every distinct error message, undefined symbol and failed dependency per module (default: the first few; ninja.log keeps the full output).
    #[arg(long, short)]
    verbose: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let arches = arches::load(root)?;
    let (pointer_bits, arch_variant) = match arches.get(&args.host) {
        Some(a) if a.host.is_some() => (
            a.pointer_bits.to_string(),
            a.soong_arch_variant.clone().unwrap_or_default(),
        ),
        _ => anyhow::bail!("{}: not a host arch in {}", args.host, arches::FILE),
    };
    let layout = Layout::of(root)?;
    let ws = layout.workspace();
    // regenerated every round: --fetch-missing adds projects between rounds
    let aosp_tree = || -> Result<std::path::PathBuf> {
        let sources = sources::load(root)?;
        let manifest = aosp::load(root, &sources.aosp)?;
        farm::aosp_tree(&layout, &sources.checkouts(None), manifest.as_ref())
    };
    let tree = aosp_tree()?;
    if tree != farm::shared_tree(&layout) {
        println!("native: AOSP tree with task copies: {}", tree.display());
    }
    // go.work and the converter binary name this checkout's tools/bp2ninja: with an explicit --out they live there too, so concurrent checkouts do not overwrite each other's
    let (out, tool_dir) = match &args.out {
        Some(dir) => {
            let dir = std::path::absolute(dir)?;
            (dir.clone(), dir.join("bp2ninja"))
        }
        None => (
            ws.join("out").join("native").join(&args.host),
            ws.join("out").join("bp2ninja"),
        ),
    };
    std::fs::create_dir_all(&out)?;
    std::fs::create_dir_all(&tool_dir)?;
    let go_work = tool_dir.join("go.work");
    std::fs::write(
        &go_work,
        format!(
            "go 1.24\n\nuse {}\n\nreplace github.com/google/blueprint => {}\n",
            root.join("tools/bp2ninja").display(),
            tree.join("build/blueprint").display()
        ),
    )?;
    let converter = tool_dir.join("bp2ninja");
    let status = Command::new("go")
        .args(["build", "-o"])
        .arg(&converter)
        .arg(".")
        .current_dir(root.join("tools/bp2ninja"))
        .env("GOWORK", &go_work)
        // the pinned blueprint is the only dependency: no toolchain or module downloads
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .status()
        .context("running go build")?;
    ensure!(status.success(), "building bp2ninja failed");
    let sources = sources::load(root)?;
    let workspace = workspace::load(root)?;
    let providers = providers::load(root, &providers::Targets::new(&workspace.crates, &sources))?;
    let omit = providers::omitted(&providers).join(",");
    patch::preflight(root, providers::upstreams(&providers))?;
    // the tree first matches sources.toml: a pinned project absent or cut off mid-clone would never be
    // fetched by the loop, which only adds unpinned ones
    if args.fetch_missing {
        fetch::add_and_fetch(root, &[])?;
    }
    // modules built outside the tree, before the converter: it links their artifacts; one entry per module whatever its row kind (crates by cargo-c, upstreams by their own build system)
    let mut provided = build_crates(root, &out, &providers)?;
    let toolchain = upstream::Toolchain {
        cc: &args.cc,
        cxx: &args.cxx,
    };
    let checkouts = sources.checkouts(None);
    for name in providers::upstreams(&providers) {
        let recipe = upstream::recipe(name).context("providers.toml checked the recipe")?;
        let c = checkouts
            .iter()
            .find(|c| matches!(&c.pin, Pin::Git(n) if n == name))
            .with_context(|| format!("providers.toml upstream {name} has no sources.toml row"))?;
        let src = layout.locate(&c.dir).dir;
        println!("native: building upstream {name} from {}", src.display());
        provided.extend(upstream::build(&src, &out, recipe, &toolchain)?);
    }
    let provided_file = out.join("provided.json");
    std::fs::write(&provided_file, serde_json::to_string_pretty(&provided)?)?;
    let mut round = 0;
    loop {
        round += 1;
        let status = Command::new(&converter)
            .arg("-tree")
            .arg(aosp_tree()?)
            .arg("-out")
            .arg(&out)
            .args([
                "-arch",
                &args.host,
                "-pointer-bits",
                &pointer_bits,
                "-arch-variant",
                &arch_variant,
                "-cc",
                &args.cc,
                "-cxx",
                &args.cxx,
                "-ar",
                &args.ar,
                "-omit",
                &omit,
            ])
            .arg("-provided")
            .arg(&provided_file)
            .args(&args.goals)
            .status()
            .context("running bp2ninja")?;
        ensure!(status.success(), "bp2ninja failed");
        let problems = std::fs::read_to_string(out.join("problems.txt")).unwrap_or_default();
        if args.fetch_missing {
            let missing = std::fs::read_to_string(out.join("missing.txt")).unwrap_or_default();
            let paths = std::fs::read_to_string(out.join("missing-paths.txt")).unwrap_or_default();
            let sources = sources::load(root)?;
            let index = Index::load(root, &sources.aosp)?;
            let manifest = aosp::load(root, &sources.aosp)?
                .context("the AOSP manifest checkout is missing: run `cargo xtask fetch`")?;
            let pinned: BTreeSet<&str> = sources.aosp.projects.keys().map(String::as_str).collect();
            let found = Found {
                modules: &missing,
                paths: &paths,
                problems: &problems,
            };
            let (want, unresolved) = wanted(&found, &index, &manifest, &pinned);
            println!(
                "native: round {round}: {} modules + {} paths missing, {} projects to fetch",
                lines(&missing).count(),
                lines(&paths).count(),
                want.len()
            );
            if !want.is_empty() {
                for (project, why) in &want {
                    println!("native: + {project:<40} {why}");
                }
                fetch::add_and_fetch(root, &want.into_keys().collect::<Vec<_>>())?;
                continue;
            }
            for u in &unresolved {
                println!("native: unresolved {u}");
            }
            println!("native: nothing new to fetch; building");
        }
        for p in problems.lines().filter(|l| !l.is_empty()) {
            println!("native: problem {p}");
        }
        break;
    }
    let log = out.join("ninja.log");
    let status = Command::new("ninja")
        .args(["-k", "0", "-C"])
        .arg(&out)
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(out.join("ninja.err"))?)
        .status()
        .context("running ninja")?;
    let gate_ok = gate(&out, &provided)?;
    if status.success() {
        println!("native: built -> {}", out.display());
        return Ok(if gate_ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let text = std::fs::read_to_string(&log)?;
    let prefixes = [
        format!("{}/", out.display()),
        format!("{}/", tree.display()),
    ];
    let failures = triage::failures(&text, &prefixes.each_ref().map(String::as_str));
    if failures.is_empty() {
        // ninja failed without a failed step: it could not load the graph (duplicate rule, missing input)
        let err = std::fs::read_to_string(out.join("ninja.err")).unwrap_or_default();
        bail!(
            "ninja failed before building a step: {}",
            err.lines().next().unwrap_or("(no ninja.err)")
        );
    }
    let total: usize = failures.values().map(|f| f.steps).sum();
    println!(
        "native: {total} failed steps in {} modules (log {})",
        failures.len(),
        log.display()
    );
    let graph_file = out.join("graph.json");
    let graph: triage::Graph = serde_json::from_str(
        &std::fs::read_to_string(&graph_file)
            .with_context(|| format!("reading {}", graph_file.display()))?,
    )
    .with_context(|| format!("parsing {}", graph_file.display()))?;
    let report = triage::rank(&graph, &failures);
    for line in triage::render(&report, &failures, args.verbose) {
        println!("native: {line}");
    }
    Ok(ExitCode::FAILURE)
}

/// Builds every `crate:` row's crate into `<out>/crates/<crate>` (the shared library) and
/// `<out>/gen/<crate>/include` (its generated headers), with cargo-c; one cargo target dir per build
/// directory, so concurrent checkouts do not share artifacts.
fn build_crates(
    root: &Path,
    out: &Path,
    providers: &providers::Providers,
) -> Result<BTreeMap<String, Provided>> {
    let mut built = BTreeMap::new();
    for (module, krate) in providers::crates(providers) {
        let prefix = out.join("crates").join(krate);
        let include = out.join("gen").join(krate).join("include");
        // headers the crate stopped generating must not stay on the include path
        if include.exists() {
            std::fs::remove_dir_all(&include)?;
        }
        let status = Command::new("cargo")
            .args([
                "cinstall",
                "--release",
                "--library-type",
                "cdylib",
                "-p",
                krate,
            ])
            .arg("--target-dir")
            .arg(out.join("cargo"))
            .arg("--prefix")
            .arg(&prefix)
            .arg("--libdir")
            .arg(prefix.join("lib"))
            .arg("--includedir")
            .arg(&include)
            .current_dir(root)
            .status()
            .context("running cargo cinstall (deps.toml row cargo-c)")?;
        ensure!(status.success(), "building crate {krate} failed");
        // cargo's library name: the package name with `-` as `_`
        let lib = prefix
            .join("lib")
            .join(format!("lib{}.so", krate.replace('-', "_")));
        ensure!(
            lib.is_file(),
            "{} missing after cargo cinstall",
            lib.display()
        );
        println!(
            "native: provided {module} = crate {krate} -> {}",
            lib.display()
        );
        built.insert(
            module.to_owned(),
            Provided {
                lib,
                include: vec![include],
                defines: Vec::new(),
            },
        );
    }
    Ok(built)
}

/// The provided gate (provided_gate.rs) over the device artifacts ninja produced; false on a violation.
/// Every provided shared object, whatever its row kind: the converter links each one first, and an in-tree strong definition of a provided symbol is a second implementation wherever it comes from.
/// Archives are not checked: their members are copied into the dependent, so there is no DT_NEEDED order or cross-object lookup to get wrong.
fn gate(out: &Path, provided: &BTreeMap<String, Provided>) -> Result<bool> {
    let exported = provided
        .values()
        .filter(|p| p.lib.extension().is_some_and(|e| e == "so"))
        .map(|p| provided_gate::exported(&p.lib))
        .collect::<Result<Vec<_>>>()?;
    if exported.is_empty() {
        return Ok(true);
    }
    let report = provided_gate::check(&out.join("device"), &exported)?;
    println!(
        "native: provided gate: {} libraries, {} artifacts, {} weak fallbacks lose, {} violations",
        exported.len(),
        report.artifacts,
        report.fallbacks,
        report.violations.len()
    );
    for v in &report.violations {
        println!("native:   gate: {v}");
    }
    Ok(report.violations.is_empty())
}

/// One converter round's output files.
struct Found<'a> {
    /// `missing.txt`: module references no module of the tree answers to
    modules: &'a str,
    /// `missing-paths.txt`: tree directories modules name by path that do not exist
    paths: &'a str,
    problems: &'a str,
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty())
}

/// Unpinned projects behind the converter's missing references and paths, each with the first `problems.txt`
/// line naming it; plus what no project to fetch can be named for.
fn wanted(
    found: &Found,
    index: &Index,
    manifest: &Manifest,
    pinned: &BTreeSet<&str>,
) -> (BTreeMap<String, String>, Vec<String>) {
    // a problem line names a reference as one token: `:name` (filegroup), `name`, `//dir:name#tag`, a path
    let line = |name: &str| {
        found
            .problems
            .lines()
            .find(|l| {
                l.split_whitespace()
                    .any(|t| t.trim_start_matches(':').split('#').next() == Some(name))
            })
            .unwrap_or("(no problem line)")
            .to_owned()
    };
    let (mut want, mut unresolved) = index.unpinned_projects(
        lines(found.modules).map(|m| (m, format!("module {m}: {}", line(m)))),
        pinned,
        // the converter builds from source only: a prebuilt is no answer to fetch
        Prebuilts::Never,
    );
    for path in lines(found.paths) {
        match manifest.project_of(path) {
            Some(p) if !pinned.contains(p) => {
                want.entry(p.to_owned())
                    .or_insert_with(|| format!("path {path}: {}", line(path)));
            }
            Some(p) => unresolved.push(format!("{path}: in pinned {p}, yet missing")),
            None => unresolved.push(format!("{path}: in no manifest project")),
        }
    }
    (want, unresolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index of a fake tree: `libz` in external/zlib, `libpng` in a project the tests pin, `fg` in
    /// frameworks/base, `dup` in two projects.
    fn index() -> Index {
        let def = |t: &str, p: &str| serde_json::json!({"module_type": t, "project": p, "file": format!("{p}/Android.bp")});
        serde_json::from_value(serde_json::json!({
            "modules": {
                "libz": [def("cc_library", "external/zlib")],
                "libpng": [def("cc_library", "external/png")],
                "fg": [def("filegroup", "frameworks/base")],
                "dup": [def("cc_library", "a"), def("cc_library", "b")],
            },
            "module_types": {},
        }))
        .unwrap()
    }

    /// Manifest of the fake tree: external/gemmlowp unpinned, external/png pinned.
    fn manifest() -> Manifest {
        crate::aosp::parse(r#"<manifest><default revision="r"/><project path="external/gemmlowp" name="g"/><project path="external/png" name="p"/></manifest>"#).unwrap()
    }

    fn run(
        missing: &str,
        paths: &str,
        problems: &str,
        pinned: &BTreeSet<&str>,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let found = Found {
            modules: missing,
            paths,
            problems,
        };
        wanted(&found, &index(), &manifest(), pinned)
    }

    #[test]
    fn missing_paths_to_projects() {
        let problems = "libtflite: include dir external/gemmlowp/fixedpoint does not exist\n";
        let paths = "external/gemmlowp/fixedpoint\nexternal/png/include\nnowhere/x\n";
        let (want, unresolved) = run("", paths, problems, &BTreeSet::from(["external/png"]));
        // the longest manifest project containing the path, evidence = the problem line naming the path
        assert_eq!(
            want,
            BTreeMap::from([(
                "external/gemmlowp".to_owned(),
                format!("path external/gemmlowp/fixedpoint: {}", problems.trim())
            )])
        );
        assert_eq!(
            unresolved,
            [
                "external/png/include: in pinned external/png, yet missing",
                "nowhere/x: in no manifest project"
            ]
        );
    }

    #[test]
    fn missing_references_to_projects() {
        let problems = "\
libandroid_runtime: device module //external/zlib:libz not found
libandroid_runtime: defaults fg not found
libandroid_runtime: source :fg is not a known filegroup
libpng-user: device module libpng not found
";
        let missing = "\
//external/zlib:libz
dup
fg
libpng
nope

";
        let pinned = BTreeSet::from(["external/png"]);
        let (want, unresolved) = run(missing, "", problems, &pinned);
        // `//dir:name` reaches the package directory; one project per reference, evidence = first line
        assert_eq!(
            want.keys().map(String::as_str).collect::<Vec<_>>(),
            ["external/zlib", "frameworks/base"]
        );
        assert_eq!(
            want["frameworks/base"],
            "module fg: libandroid_runtime: defaults fg not found"
        );
        assert_eq!(
            want["external/zlib"],
            "module //external/zlib:libz: libandroid_runtime: device module //external/zlib:libz not found"
        );
        // no problem line names it: still wanted, evidence says so
        let (want, _) = run("libz", "", "", &BTreeSet::new());
        assert_eq!(want["external/zlib"], "module libz: (no problem line)");
        // a pinned definition (lookup rule, not absent checkout), an ambiguous and an unknown name
        assert_eq!(
            unresolved,
            [
                "dup: ambiguous: a, b",
                "libpng: defined in pinned external/png, yet missing",
                "nope: not in any Android.bp"
            ]
        );
        // nothing missing -> nothing to do
        assert_eq!(run("\n", "\n", problems, &pinned), Default::default());
    }

    #[test]
    fn evidence_matches_whole_tokens() {
        let p =
            "x: device module libzstd not found\ny: source :libz#tag is not a known filegroup\n";
        let (want, _) = run("libz", "", p, &BTreeSet::new());
        assert_eq!(
            want["external/zlib"],
            "module libz: y: source :libz#tag is not a known filegroup"
        );
    }
}
