// SPDX-License-Identifier: MIT
//! `xtask native`: AOSP native modules built for this Linux host by our Blueprint converter (`tools/bp2ninja`, plan D7-2): builds the converter with the distro Go against the pinned `build/blueprint` (converter.rs),
//! builds the providers.toml modules provided outside the tree (`crate:` with cargo-c, `upstream:` by their recipe; `system:` resolved with pkg-config, system.rs), converts the goals' closure into ninja, builds it with `ninja -k 0`, then Mesa with its own meson build against those libraries (mesa.rs `build_after_ninja`; skipped when the goals lack them),
//! summarises the failures by module and runs the provided gate (provided_gate.rs) and the table check: providers.toml rows the converter finds wrong, and `none` rows a still-built module needs (none_gate.rs). Either fails the run.
//! After a clean build it builds the workspace packages that link converter modules the graph holds (packages.rs: the launcher against libsigchain, atl-aidl against libbinder_rs).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Index, Manifest, Prebuilts};
use crate::build_state;
use crate::providers::{Partial, Provided};
use crate::sources::Pin;
use crate::verdict::{Failure, Verdict};
use crate::{
    arches, build_pool, converter, deps, fetch, licence_gate, licenses, ndk_surface, none_gate,
    packages, patch, provided_gate, providers, prune, runtimes, slot, sources, system, tree,
    triage, upstream, workspace,
};

#[derive(clap::Args)]
pub struct Args {
    /// Android.bp module names to build (with their dependency closure); `nb:<module>` = its native bridge variant (needs --native-bridge).
    #[arg(required_unless_present = "shell")]
    goals: Vec<String>,
    /// The host arch (an arches.toml row with `[host]`); also Soong's arch name.
    #[arg(long)]
    host: String,
    /// Guest arch of the native bridge side (an arches.toml row with `[guest]`): first builds its LLVM runtimes (runtimes.rs) against bionic's `libc_headers`, then converts
    /// `nb:` goals against bionic into `<out>/nb/<arch>`.
    #[arg(long)]
    native_bridge: Option<String>,
    /// C compiler (deps.toml row `clang`).
    #[arg(long, default_value = "clang-19")]
    cc: String,
    /// C++ compiler and link driver (deps.toml row `clang`).
    #[arg(long, default_value = crate::libcxx_gate::CXX)]
    cxx: String,
    /// Archiver (deps.toml row `llvm`).
    #[arg(long, default_value = "llvm-ar-19")]
    ar: String,
    /// Compiler cache run in front of every C/C++ compile (deps.toml row `sccache`); its cache `<workspace>/cache/sccache` is shared by every out dir and checkout.
    #[arg(long, default_value = "sccache")]
    compiler_cache: String,
    /// Compile without the compiler cache, e.g. to rule it out while debugging a build.
    #[arg(long)]
    no_compiler_cache: bool,
    /// Build nothing: open $SHELL inside the task-copy namespace (task copies bound over src/aosp), e.g. to run `ninja` by hand on the slot; outside it ninja reads the unpatched src/aosp.
    #[arg(long)]
    pub shell: bool,
    /// Before building, convert again and again: pin and fetch the AOSP projects behind the modules and
    /// tree paths the converter reports missing (`missing.txt`, `missing-paths.txt`), until a round finds nothing new to fetch. Ninja
    /// runs once, after that.
    #[arg(long)]
    fetch_missing: bool,
    /// Build directory (default: this checkout's slot, see `xtask slot`; else `<workspace>/out/native/<host>`): concurrent checkouts keep their outputs apart.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// Failure summary: every distinct error message, undefined symbol and failed dependency per module (default: the first few; ninja.log keeps the full output).
    #[arg(long, short)]
    verbose: bool,
    /// WSL only: refuse to start when the Windows drive holding the distro's vhdx has less free space (GiB). 6: a full libandroid_runtime build from cold grows the vhdx by
    /// several GiB, and a host drive that fills mid-build fails writes inside the distro (owner's hard stop).
    #[arg(long, default_value_t = 6)]
    host_free_stop_gib: u64,
    /// WSL only: under this much free space (GiB) on that drive, a note asks to compact the vhdx (owner: compact when C: < 30 GB). Only a note, never a refusal.
    #[arg(long, default_value_t = 30)]
    host_free_compact_gib: u64,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    crate::host_disk::check(crate::host_disk::Limits {
        stop_gib: args.host_free_stop_gib,
        compact_gib: args.host_free_compact_gib,
    })?;
    let arches = arches::load(root)?;
    let (pointer_bits, arch_variant, abi, prebuilt_os) = match arches.get(&args.host) {
        Some(a @ arches::Arch { host: Some(h), .. }) => (
            a.pointer_bits.to_string(),
            a.soong_arch_variant.clone().unwrap_or_default(),
            // Soong's Arch.Abi[0]: the lib/<abi>/ of an apk's JNI libraries
            a.guest
                .as_ref()
                .and_then(|g| g.android_abis.first().cloned())
                .unwrap_or_default(),
            h.soong_prebuilt_os.clone().with_context(|| {
                format!(
                    "{}: no soong_prebuilt_os (the JDK and build tools the converter's host Java tools need)",
                    args.host
                )
            })?,
        ),
        _ => anyhow::bail!("{}: not a host arch in {}", args.host, arches::FILE),
    };
    let layout = Layout::of(root)?;
    let ws = layout.workspace();
    // the task copies are bound over it: main entered the namespace (tree::enter)
    let tree = tree::shared_tree(&layout);
    let overlays = tree::overlays_of(root)?;
    tree::verify(&overlays)?;
    let cache = CompilerCache::of(args, ws, &tree::describe(&overlays));
    // go.work and the converter binary live in the build dir, so concurrent checkouts (each in its own dir) do not overwrite each other's converter
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |workspace| {
        workspace.join("out").join("native").join(&args.host)
    })?;
    // the state the crates will be built from, taken first; the record goes until they are
    let state = build_state::of_build(root)?;
    std::fs::create_dir_all(&out)?;
    build_state::forget(&out)?;
    let tool_dir = out.join("bp2ninja");
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
    let deps = deps::load(root, &deps::Consumers::new(&workspace, &sources))?;
    let providers = providers::load(
        root,
        &providers::Targets::new(&workspace.crates, &sources, &deps),
    )?;
    let omit = providers::omitted(&providers).join(",");
    let rust_android = providers::rust_android(&providers).join(",");
    let defines = providers::defines(&providers).join(",");
    let stand_in = providers::stand_ins(&providers)
        .iter()
        .map(|(module, alt)| format!("{module}={alt}"))
        .collect::<Vec<_>>()
        .join(",");
    patch::preflight(root, providers::upstreams(&providers))?;
    // the tree first matches sources.toml: a pinned project absent or cut off mid-clone would never be
    // fetched by the loop, which only adds unpinned ones
    if args.fetch_missing {
        fetch::add_and_fetch(root, &[])?;
    }
    // modules provided outside the tree, before the converter: it links their artifacts; each provider is built or resolved once (crates by cargo-c, upstreams by their own build system, system libraries as installed), then each module gets one entry joining its row's providers in order
    let crates = build_crates(root, &out, &providers)?;
    let mut systems = BTreeMap::new();
    for (_, row) in providers::systems(&providers) {
        if systems.contains_key(row) {
            continue;
        }
        let pkg = deps[row]
            .pkgconfig
            .as_deref()
            .context("providers.toml checked the pkgconfig")?;
        let p = system::resolve(row, pkg)?;
        println!(
            "native: system {row} (pkg-config {pkg}) -> {}",
            p.lib.display()
        );
        systems.insert(row, p);
    }
    let libcxx = PathBuf::from(
        crate::host_art::output(
            Command::new(&converter).args(["-libcxx-headers", "-cxx", &args.cxx]),
            "bp2ninja -libcxx-headers",
        )?
        .trim(),
    );
    let toolchain = upstream::Toolchain {
        cc: &args.cc,
        cxx: &args.cxx,
        libcxx: &libcxx,
    };
    let checkouts = sources.checkouts(None);
    // by module: a recipe installs one artifact per module it provides
    let mut upstreams = BTreeMap::new();
    for name in providers::upstreams(&providers) {
        let recipe = upstream::recipe(name).context("providers.toml checked the recipe")?;
        let c = checkouts
            .iter()
            .find(|c| matches!(&c.pin, Pin::Git(n) if n == name))
            .with_context(|| format!("providers.toml upstream {name} has no sources.toml row"))?;
        let src = layout.locate(&c.dir).dir;
        println!("native: building upstream {name} from {}", src.display());
        upstreams.extend(upstream::build(&src, &out, recipe, &toolchain)?);
    }
    let mut provided = BTreeMap::new();
    // rust_os rows have no provider and module: rows a tree one: the tree builds them (-stand-in)
    let stood: BTreeSet<&str> = providers::stand_ins(&providers)
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    for (module, row) in providers
        .iter()
        .filter(|(m, row)| !row.is_none() && !stood.contains(m.as_str()))
    {
        let Some(provider) = &row.provider else {
            continue;
        };
        let p = match provider.split_once(':') {
            Some(("crate", name)) => crates.get(name),
            Some(("system", name)) => systems.get(name),
            Some(("upstream", _)) => upstreams.get(module),
            _ => None,
        }
        .cloned()
        .with_context(|| format!("{module}: provider {provider} was not built"))?;
        let part = if row.srcs.is_empty() {
            ""
        } else {
            " (its srcs row)"
        };
        println!(
            "native: provided {module}{part} = {provider} -> {}",
            p.lib.display()
        );
        provided.insert(module.as_str(), p);
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
    // workspace packages' AIDL crates (packages.rs aidl_crates): always converted and built, so those packages build whatever the goals
    let (aidl_crates, aidl_goals) = packages::workspace_aidl_crates(root)?;
    let aidl_crates_file = out.join("aidl-crates.json");
    std::fs::write(
        &aidl_crates_file,
        serde_json::to_string_pretty(&aidl_crates)?,
    )?;
    let rust = rust_toolchain(root)?;
    let libclang = libclang_dir(&args.cc)?;
    let llvm = llvm_prefix(&args.cc)?;
    let nb = native_bridge_flags(root, &arches, args, &converter, &tree, &out, &llvm)?;
    // the pinned tag's release config: build flags, and the values aconfig flag libraries are generated with (the converter builds aconfig from the tree)
    let release = aosp::target_release(&tree)?;
    // the table check: providers.toml rows the converter finds wrong against the tree, then the none gate
    let mut table: Vec<String> = Vec::new();
    let mut round = 0;
    loop {
        round += 1;
        // the converter's tree scan skips the directories prune.rs marks (bp.go discover); the table is re-read every round
        let sources = sources::load(root)?;
        prune::apply(&tree, &prune::load(root, &sources)?)?;
        // host Java tools (xsdc for xsd_config's C++ output, ...) are built by the converter's Java pipeline where a goal reaches one; nothing Java is emitted otherwise
        let (jdk, build_tools) = arches::java_tools(&tree, &prebuilt_os);
        let status = Command::new(&converter)
            .arg("-tree")
            .arg(&tree)
            .arg("-jdk")
            .arg(&jdk)
            .arg("-build-tools")
            .arg(&build_tools)
            .arg("-out")
            .arg(&out)
            .args([
                "-arch",
                &args.host,
                "-pointer-bits",
                &pointer_bits,
                "-arch-variant",
                &arch_variant,
                "-abi",
                &abi,
                "-cc",
                &args.cc,
                "-cxx",
                &args.cxx,
                "-ar",
                &args.ar,
                "-omit",
                &omit,
                "-rust-android",
                &rust_android,
                "-defines",
                &defines,
                "-stand-in",
                &stand_in,
                "-rust-version",
                &rust.version,
            ])
            .arg("-rustc")
            .arg(&rust.rustc)
            .arg("-libclang")
            .arg(&libclang)
            .arg("-llvm")
            .arg(&llvm)
            .arg("-provided")
            .arg(&provided_file)
            .arg("-srcs")
            .arg(&srcs_file)
            .arg("-aidl-crates")
            .arg(&aidl_crates_file)
            .arg("-release")
            .arg(&release)
            .args(cache.iter().flat_map(CompilerCache::converter_args))
            .args(&nb.flags)
            .args(&args.goals)
            .args(&aidl_goals)
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
    if let Some(c) = &cache {
        c.start()?;
    }
    let log = out.join("ninja.log");
    // only ninja queues for a machine-wide token (build_pool.rs); the gates below run after it is released
    let token = build_pool::acquire(
        &build_pool::dir(ws),
        build_pool::TOKENS,
        &build_pool::this_holder(root),
        build_pool::POLL,
        |line| println!("native: {line}"),
    )?;
    println!(
        "native: build token {} of {}",
        token.index,
        build_pool::TOKENS
    );
    let graph_file = out.join("graph.json");
    let graph: triage::Graph = serde_json::from_str(
        &std::fs::read_to_string(&graph_file)
            .with_context(|| format!("reading {}", graph_file.display()))?,
    )
    .with_context(|| format!("parsing {}", graph_file.display()))?;
    if let Some(p) = &nb.pass2 {
        runtimes_pass2(root, args, &out, &tree, &graph, p)?;
    }
    let status = Command::new("ninja")
        .args(["-k", "0", "-C"])
        .arg(&out)
        .envs(cache.iter().flat_map(CompilerCache::env))
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(out.join("ninja.err"))?)
        .status()
        .context("running ninja")?;
    // Mesa's own meson build against what ninja built (mesa.rs), still under the token: it compiles as much as a converter module
    let mesa_step = crate::mesa::build_after_ninja(&out, &tree.join(crate::mesa::MESA_TREE))?;
    drop(token);
    let provided_libs: BTreeSet<&Path> = provided
        .values()
        .chain(partial.values().filter_map(|p| p.provider.as_ref()))
        .map(|p| p.lib.as_path())
        .collect();
    // every check that fails the run, with its first error line: the last line names and quotes them (`verdict`)
    let mut failed: Vec<Failure> = Vec::new();
    let mesa_build = match mesa_step {
        crate::mesa::Step::Failed(first) => {
            failed.push(Failure::step("mesa", first));
            None
        }
        crate::mesa::Step::Built(b) => Some(b),
        crate::mesa::Step::Skipped => None,
    };
    if let Some(first) = gate(&out, provided_libs.into_iter())? {
        failed.push(Failure::step("provided gate", first));
    }
    if let Some(first) = licence_gate(
        root,
        &layout,
        &sources,
        &out,
        mesa_build.as_deref(),
        args.verbose,
    )? {
        failed.push(Failure::step("licence gate", first));
    }
    if status.success() {
        println!("native: built -> {}", out.display());
        for built in packages::build(root, &out)? {
            for bin in built.bins {
                println!("native: package {} -> {}", built.package, bin.display());
            }
        }
        // the runtime gates read the converter packages and crates from here (build_state::gate_out); a failed ninja leaves the old packages, so no record
        build_state::record(&out, &state)?;
        if let Some(first) = table_check(&table) {
            failed.push(Failure::step("table check", first));
        }
        return Ok(verdict(failed));
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
    let report = triage::rank(&graph, &failures);
    for line in triage::render(&report, &failures, args.verbose) {
        println!("native: {line}");
    }
    table.extend(
        none_gate::check(&graph, &failures, &tree)
            .iter()
            .map(ToString::to_string),
    );
    // the ninja failure first: it is the cause the gates' and the table's findings may follow from
    let cause = failures
        .values()
        .max_by_key(|f| f.steps)
        .and_then(|f| f.errors.first())
        .map_or_else(
            || "ninja failed; its log names no error line".to_owned(),
            |c| c.line.clone(),
        );
    failed.insert(0, Failure::step("ninja", cause));
    if let Some(first) = table_check(&table) {
        failed.push(Failure::step("table check", first));
    }
    Ok(verdict(failed))
}

/// The run's last line and exit code: every failed check by name, with the first error line of the first one. A run is never read as passed from a tail that ends in a check reporting 0 problems.
/// Errors (`?`, `bail!`: package build, clippy, converter) get their verdict line from main, then their `Error:` line.
fn verdict(failed: Vec<Failure>) -> ExitCode {
    let verdict = Verdict {
        cmd: "native",
        total: None,
        skipped: Vec::new(),
        failures: failed,
    };
    println!("{}", verdict.line());
    if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Size limit of the compiler cache (sccache evicts least recently used entries past it).
/// One clean build of the libandroid_runtime closure stores ~80 MB (2026-11-01, zstd entries): 2 GB keeps ~25 such generations (main + task branches whose flags, pins or patches differ) and leaves room for the goal closure to grow several-fold before this needs a new measurement.
const CACHE_SIZE: &str = "2G";

/// The converter's goal prefix for a native bridge variant (tools/bp2ninja cc.go nbGoalPrefix).
const NB_GOAL: &str = "nb:";

/// Converter flags of the native bridge side, empty without --native-bridge: the guest row's arch + pointer width, and `-nb-provided` = the guest LLVM runtimes,
/// built (runtimes.rs) against the include flags the converter evaluates for bionic's `libc_headers` on that side (`-nb-exports`), so they see the headers the guest libc is compiled with.
fn native_bridge_flags(
    root: &Path,
    arches: &arches::Arches,
    args: &Args,
    converter: &Path,
    tree: &Path,
    out: &Path,
    llvm: &Path,
) -> Result<NbSide> {
    let Some(arch) = &args.native_bridge else {
        ensure!(
            !args.goals.iter().any(|g| g.starts_with(NB_GOAL)),
            "{NB_GOAL} goals need --native-bridge"
        );
        return Ok(NbSide {
            flags: Vec::new(),
            pass2: None,
        });
    };
    let row = arches
        .get(arch)
        .filter(|a| a.guest.is_some())
        .with_context(|| format!("--native-bridge {arch}: no [guest] row in {}", arches::FILE))?;
    let host_bits = arches
        .get(&args.host)
        .context("run checked the host row")?
        .pointer_bits;
    let side = vec![
        "-native-bridge".to_owned(),
        arch.clone(),
        "-native-bridge-bits".to_owned(),
        row.pointer_bits.to_string(),
    ];
    let printed = crate::host_art::output(
        Command::new(converter)
            .arg("-tree")
            .arg(tree)
            .arg("-out")
            .arg(out)
            .args(["-arch", &args.host, "-pointer-bits", &host_bits.to_string()])
            // the ART plugin config is read on every run
            .arg("-llvm")
            .arg(llvm)
            .args(&side)
            .args(["-nb-exports", "libc_headers"]),
        "bp2ninja -nb-exports libc_headers",
    )?;
    // the JSON is the last line; the converter reports Soong's dropped global flags before it
    let bionic: Vec<String> = serde_json::from_str(printed.lines().last().unwrap_or_default())
        .context("bp2ninja -nb-exports output")?;
    let built = runtimes::build_guest(root, arch, &args.cc, &args.cxx, out, &bionic)?;
    let builtins = built
        .iter()
        .find(|b| b.module == runtimes::BUILTINS)
        .map(|b| b.lib.clone())
        .context("runtimes built no compiler-rt builtins")?;
    let libcxx_include = built
        .iter()
        .find(|b| b.module == runtimes::STATIC_LIBCXX)
        .and_then(|b| b.include.clone())
        .context("runtimes built no libc++ headers")?;
    let mut provided: BTreeMap<&str, Provided> = built
        .into_iter()
        .map(|b| {
            let p = Provided {
                lib: b.lib,
                export_include: b.include.into_iter().collect(),
                ..Provided::default()
            };
            (b.module, p)
        })
        .collect();
    // pass 2's output, built before ninja links any of its dependents (run)
    provided.insert(
        runtimes::SHARED_LIBCXX,
        Provided {
            lib: runtimes::shared_libcxx(out, arch),
            export_include: vec![libcxx_include],
            ..Provided::default()
        },
    );
    let file = out.join("nb-provided.json");
    std::fs::write(&file, serde_json::to_string_pretty(&provided)?)?;
    let mut flags = side;
    flags.extend([
        "-nb-provided".to_owned(),
        file.display().to_string(),
        "-clang-rt-include".to_owned(),
        runtimes::rt_include(root)?.display().to_string(),
        "-bionic-static-rust".to_owned(),
        root.join(GUEST_DLSYM).display().to_string(),
    ]);
    Ok(NbSide {
        flags,
        pass2: Some(Pass2 {
            arch: arch.clone(),
            bionic,
            builtins,
        }),
    })
}

/// crates/atl-guest-dlsym's crate root, which the converter compiles for the guest (`-bionic-static-rust`).
const GUEST_DLSYM: &str = "crates/atl-guest-dlsym/src/lib.rs";

/// The native bridge side's converter flags, and what runtimes pass 2 needs (none without --native-bridge).
struct NbSide {
    flags: Vec<String>,
    pass2: Option<Pass2>,
}

/// Runtimes pass 2 inputs known before the converter runs: the guest arch, bionic's include flags, pass 1's builtins archive.
struct Pass2 {
    arch: String,
    bionic: Vec<String>,
    builtins: PathBuf,
}

/// Runtimes pass 2 (runtimes.rs SHARED): ninja builds the guest libc it links against, the converter's `runtimes::SYSROOT_MODULES` (graph.json artifacts), then CMake builds `libc++.so`. Runs before the main ninja run, which links its dependents.
fn runtimes_pass2(
    root: &Path,
    args: &Args,
    out: &Path,
    tree: &Path,
    graph: &triage::Graph,
    p: &Pass2,
) -> Result<()> {
    let libs = runtimes::SYSROOT_MODULES
        .iter()
        .map(|m| {
            let dir = format!("{NB_DIR}{}/{m}", p.arch);
            graph
                .artifact_out(&dir)
                .map(PathBuf::from)
                .with_context(|| format!("runtimes pass 2: no graph.json artifact {dir}: add a {NB_GOAL} goal that links it"))
        })
        .collect::<Result<Vec<_>>>()?;
    let log = out.join("ninja-pass2.log");
    let status = Command::new("ninja")
        .args(["-C"])
        .arg(out)
        .args(&libs)
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(out.join("ninja-pass2.err"))?)
        .status()
        .context("running ninja for the guest libc")?;
    ensure!(
        status.success(),
        "ninja for runtimes pass 2 (guest libc) failed: {}",
        log.display()
    );
    let built = runtimes::build_shared(
        root,
        &p.arch,
        (&args.cc, &args.cxx),
        out,
        &p.bionic,
        &libs,
        &p.builtins,
    )?;
    // it linked against the real libs, not the NDK stubs llvm_android uses: no binding outside the stub surface (ndk_surface.rs)
    let (n, bad) = ndk_surface::check(&built.lib, tree, &p.arch)?;
    println!(
        "native: ndk-surface gate: {} bindings, {} outside the NDK stubs",
        n,
        bad.len()
    );
    ensure!(
        bad.is_empty(),
        "{}: binds outside the NDK stub surface: {}",
        built.lib.display(),
        bad.join(", ")
    );
    Ok(())
}

/// The converter's module directory prefix of the native bridge side (tools/bp2ninja Side.String: `nb/<arch>`).
const NB_DIR: &str = "nb/";

/// 64-bit FNV-1a: a stable short name for a source view (a socket path is capped at 108 bytes; std's hasher is not stable across releases).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The environment `native --shell` gives the shell: the compiler cache's variables for source view `view` (`tree::describe`), the ones every ninja run in that view gets (`CompilerCache::env`).
/// Without them a hand-run `ninja` reads no `SCCACHE_SERVER_UDS` and reaches another view's server, which compiles that view's sources (measured: the shared `server.sock`).
pub(crate) fn shell_env(
    args: &Args,
    repo: &Path,
    view: &str,
) -> Result<Vec<(&'static str, std::ffi::OsString)>> {
    let workspace = Layout::of(repo)?.workspace().to_path_buf();
    Ok(CompilerCache::of(args, &workspace, view).map_or_else(Vec::new, |c| c.env().to_vec()))
}

/// The compiler cache of the converter's C/C++ compiles: sccache, prefixed to the cc rule by the converter's `-launcher` (tools/bp2ninja ninja.go says why Rust and yasm steps go uncached).
/// Preprocessor mode only (`SCCACHE_DIRECT=false`): it keys a compile on its preprocessed text and non-preprocessor flags, not on paths, so another out dir or a task build hits wherever the text is equal (measured with sccache 0.10).
/// Direct mode, sccache's default, keys on the include paths, which name the out dir (`-I<out>/compat/device`, `-isystem <out>/gen/...`): measured 21% hits in a second out dir, every cross-out-dir lookup a miss. Do not drop the setting.
/// It writes the depfile from its own preprocessor run, so `deps = gcc` sees this out dir's headers on a hit too. Compiles with debug info run uncached (bp2ninja `debugInfo`): a hit would carry the filling out dir's paths in DWARF.
/// The one remaining difference from an uncached build: two sources with equal preprocessed text (empty `*_neon.cc` on x86, trivial generated AIDL/pbzero units) share an entry, so the local `STT_FILE` symbol may name the other source's basename; code, data and relocations are equal.
/// Its server is this cache's own: a socket inside the cache dir, so a user's own sccache server (another dir, another size) is never the one answering.
/// One server per build: the server, not the client, runs the preprocessor and the compiler, so it reads sources in the mount namespace it started in.
/// A copy removed and re-created under its path (patch --done, --edit) leaves an older server on the deleted directory: compiles then fail with `no such file`. So the socket carries the view (`tree::describe`) and a token unique to each cache, and `Drop` stops it with the build.
/// Do not share one socket across builds or views.
/// The on-disk entries stay shared: sccache 0.10 c.rs `hash_key` keys on compiler digest + common/arch args (-D yes, -I/-isystem no) + 7 whitelisted env vars + the preprocessed text, so a differing patched file gives a differing key.
#[derive(Debug, PartialEq)]
pub(crate) struct CompilerCache {
    launcher: String,
    dir: PathBuf,
    socket: PathBuf,
}

impl CompilerCache {
    /// None with `--no-compiler-cache`. `view` = the overlay set the build sees (`tree::describe`, empty for a plain build).
    fn of(args: &Args, workspace: &Path, view: &str) -> Option<Self> {
        (!args.no_compiler_cache).then(|| {
            Self::for_view(
                &args.compiler_cache,
                workspace.join("cache").join("sccache"),
                view,
            )
        })
    }

    /// A new cache in `dir` as seen from source view `view` (task-bind gate: a scratch dir). Its server is shared with no other cache.
    pub(crate) fn for_view(launcher: &str, dir: PathBuf, view: &str) -> Self {
        Self {
            launcher: launcher.into(),
            socket: dir.join(format!(
                "server-{:016x}-{:016x}.sock",
                fnv1a(view.as_bytes()),
                build_token()
            )),
            dir,
        }
    }

    /// Stops this cache's server and removes its socket file (sccache leaves it behind). Runs on `Drop`: a build's server ends with the build.
    fn stop(&self) {
        let _ = Command::new(&self.launcher)
            .arg("--stop-server")
            .envs(self.env())
            .output();
        let _ = std::fs::remove_file(&self.socket);
    }

    fn converter_args(&self) -> [&str; 2] {
        ["-launcher", &self.launcher]
    }

    /// The environment of every sccache client ninja runs; the first one starts the server with it.
    pub(crate) fn env(&self) -> [(&'static str, std::ffi::OsString); 4] {
        [
            ("SCCACHE_DIRECT", "false".into()),
            ("SCCACHE_DIR", self.dir.clone().into()),
            ("SCCACHE_CACHE_SIZE", CACHE_SIZE.into()),
            ("SCCACHE_SERVER_UDS", self.socket.clone().into()),
        ]
    }

    /// Connects to the server on this cache's socket, starting one when none answers: `--show-stats` does both (measured: with no server it starts one, with one it only reports).
    /// `--start-server` is not called: on a live socket it starts a second server that takes the socket and orphans the first (measured: every run left one behind).
    /// sccache has no fallback to a plain compile when its server cannot start or its dir is unwritable (measured: every compile fails), so that fails here, once, naming the way out.
    pub(crate) fn start(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating the compiler cache {}", self.dir.display()))?;
        let out = Command::new(&self.launcher)
            .arg("--show-stats")
            .envs(self.env())
            .output()
            .with_context(|| {
                format!(
                    "running {} --show-stats (deps.toml row sccache; --no-compiler-cache builds without it)",
                    self.launcher
                )
            })?;
        ensure!(
            out.status.success(),
            "{} --show-stats failed (--no-compiler-cache builds without it): {}",
            self.launcher,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        println!(
            "native: compiler cache {} in {} (limit {CACHE_SIZE})",
            self.launcher,
            self.dir.display()
        );
        Ok(())
    }
}

impl Drop for CompilerCache {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Differs between processes and between calls (pid + clock): the socket name must never name an earlier build's server.
fn build_token() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(0));
    fnv1a(
        &[
            u64::from(std::process::id()).to_le_bytes(),
            now.to_le_bytes(),
        ]
        .concat(),
    )
}

/// Prints the table check's problems; the first one, None when there are none.
fn table_check(problems: &[String]) -> Option<String> {
    println!("native: table check: {} problems", problems.len());
    for p in problems {
        println!("native:   table: {p}");
    }
    problems.first().cloned()
}

/// The rustc AOSP Rust modules build with: rust-toolchain.toml's, the toolchain this repo builds with (no second Rust toolchain, hence no prebuilts/rust).
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

/// The directory of `cc`'s own libclang.so (deps.toml row `libclang`), which rust_bindgen's bindgen loads: the clang that compiles the C/C++ also parses the headers the bindings describe.
/// `-print-file-name` answers from the driver's library search path; a name it cannot find comes back bare.
fn libclang_dir(cc: &str) -> Result<PathBuf> {
    let out = Command::new(cc)
        .arg("-print-file-name=libclang.so")
        .output()
        .with_context(|| format!("running {cc} -print-file-name=libclang.so"))?;
    ensure!(
        out.status.success(),
        "{cc} -print-file-name=libclang.so failed"
    );
    let lib = PathBuf::from(String::from_utf8(out.stdout)?.trim());
    ensure!(
        lib.is_absolute() && lib.exists(),
        "{cc} finds no libclang.so: install deps.toml row `libclang` (`cargo xtask deps --check`)"
    );
    let dir = lib.parent().context("libclang.so has no parent")?;
    dir.canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))
}

/// The LLVM install prefix of `cc` (deps.toml row `llvm`): the directory above the `bin/llvm-addr2line` that clang's own `-print-prog-name` finds, so the tool is the one of the clang that compiles the build. ART_CLANG_PATH names it (tools/bp2ninja artplugin.go).
fn llvm_prefix(cc: &str) -> Result<PathBuf> {
    let out = Command::new(cc)
        .arg("-print-prog-name=llvm-addr2line")
        .output()
        .with_context(|| format!("running {cc} -print-prog-name=llvm-addr2line"))?;
    ensure!(
        out.status.success(),
        "{cc} -print-prog-name=llvm-addr2line failed"
    );
    llvm_prefix_of(Path::new(String::from_utf8(out.stdout)?.trim()))
}

/// `tool`'s install prefix, `<prefix>/bin/<tool>`. The file must exist: clang's bare-name answer (no such tool found) or a missing file fails here, not in ART's stack dump.
fn llvm_prefix_of(tool: &Path) -> Result<PathBuf> {
    ensure!(
        tool.is_absolute() && tool.is_file(),
        "{} is not an installed llvm-addr2line: install deps.toml row `llvm` (`cargo xtask deps --check`)",
        tool.display()
    );
    let bin = tool.parent().context("llvm-addr2line has no parent")?;
    ensure!(
        bin.ends_with("bin"),
        "{} is not under a bin/ directory",
        tool.display()
    );
    let prefix = bin.parent().context("bin/ has no parent")?;
    prefix
        .canonicalize()
        .with_context(|| format!("resolving {}", prefix.display()))
}

/// Where DESTDIR installs `path`: `dest` + the absolute path (cargo-c follows the DESTDIR convention; the `.pc` files keep the real prefix).
fn staged(dest: &Path, path: &Path) -> Result<PathBuf> {
    let rel = path
        .strip_prefix("/")
        .with_context(|| format!("{} is not absolute", path.display()))?;
    Ok(dest.join(rel))
}

/// Makes `dst` a copy of `src` (files, symlinks, subdirectories), writing only what differs: an identical file keeps its mtime, so ninja's comparisons against it stay as they were.
/// `prune` also removes what `dst` holds and `src` lacks (headers a crate stopped generating).
fn sync_tree(src: &Path, dst: &Path, prune: bool) -> Result<()> {
    if !dst.is_dir() {
        remove_path(dst)?;
        std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    }
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        let kind = entry.file_type()?; // does not follow symlinks
        if kind.is_dir() {
            sync_tree(&from, &to, prune)?;
        } else if kind.is_symlink() {
            sync_link(&from, &to)?;
        } else {
            sync_file(&from, &to)?;
        }
        names.insert(entry.file_name());
    }
    if prune {
        for entry in std::fs::read_dir(dst).with_context(|| format!("reading {}", dst.display()))? {
            let entry = entry?;
            if !names.contains(&entry.file_name()) {
                remove_path(&entry.path())?;
            }
        }
    }
    Ok(())
}

/// Copies `from` over `to` unless `to` is already a regular file with the same bytes.
fn sync_file(from: &Path, to: &Path) -> Result<()> {
    let new = std::fs::read(from).with_context(|| format!("reading {}", from.display()))?;
    let same = std::fs::symlink_metadata(to).is_ok_and(|m| m.is_file())
        && std::fs::read(to).is_ok_and(|old| old == new);
    if same {
        return Ok(());
    }
    remove_path(to)?;
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

/// Makes `to` a symlink with the target `from` names; a link already pointing there is kept.
fn sync_link(from: &Path, to: &Path) -> Result<()> {
    let target =
        std::fs::read_link(from).with_context(|| format!("reading link {}", from.display()))?;
    if std::fs::read_link(to).is_ok_and(|t| t == target) {
        return Ok(());
    }
    remove_path(to)?;
    std::os::unix::fs::symlink(&target, to).with_context(|| format!("linking {}", to.display()))?;
    Ok(())
}

/// Removes whatever `path` is (file, symlink or directory tree); nothing when absent.
fn remove_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Builds every `crate:` row's crate into `<out>/crates/<crate>` (the shared library) and
/// `<out>/gen/<crate>/include` (its generated headers), with cargo-c; one cargo target dir per build
/// directory, so concurrent checkouts do not share artifacts. Each crate is built once, however many rows name it (atl-bionic: libc, libasync_safe).
/// A crate installing no header (its callers include upstream headers) gets no include dir.
/// cargo-c installs into a DESTDIR staging tree that is replaced each run; `sync_tree` copies only what changed into the installed paths, so an unchanged header or library keeps its mtime and its dependants stay clean.
fn build_crates<'a>(
    root: &Path,
    out: &Path,
    providers: &'a providers::Providers,
) -> Result<BTreeMap<&'a str, Provided>> {
    let crates: BTreeSet<&str> = providers::crates(providers)
        .into_iter()
        .map(|(_, krate)| krate)
        .collect();
    let mut built = BTreeMap::new();
    for krate in crates {
        let prefix = out.join("crates").join(krate);
        let include = out.join("gen").join(krate).join("include");
        let dest = out.join("cinstall").join(krate);
        remove_path(&dest)?;
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
            .arg("--destdir")
            .arg(&dest)
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
        let staged_lib = staged(&dest, &prefix.join("lib"))?;
        if staged_lib.is_dir() {
            sync_tree(&staged_lib, &prefix.join("lib"), false)?;
        }
        // headers the crate stopped generating must not stay on the include path
        let staged_include = staged(&dest, &include)?;
        if staged_include.is_dir() {
            sync_tree(&staged_include, &include, true)?;
        } else {
            remove_path(&include)?;
        }
        // cargo's library name: the package name with `-` as `_`
        let lib = prefix
            .join("lib")
            .join(format!("lib{}.so", krate.replace('-', "_")));
        ensure!(
            lib.is_file(),
            "{} missing after cargo cinstall",
            lib.display()
        );
        println!("native: crate {krate} -> {}", lib.display());
        let include = if include.is_dir() {
            vec![include]
        } else {
            Vec::new()
        };
        built.insert(
            krate,
            Provided {
                lib,
                include,
                ..Provided::default()
            },
        );
    }
    Ok(built)
}

/// The provided gate (provided_gate.rs) over the device artifacts ninja produced; false on a violation.
/// Every provided shared object, whatever its row kind (`srcs` rows' included): the converter links each one first, and an in-tree strong definition of a provided symbol is a second implementation wherever it comes from.
/// Archives are not checked: their members are copied into the dependent, so there is no DT_NEEDED order or cross-object lookup to get wrong.
/// The provided gate's first violation, None when it passes.
fn gate<'a>(out: &Path, libs: impl Iterator<Item = &'a Path>) -> Result<Option<String>> {
    let exported = libs
        .filter(|lib| lib.extension().is_some_and(|e| e == "so"))
        .map(provided_gate::exported)
        .collect::<Result<Vec<_>>>()?;
    if exported.is_empty() {
        return Ok(None);
    }
    let current = provided_gate::current_outputs(out)?;
    let report = provided_gate::check(&out.join("device"), &exported, &current)?;
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
    Ok(report.violations.first().map(ToString::to_string))
}

/// The licence gate (licence_gate.rs) over the device artifacts ninja produced, failed ones aside; false on a denied or unresolved file or a stale artifact.
/// The licence gate's first problem, None when it passes: a denied file, an unresolved one, then a stale artifact.
fn licence_gate(
    root: &Path,
    layout: &Layout,
    sources: &sources::Sources,
    out: &Path,
    mesa_build: Option<&Path>,
    verbose: bool,
) -> Result<Option<String>> {
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let roots = licence_gate::roots(layout, &sources.checkouts(None));
    let out = std::fs::canonicalize(out)?;
    licenses::check_test_only(&licenses)?;
    let test_only = licenses::test_only_outputs(&licenses, &out)?
        .into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let report = licence_gate::check(&out, &roots, &licenses, &test_only)?;
    let first = licence_report("licence gate", &report, verbose);
    let Some(build) = mesa_build else {
        return Ok(first);
    };
    let mesa = licence_gate::check_build_outputs(
        &out,
        &std::fs::canonicalize(build)?,
        crate::mesa::OUTPUTS,
        &roots,
        &licenses,
    )?;
    Ok(first.or(licence_report("licence gate (Mesa)", &mesa, verbose)))
}

/// Prints a licence gate report under `label`; its first problem, None when it passes.
fn licence_report(label: &str, report: &licence_gate::Report, verbose: bool) -> Option<String> {
    println!(
        "native: {label}: {} artifacts ({} test-only skipped), {} files checked, not checked: {} generated, {} system, {} stale artifacts; {} licences denied, {} files unresolved",
        report.artifacts,
        report.test_only,
        report.checked,
        report.generated,
        report.system,
        report.stale.len(),
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
    for s in report.stale.iter().take(shown(report.stale.len())) {
        println!("native:   licence: stale {s} (its link did not run in this build: not checked)");
    }
    if report.ok() {
        return None;
    }
    let first = report
        .denied
        .iter()
        .flat_map(|(expr, files)| {
            files.iter().map(move |(file, artifact)| {
                format!(
                    "{file} (in {artifact}): `{expr}` is not allowed in [artifact.{}]",
                    licenses::RUNTIME
                )
            })
        })
        .chain(report.unresolved.iter().map(|u| format!("unresolved {u}")))
        .chain(report.stale.iter().map(|s| format!("stale {s}")))
        .next();
    Some(first.unwrap_or_else(|| format!("{label} failed")))
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

    /// Each check `run` can fail by name exits nonzero alone; only none failed exits 0.
    #[test]
    fn verdict_fails_on_every_check() {
        for check in [
            "ninja",
            "mesa",
            "provided gate",
            "licence gate",
            "table check",
        ] {
            assert_eq!(
                verdict(vec![Failure::step(check, "x")]),
                ExitCode::FAILURE,
                "{check}"
            );
        }
        assert_eq!(verdict(vec![]), ExitCode::SUCCESS);
    }

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

    /// The converter gets `-launcher` only while the cache is on; `--no-compiler-cache` drops it and the clients' environment with it (the converter's ninja test shows the cc rule then runs the compiler directly).
    #[test]
    fn no_compiler_cache_flag() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            native: Args,
        }
        let parse = |extra: &[&str]| {
            let argv = [&["native", "--host", "x86_64"], extra, &["libfoo"]].concat();
            <Cli as clap::Parser>::try_parse_from(argv).unwrap().native
        };
        let ws = Path::new("/ws");
        let on = CompilerCache::of(&parse(&[]), ws, "").expect("on by default");
        assert_eq!(on.converter_args(), ["-launcher", "sccache"]);
        assert_eq!(on.dir, Path::new("/ws/cache/sccache"));
        let env: BTreeMap<_, _> = on.env().into_iter().collect();
        assert_eq!(env["SCCACHE_DIR"], "/ws/cache/sccache");
        assert_eq!(env["SCCACHE_DIRECT"], "false");
        // view hash = FNV-1a 64 offset basis for the empty view (the plain build); then this cache's token
        let uds = env["SCCACHE_SERVER_UDS"].to_string_lossy().into_owned();
        assert!(uds.starts_with("/ws/cache/sccache/server-cbf29ce484222325-"));
        assert!(uds.ends_with(".sock"));
        // another source view -> another server, same on-disk cache
        let task = CompilerCache::of(&parse(&[]), ws, "bionic=/ws/src-wt/t/bionic").unwrap();
        assert_ne!(task.socket, on.socket);
        assert_eq!(task.dir, on.dir);
        // the same view again -> still another server: no build inherits an earlier build's
        let again = CompilerCache::of(&parse(&[]), ws, "").unwrap();
        assert_ne!(again.socket, on.socket);
        let other =
            CompilerCache::of(&parse(&["--compiler-cache", "/opt/sccache"]), ws, "").unwrap();
        assert_eq!(other.converter_args(), ["-launcher", "/opt/sccache"]);
        assert_eq!(
            CompilerCache::of(&parse(&["--no-compiler-cache"]), ws, ""),
            None
        );
    }

    /// sccache leaves its socket file behind; a dropped cache's file must go with it.
    #[test]
    fn dropping_a_cache_removes_its_socket_file() {
        let dir = std::env::temp_dir().join(format!("atl-cc-socket-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        // no such launcher: the stop fails quietly and the file must still go
        let cache = CompilerCache::for_view("atl-no-such-compiler-launcher", dir.clone(), "");
        let socket = cache.socket.clone();
        std::fs::write(&socket, b"").expect("stand-in socket file");
        drop(cache);
        let gone = !socket.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the socket file of a dropped cache is still there");
    }

    #[test]
    fn llvm_prefix_is_the_directory_above_bin() {
        let root = std::env::temp_dir().join(format!("xtask-llvm-prefix-{}", std::process::id()));
        let bin = root.join("llvm").join("bin");
        std::fs::create_dir_all(&bin).expect("test dir");
        let tool = bin.join("llvm-addr2line");
        std::fs::write(&tool, b"").expect("test tool");
        assert_eq!(
            llvm_prefix_of(&tool).expect("installed tool"),
            root.join("llvm").canonicalize().expect("test prefix")
        );
        // clang answers with the bare name when it finds no such tool; a missing file is the same failure
        assert!(llvm_prefix_of(Path::new("llvm-addr2line")).is_err());
        assert!(llvm_prefix_of(&bin.join("missing")).is_err());
        std::fs::remove_dir_all(&root).expect("test cleanup");
    }

    #[test]
    fn sync_tree_writes_only_what_changed() {
        let root = std::env::temp_dir().join(format!("xtask-sync-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (src, dst) = (root.join("src"), root.join("dst"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(dst.join("sub")).unwrap();
        std::fs::write(src.join("same.h"), "same").unwrap();
        std::fs::write(dst.join("same.h"), "same").unwrap();
        std::fs::write(src.join("sub/nested.h"), "nested").unwrap();
        std::fs::write(dst.join("sub/nested.h"), "nested").unwrap();
        std::fs::write(src.join("changed.h"), "new").unwrap();
        std::fs::write(dst.join("changed.h"), "old").unwrap();
        // a header the crate stopped generating
        std::fs::write(dst.join("gone.h"), "stale").unwrap();
        std::os::unix::fs::symlink("same.h", src.join("alias.h")).unwrap();
        // an arbitrary past mtime: a rewrite would show as now
        let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        for f in ["same.h", "sub/nested.h"] {
            std::fs::File::options()
                .write(true)
                .open(dst.join(f))
                .unwrap()
                .set_modified(past)
                .unwrap();
        }
        let mtime = |f: &str| std::fs::metadata(dst.join(f)).unwrap().modified().unwrap();
        sync_tree(&src, &dst, true).unwrap();
        assert_eq!(mtime("same.h"), past, "identical content keeps its mtime");
        assert_eq!(
            mtime("sub/nested.h"),
            past,
            "identical nested content keeps its mtime"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("changed.h")).unwrap(),
            "new"
        );
        assert!(
            std::fs::symlink_metadata(dst.join("gone.h")).is_err(),
            "a header the crate stopped generating is pruned"
        );
        assert_eq!(
            std::fs::read_link(dst.join("alias.h")).unwrap(),
            Path::new("same.h")
        );
        // a second sync with nothing changed keeps every mtime, the changed file's included
        let changed = mtime("changed.h");
        sync_tree(&src, &dst, true).unwrap();
        assert_eq!(mtime("changed.h"), changed);
        assert_eq!(mtime("same.h"), past);
        let _ = std::fs::remove_dir_all(&root);
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
