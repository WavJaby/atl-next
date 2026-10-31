// SPDX-License-Identifier: MIT
//! `xtask native`: AOSP native modules built for this Linux host by our Blueprint converter
//! (`tools/bp2ninja`, plan D7-2): builds the converter with the distro Go against the pinned
//! `build/blueprint` (converter.rs), builds the
//! providers.toml modules provided outside the tree (`crate:` with cargo-c, `upstream:` by their
//! recipe), converts the goals' closure into ninja, builds it with `ninja -k 0`, summarises the
//! failures by module and runs the provided gate (provided_gate.rs) and the table check: providers.toml
//! rows the converter finds wrong, and `none` rows a still-built module needs (none_gate.rs). Either
//! fails the run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Index, Manifest, Prebuilts};
use crate::providers::{Partial, Provided};
use crate::sources::Pin;
use crate::{
    arches, converter, farm, fetch, licence_gate, licenses, none_gate, patch, provided_gate,
    providers, sources, triage, upstream, workspace,
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
    let go_work = converter::go_work(root, &layout, &tool_dir)?;
    let converter = tool_dir.join("bp2ninja");
    converter::go(
        root,
        &go_work,
        &["build", "-o", &converter.display().to_string(), "."],
    )
    .context("building bp2ninja")?;
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
    // a srcs row's provider stands in for some sources of its module, not for the module
    let partial: BTreeMap<&str, Partial> = providers::partial(&providers)
        .into_iter()
        .map(|(module, row)| {
            let provider = provided.remove(module);
            let srcs = row.srcs.clone();
            (module, Partial { srcs, provider })
        })
        .collect();
    let provided_file = out.join("provided.json");
    std::fs::write(&provided_file, serde_json::to_string_pretty(&provided)?)?;
    let srcs_file = out.join("srcs.json");
    std::fs::write(&srcs_file, serde_json::to_string_pretty(&partial)?)?;
    let rust = rust_toolchain(root)?;
    // the table check: providers.toml rows the converter finds wrong against the tree, then the none gate
    let mut table: Vec<String> = Vec::new();
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
                "-rust-version",
                &rust.version,
            ])
            .arg("-rustc")
            .arg(&rust.rustc)
            .arg("-provided")
            .arg(&provided_file)
            .arg("-srcs")
            .arg(&srcs_file)
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
        table.extend(
            problems
                .lines()
                .filter(|l| l.starts_with(providers::FILE))
                .map(str::to_owned),
        );
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
    let libs = provided
        .values()
        .chain(partial.values().filter_map(|p| p.provider.as_ref()))
        .map(|p| p.lib.as_path());
    let gate_ok = gate(&out, libs)? & licence_gate(root, &layout, &sources, &out, args.verbose)?;
    if status.success() {
        println!("native: built -> {}", out.display());
        return Ok(if gate_ok && table_check(&table) {
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
    table.extend(
        none_gate::check(&graph, &failures, &tree)
            .iter()
            .map(ToString::to_string),
    );
    table_check(&table);
    Ok(ExitCode::FAILURE)
}

/// Prints the table check's problems; true when there are none.
fn table_check(problems: &[String]) -> bool {
    println!("native: table check: {} problems", problems.len());
    for p in problems {
        println!("native:   table: {p}");
    }
    problems.is_empty()
}

/// The rustc AOSP Rust modules build with: rust-toolchain.toml's, the toolchain this repo builds with (no second Rust toolchain; Soong's `prebuilts/rust` goes with Soong).
struct RustToolchain {
    rustc: std::path::PathBuf,
    /// `x.y.z`: Soong's ANDROID_RUST_VERSION
    version: String,
}

/// Resolves rust-toolchain.toml's rustc through rustup in the repo root (ninja runs elsewhere, where rustup would not see the pin).
fn rust_toolchain(root: &Path) -> Result<RustToolchain> {
    let out = Command::new("rustup")
        .args(["which", "rustc"])
        .current_dir(root)
        .output()
        .context("running rustup which rustc")?;
    ensure!(out.status.success(), "rustup which rustc failed");
    let rustc = std::path::PathBuf::from(String::from_utf8(out.stdout)?.trim());
    let out = Command::new(&rustc)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", rustc.display()))?;
    ensure!(out.status.success(), "{} --version failed", rustc.display());
    // `rustc 1.99.1 (<hash> <date>)`
    let text = String::from_utf8(out.stdout)?;
    let version = text
        .split_whitespace()
        .nth(1)
        .with_context(|| format!("unexpected rustc --version output {text:?}"))?
        .to_owned();
    Ok(RustToolchain { rustc, version })
}

/// Builds every `crate:` row's crate into `<out>/crates/<crate>` (the shared library) and
/// `<out>/gen/<crate>/include` (its generated headers), with cargo-c; one cargo target dir per build
/// directory, so concurrent checkouts do not share artifacts. A crate providing several modules (atl-bionic:
/// libc, libasync_safe) is built once; each of its modules gets the same entry.
fn build_crates(
    root: &Path,
    out: &Path,
    providers: &providers::Providers,
) -> Result<BTreeMap<String, Provided>> {
    let mut by_crate: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (module, krate) in providers::crates(providers) {
        by_crate.entry(krate).or_default().push(module);
    }
    let mut built = BTreeMap::new();
    for (krate, modules) in by_crate {
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
        for module in modules {
            let part = if providers[module].srcs.is_empty() {
                ""
            } else {
                " (its srcs row)"
            };
            println!(
                "native: provided {module}{part} = crate {krate} -> {}",
                lib.display()
            );
            built.insert(
                module.to_owned(),
                Provided {
                    lib: lib.clone(),
                    include: vec![include.clone()],
                    defines: Vec::new(),
                },
            );
        }
    }
    Ok(built)
}

/// The provided gate (provided_gate.rs) over the device artifacts ninja produced; false on a violation.
/// Every provided shared object, whatever its row kind (`srcs` rows' included): the converter links each one first, and an in-tree strong definition of a provided symbol is a second implementation wherever it comes from.
/// Archives are not checked: their members are copied into the dependent, so there is no DT_NEEDED order or cross-object lookup to get wrong.
fn gate<'a>(out: &Path, libs: impl Iterator<Item = &'a Path>) -> Result<bool> {
    let exported = libs
        .filter(|lib| lib.extension().is_some_and(|e| e == "so"))
        .map(provided_gate::exported)
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

/// The licence gate (licence_gate.rs) over the device artifacts ninja produced, failed ones aside; false on a denied or unresolved file.
fn licence_gate(
    root: &Path,
    layout: &Layout,
    sources: &sources::Sources,
    out: &Path,
    verbose: bool,
) -> Result<bool> {
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let roots = licence_gate::roots(layout, &sources.checkouts(None));
    let report = licence_gate::check(&std::fs::canonicalize(out)?, &roots, &licenses)?;
    println!(
        "native: licence gate: {} artifacts, {} files checked, not checked: {} generated, {} system, {} members of libraries built outside ninja; {} licences denied, {} files unresolved",
        report.artifacts,
        report.checked,
        report.generated,
        report.system,
        report.outside,
        report.denied.len(),
        report.unresolved.len()
    );
    let shown = |n: usize| if verbose { n } else { n.min(5) };
    for (expr, files) in &report.denied {
        println!(
            "native:   licence: `{expr}` is not allowed in [artifact.{}]: {} files",
            licenses::RUNTIME,
            files.len()
        );
        for (file, artifact) in files.iter().take(shown(files.len())) {
            println!("native:     {file} (in {artifact})");
        }
    }
    for u in report
        .unresolved
        .iter()
        .take(shown(report.unresolved.len()))
    {
        println!("native:   licence: unresolved {u}");
    }
    Ok(report.ok())
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
