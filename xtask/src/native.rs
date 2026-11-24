// SPDX-License-Identifier: MIT
//! `xtask native`: AOSP native modules built for this Linux host by our Blueprint converter (`tools/bp2ninja`, plan D7-2): builds the converter with the distro Go against the pinned `build/blueprint` (converter.rs),
//! builds the providers.toml modules provided outside the tree (`crate:` with cargo-c, `upstream:` by their recipe; `system:` resolved with pkg-config, system.rs), converts the goals' closure into ninja, builds it with `ninja -k 0`,
//! summarises the failures by module and runs the provided gate (provided_gate.rs) and the table check: providers.toml rows the converter finds wrong, and `none` rows a still-built module needs (none_gate.rs). Either fails the run.
//! After a clean build it builds the workspace packages that link converter modules the graph holds (packages.rs: the launcher against libsigchain, atl-aidl against libbinder_rs).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Index, Manifest, Prebuilts};
use crate::build_state;
use crate::providers::{Partial, Provided};
use crate::sources::Pin;
use crate::timing::Timings;
use crate::verdict::{Failure, Verdict};
use crate::{
    arches, build_pool, converter, deps, fetch, licence_gate, licenses, ndk_surface, none_gate,
    packages, patch, provided_gate, providers, prune, runtimes, sources, system, tree, triage,
    upstream, workspace, ws_config,
};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Android.bp module names to build (with their dependency closure); `nb:<module>` = its native bridge variant (needs --native-bridge).
    #[arg(required_unless_present_any = ["shell", "for_gates"])]
    goals: Vec<String>,
    /// Also build the goals these gates declare (gate_goals.rs GATES; `all-gates` = every gate), comma-separated. Gates needing the native-bridge guest set --native-bridge to --host.
    #[arg(long = "for", value_delimiter = ',')]
    for_gates: Vec<String>,
    /// The host arch (an arches.toml row with `[host]`); also Soong's arch name. An arch other than the build machine's is cross-built with its row's clang, sysroot and isa.
    #[arg(long)]
    pub host: String,
    /// Guest arch of the native bridge side (an arches.toml row with `[guest]`): first builds its LLVM runtimes (runtimes.rs) against bionic's `libc_headers`, then converts `nb:` goals against bionic into `<out>/nb/<arch>`, or for a guest other than the host's into its own graph dir (`graph_dir`).
    #[arg(long)]
    native_bridge: Option<String>,
    /// Key set the product APKs are signed with, `<role>.x509.pem` + `<role>.pk8` per certificate role (keys.rs; default: the workspace's dev set, <workspace>/keys, made by `xtask keys --execute`).
    #[arg(long)]
    keys: Option<PathBuf>,
    /// C compiler of the build machine's code (default: deps.toml row `clang`'s binary); a cross host's own code takes its arches.toml `cc` row.
    #[arg(long)]
    cc: Option<String>,
    /// C++ compiler and link driver of the build machine's code (default: deps.toml row `clang-cxx`'s binary); a cross host's own code takes its arches.toml `cxx` row.
    #[arg(long)]
    cxx: Option<String>,
    /// Archiver (default: deps.toml row `llvm`'s binary).
    #[arg(long)]
    ar: Option<String>,
    /// Compiler cache run in front of every C/C++ compile (deps.toml row `sccache`); its cache `<workspace>/cache/sccache` is shared by every out dir and checkout.
    #[arg(long, default_value = "sccache")]
    compiler_cache: String,
    /// Compile without the compiler cache, e.g. to rule it out while debugging a build.
    #[arg(long)]
    no_compiler_cache: bool,
    /// Build nothing: open $SHELL inside the task-copy namespace (task copies bound over src/aosp), e.g. to run `ninja` by hand on the build dir; outside it ninja reads the unpatched src/aosp.
    #[arg(long)]
    pub shell: bool,
    /// Before building, convert again and again: pin and fetch the AOSP projects behind the modules and
    /// tree paths the converter reports missing (`missing.txt`, `missing-paths.txt`), until a round finds nothing new to fetch. Ninja
    /// runs once, after that.
    #[arg(long)]
    fetch_missing: bool,
    /// Build directory (default: this checkout's build dir, see `xtask out`): concurrent checkouts keep their outputs apart.
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// Failure summary: every distinct error message, undefined symbol and failed dependency per module (default: the first few; ninja.log keeps the full output).
    #[arg(long, short)]
    verbose: bool,
}

impl Args {
    /// The explicit goals plus those of the `--for` gates; their nb goals set --native-bridge to the host on an identity host (arches.toml `identity`, the identity engine's guest, atld_run.rs); elsewhere the guest is the caller's foreign one.
    fn resolved(&self, root: &Path) -> Result<Args> {
        let mut a = self.clone();
        let wanted = crate::gate_goals::union(root, &self.for_gates)?;
        if !wanted.nb.is_empty() {
            let arches = crate::arches::load(root)?;
            let identity = arches
                .get(&a.host)
                .and_then(|r| r.host.as_ref())
                .is_some_and(|h| h.identity);
            match (&a.native_bridge, identity) {
                (None, true) => a.native_bridge = Some(a.host.clone()),
                // no identity guest: this run builds the host goals; the gates' foreign guest is a second run with --native-bridge <arch>
                (None, false) => println!(
                    "native: --host {} has no identity engine (arches.toml): building the host goals of --for {}; its guest goals need --native-bridge <arch>",
                    a.host,
                    self.for_gates.join(",")
                ),
                // a foreign --native-bridge on an identity host: the gates' guest for an app of that ABI (run-apk, build_state.rs `foreign_out`); the identity guest stays the host run's
                (Some(arch), false) if *arch == a.host => bail!(
                    "--for {}: --host {} has no identity engine (arches.toml), so its own ISA is no native-bridge guest",
                    self.for_gates.join(","),
                    a.host
                ),
                _ => {}
            }
        }
        // a foreign guest's graph (graph_dir) takes only the gates' nb goals; their host goals are the host run's (same --for, no --native-bridge), which takes no nb goal
        let foreign = a.native_bridge.as_ref().is_some_and(|nb| *nb != a.host);
        for g in wanted.converter() {
            let nb = g.starts_with(NB_GOAL);
            if (foreign && !nb) || (a.native_bridge.is_none() && nb) {
                continue;
            }
            if !a.goals.contains(&g) {
                a.goals.push(g);
            }
        }
        // convert for all-gates always (flow-opt R5): a foreign graph holds every gate's nb goals, so no run narrows it; the run's own goals only add
        if foreign {
            let all = crate::gate_goals::union(root, &[crate::gate_goals::ALL.to_owned()])?;
            with_nb_goals(&mut a.goals, &all.nb);
        }
        Ok(a)
    }

    /// The build record's names of this run's goals (build_state.rs): `nb:<m>` -> `nb:<arch>:<m>`.
    fn recorded_goals(&self) -> BTreeSet<String> {
        self.goals
            .iter()
            .map(|g| match (g.strip_prefix(NB_GOAL), &self.native_bridge) {
                (Some(m), Some(arch)) => crate::gate_goals::nb_key(arch, m),
                _ => g.clone(),
            })
            .collect()
    }
}

/// A tool flag's value, else the binary of its deps.toml row.
/// The converter goal that compiles the pinned SELinux policy and derives the service access table (tools/bp2ninja sepolicy.go).
pub const SEPOLICY_GOAL: &str = "atl-sepolicy";
/// Its table tool's package (crate atl-sepolicy-av).
const SEPOLICY_AV: &str = "atl-sepolicy-av";
/// The repo's policy dir (`java/sepolicy`) as the tree shows it through the repo bind (AOSP's `vendor/<name>/sepolicy` place), compiled with the platform policy as a SYSTEM_EXT_PRIVATE_SEPOLICY_DIRS entry;
/// tree-relative, as product config names one, so the outputs do not depend on the checkout's path.
const SEPOLICY_DIR: &str = "vendor/atl/sepolicy";
/// The device's resource overlay (`java/overlay`, laid out as the tree), a DEVICE_PACKAGE_OVERLAYS entry as AOSP products name theirs; tree-relative for [`SEPOLICY_DIR`]'s reason.
const RESOURCE_OVERLAY_DIR: &str = "vendor/atl/overlay";

/// Goal [`SEPOLICY_GOAL`]'s converter flags: the deps.toml rows m4, checkpolicy and secilc, the table tool (built here for the build machine into `<out>/cargo`, before the conversion names it) and
/// [`SEPOLICY_DIR`]. Empty without the goal.
fn sepolicy_args(root: &Path, goals: &[String], out: &Path) -> Result<Vec<String>> {
    if !goals.iter().any(|g| g == SEPOLICY_GOAL) {
        return Ok(Vec::new());
    }
    let target = out.join("cargo");
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", SEPOLICY_AV])
            .arg("--target-dir")
            .arg(&target),
        "cargo build (sepolicy table tool)",
    )?;
    let av = target.join("release").join(SEPOLICY_AV);
    ensure!(av.is_file(), "{} missing after cargo build", av.display());
    Ok(vec![
        "-m4".to_owned(),
        deps::binary(root, "m4")?,
        "-checkpolicy".to_owned(),
        deps::binary(root, "checkpolicy")?,
        "-secilc".to_owned(),
        deps::binary(root, "secilc")?,
        "-sepolicy-av".to_owned(),
        av.display().to_string(),
        "-sepolicy-system-ext-private".to_owned(),
        SEPOLICY_DIR.to_owned(),
    ])
}

fn flag_or_row(flag: &Option<String>, root: &Path, row: &str) -> Result<String> {
    match flag {
        Some(name) => Ok(name.clone()),
        None => deps::binary(root, row),
    }
}

/// The converter's `-cc` / `-cxx`, for every converter run: the converter probes `-cxx` for libc++ (libcxxProvider) even in `-nb-exports` mode, and its default is the `clang++` on PATH, which need not be the deps.toml row (Debian: PATH clang++ = 19).
fn compiler_args(args: &Args, root: &Path) -> Result<[String; 4]> {
    Ok([
        "-cc".to_owned(),
        flag_or_row(&args.cc, root, "clang")?,
        "-cxx".to_owned(),
        flag_or_row(&args.cxx, root, "clang-cxx")?,
    ])
}

/// The converter's flags for a cross host (`cross`: arches::cross_target): the target's clang triple, its fetched sysroot, its isa, its rust target and rustc features (arches::rustc_target_features), the libc++ headers in that sysroot (atl_layout::libcxx_dir, the one derivation), its compiler (cc / cxx rows) and per-module flags (module_flags),
/// and the build machine's row and pointer width (`build`: arches::build_machine), which the converter's build side (tools a build rule executes or loads) is compiled for. Empty for the build machine, whose compiler already targets it.
fn cross_args(
    root: &Path,
    cross: Option<&arches::CrossTarget>,
    (build_name, build): (&str, &arches::Arch),
) -> Result<Vec<String>> {
    let Some(c) = cross else {
        return Ok(Vec::new());
    };
    let sysroot = deps::sysroot_root(root, &c.sysroot)?;
    let libcxx = atl_layout::libcxx_dir(&sysroot)?;
    Ok(vec![
        "-host-target".to_owned(),
        c.clang.clone(),
        "-host-sysroot".to_owned(),
        sysroot.display().to_string(),
        "-host-isa".to_owned(),
        c.isa.clone(),
        "-host-libcxx".to_owned(),
        libcxx.display().to_string(),
        "-host-rust-target".to_owned(),
        c.rust.clone(),
        "-host-rust-features".to_owned(),
        arches::rustc_target_features(&c.isa)?,
        "-host-cc".to_owned(),
        deps::binary(root, &c.cc)?,
        "-host-cxx".to_owned(),
        deps::binary(root, &c.cxx)?,
        "-host-module-flags".to_owned(),
        arches::module_flags_arg(&c.module_flags),
        "-build-arch".to_owned(),
        build_name.to_owned(),
        "-build-pointer-bits".to_owned(),
        build.pointer_bits.to_string(),
    ])
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let mut phases = Phases::new();
    let args = &args.resolved(root)?;
    {
        let ws = atl_layout::Layout::of(root)?.workspace().to_owned();
        crate::disk_floor::check(&ws, &crate::ws_config::load(&ws)?)?;
    }
    let arches = arches::load(root)?;
    let (pointer_bits, arch_variant, abi, host) = match arches.get(&args.host) {
        Some(a @ arches::Arch { host: Some(h), .. }) => (
            a.pointer_bits.to_string(),
            a.soong_arch_variant.clone().unwrap_or_default(),
            // Soong's Arch.Abi[0]: the lib/<abi>/ of an apk's JNI libraries
            a.guest
                .as_ref()
                .and_then(|g| g.android_abis.first().cloned())
                .unwrap_or_default(),
            h,
        ),
        _ => anyhow::bail!("{}: not a host arch in {}", args.host, arches::FILE),
    };
    // the converter and its prebuilt build-only tools run on the build machine, so the prebuilt directory is that row's whatever the host; a host of another arch is compiled for its own row's target
    let (build_name, build) = arches::build_machine(root, &arches)?;
    let prebuilt_os = prebuilt_os(build_name, build)?;
    let cross = arches::cross_target(&args.host, host, build_name)?;
    let cross_flags = cross_args(root, cross.as_ref(), (build_name, build))?;
    // a cross host's cargo target (rust std, sysroot, linker): checked before any build starts
    let cross_cargo = cross
        .as_ref()
        .map(|_| crate::cross::cargo_target(root, &args.host))
        .transpose()?;
    let layout = Layout::of(root)?;
    let ws = layout.workspace();
    // the task copies are bound over it: main entered the namespace (tree::enter)
    let tree = tree::shared_tree(&layout);
    tree::verify(&tree::binds_of(root)?)?;
    let overlays = tree::overlays_of(root)?;
    let cache = CompilerCache::of(args, ws, &tree::describe(&overlays))?;
    // the signing key set (keys.rs): checked before converting, so a missing set fails with the command that makes it rather than at the first sign edge
    let keys = args
        .keys
        .clone()
        .unwrap_or_else(|| crate::keys::dev_dir(&layout));
    crate::keys::check_set(&keys)?;
    // go.work and the converter binary live in the build dir, so concurrent checkouts (each in its own dir) do not overwrite each other's converter
    let out = host_out(root, args.out.as_deref(), &args.host)?;
    // the converter graph and what it builds: the host's out dir, or a foreign guest's own dir (graph_dir), so a foreign run writes neither the host build.ninja nor its args file
    let graph_out = match &args.native_bridge {
        Some(arch) => graph_dir(&out, &args.host, arch),
        None => out.clone(),
    };
    let foreign = graph_out != out;
    // the state the crates will be built from, taken first; the record goes until they are
    let state = build_state::of_build(root)?;
    std::fs::create_dir_all(&out)?;
    std::fs::create_dir_all(&graph_out)?;
    let mut built_goals = build_state::forget(&graph_out, &state)?;
    built_goals.extend(args.recorded_goals());
    phases.lap("preflight");
    let tool_dir = out.join("bp2ninja");
    std::fs::create_dir_all(&out)?;
    let go_work = converter::go_work(root, &layout, &tool_dir)?;
    let converter = tool_dir.join("bp2ninja");
    converter::build(root, &go_work, &converter).context("building bp2ninja")?;
    phases.lap("bp2ninja build");
    let sources = sources::load(root)?;
    let workspace = workspace::load(root)?;
    let deps = deps::load(root, &deps::Consumers::new(&workspace, &sources, &arches))?;
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
    phases.timed("patch preflight", || {
        patch::preflight(root, providers::upstreams(&providers))
    })?;
    // the tree first matches sources.toml: a pinned project absent or cut off mid-clone would never be
    // fetched by the loop, which only adds unpinned ones
    if args.fetch_missing {
        fetch::add_and_fetch(root, &[])?;
    }
    // modules provided outside the tree, before the converter: it links their artifacts
    // a cross host builds every provider for its target: crates with cross::cargo_target, system rows from its sysroot, upstreams with the converter's cross flags (bp2ninja -upstream-toolchain)
    let to_build = &providers;
    let checkouts = sources.checkouts(None);
    // a cross host's system rows resolve in its sysroot: the build machine's pkg-config path and library dirs never apply
    let target_root = cross
        .as_ref()
        .map(|c| deps::sysroot_root(root, &c.sysroot))
        .transpose()?;
    let target = match (&cross, &target_root) {
        (Some(c), Some(dir)) => Some(system::Sysroot {
            name: &c.sysroot,
            root: dir,
            triple: &c.clang,
        }),
        _ => None,
    };
    let cc = flag_or_row(&args.cc, root, "clang")?;
    let cxx = flag_or_row(&args.cxx, root, "clang-cxx")?;
    let libclang = libclang_dir(&cc)?;
    let ctx = ProvideCtx {
        libclang: &libclang,
        root,
        providers: to_build,
        deps: &deps,
        layout: &layout,
        checkouts: &checkouts,
        converter: &converter,
        cc: &cc,
        cxx: &cxx,
    };
    let mut provided = provide(
        &ctx,
        &Machine {
            label: "",
            out: out.clone(),
            cargo: cross_cargo.as_ref(),
            target,
            flags: &cross_flags,
            cross_processor: cross.as_ref().map(|_| args.host.as_str()),
            rust_triple: &host.rust,
        },
        &mut phases,
    )?;
    // a cross host's build side (tools a build rule executes or loads: bp2ninja Build) links the same providers built for the build machine, as Soong resolves a BuildOSTarget module's deps in the build OS variant;
    // in their own dir, apart from the converter's device/, host/ and build/<module> nodes and the gates over them
    let build_provided_file = out.join("build-provided.json");
    let build_provided_args = match &cross {
        None => Vec::new(),
        Some(_) => {
            let build_machine = Machine {
                label: "build ",
                out: out.join("build-provided"),
                cargo: None,
                target: None,
                flags: &[],
                cross_processor: None,
                rust_triple: &build
                    .host
                    .as_ref()
                    .context("the build machine's arches.toml row is a host row")?
                    .rust,
            };
            let mut bp = provide(&ctx, &build_machine, &mut phases)?;
            // a srcs row's provider stands in for sources of a product module; no build-side module has one
            for (module, _) in providers::partial(to_build) {
                bp.remove(module);
            }
            write_if_changed(&build_provided_file, &serde_json::to_string_pretty(&bp)?)?;
            vec![
                "-build-provided".to_owned(),
                build_provided_file.display().to_string(),
            ]
        }
    };
    // a srcs row's provider stands in for some sources of its module, not for the module
    let partial: BTreeMap<&str, Partial> = providers::partial(to_build)
        .into_iter()
        .map(|(module, row)| {
            let provider = provided.remove(module);
            let srcs = row.srcs.clone();
            (module, Partial { srcs, provider })
        })
        .collect();
    let provided_file = out.join("provided.json");
    write_if_changed(&provided_file, &serde_json::to_string_pretty(&provided)?)?;
    let srcs_file = out.join("srcs.json");
    write_if_changed(&srcs_file, &serde_json::to_string_pretty(&partial)?)?;
    // workspace packages' AIDL crates (packages.rs aidl_crates): always converted and built, so those packages build whatever the goals
    let (aidl_crates, aidl_goals) = packages::workspace_aidl_crates(root)?;
    // a foreign graph builds no workspace packages (below), so it converts none of their AIDL goals
    let aidl_goals = if foreign { Vec::new() } else { aidl_goals };
    let aidl_crates_file = out.join("aidl-crates.json");
    write_if_changed(
        &aidl_crates_file,
        &serde_json::to_string_pretty(&aidl_crates)?,
    )?;
    let rust = rust_toolchain(root)?;
    let llvm = llvm_prefix(&flag_or_row(&args.cc, root, "clang")?)?;
    let nb = native_bridge_flags(root, &arches, args, &converter, &tree, &graph_out, &llvm)?;
    // the pinned tag's release config: build flags, and the values aconfig flag libraries are generated with (the converter builds aconfig from the tree)
    let release = aosp::target_release(&tree)?;
    let sepolicy = sepolicy_args(root, &args.goals, &graph_out)?;
    // the table check: providers.toml rows the converter finds wrong against the tree, then the none gate
    let mut table: Vec<String> = Vec::new();
    // host Java tools (xsdc for xsd_config's C++ output, ...) are built by the converter's Java pipeline where a goal reaches one; nothing Java is emitted otherwise
    let (jdk, build_tools) =
        arches::java_tools(&tree, &prebuilt_os, &deps::aosp_project(root, "jdk")?);
    // every ninja run's environment: the compiler cache's variables and the pinned JDK
    let env: Vec<(&'static str, OsString)> = cache
        .iter()
        .flat_map(CompilerCache::env)
        .chain(java_env(&jdk))
        .collect();
    // run-time ISA tiers (isa-tiers.toml): the table is read once per run; a cross host's converter input goes in its out dir
    let tiers = crate::isa_tiers::load(root)?;
    let isa_tiers_file = match &cross {
        Some(c) => Some(crate::isa_tiers::write_converter_file(
            &tiers, &c.isa, &graph_out,
        )?),
        None => None,
    };
    phases.lap("bootstrap");
    // compared after the rounds: a rewritten build.ninja means the graph was (re)converted this run
    let manifest_before = manifest_mtime(&graph_out);
    let mut round = 0;
    loop {
        round += 1;
        // the converter's tree scan skips the directories prune.rs marks (bp.go discover); the table is re-read every round
        let sources = sources::load(root)?;
        prune::apply(&tree, &prune::load(root, &sources)?)?;
        let path = |p: &Path| p.display().to_string();
        let mut argv: Vec<String> = vec![
            "-tree".into(),
            path(&tree),
            "-jdk".into(),
            path(&jdk),
            "-build-tools".into(),
            path(&build_tools),
            "-keys".into(),
            path(&keys),
            "-device-resource-overlays".into(),
            RESOURCE_OVERLAY_DIR.into(),
            "-out".into(),
            path(&graph_out),
        ];
        argv.extend(cross_flags.iter().cloned());
        argv.extend(build_provided_args.iter().cloned());
        if let Some(file) = &isa_tiers_file {
            argv.extend(["-isa-tiers".to_owned(), path(file)]);
        }
        argv.extend(compiler_args(args, root)?);
        argv.extend(
            [
                "-arch",
                &args.host,
                "-pointer-bits",
                &pointer_bits,
                "-arch-variant",
                &arch_variant,
                "-abi",
                &abi,
                "-ar",
                &flag_or_row(&args.ar, root, "llvm")?,
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
                "-rustc",
                &path(&rust.rustc),
                "-libclang",
                &path(&libclang),
                "-llvm",
                &path(&llvm),
                "-provided",
                &path(&provided_file),
                "-srcs",
                &path(&srcs_file),
                "-aidl-crates",
                &path(&aidl_crates_file),
                "-release",
                &release,
                "-licence-gate",
                &licence_gate_command(root, &graph_out)?,
            ]
            .map(str::to_owned),
        );
        argv.extend(
            cache
                .iter()
                .flat_map(CompilerCache::converter_args)
                .map(str::to_owned),
        );
        argv.extend(nb.flags.iter().cloned());
        argv.extend(sepolicy.iter().cloned());
        argv.extend(args.goals.iter().cloned());
        argv.extend(aidl_goals.iter().cloned());
        let regen = convert(&converter, &graph_out, &argv, &env)?;
        phases.lap(&format!("convert: {regen}"));
        let problems = std::fs::read_to_string(graph_out.join("problems.txt")).unwrap_or_default();
        if args.fetch_missing {
            let missing =
                std::fs::read_to_string(graph_out.join("missing.txt")).unwrap_or_default();
            let paths =
                std::fs::read_to_string(graph_out.join("missing-paths.txt")).unwrap_or_default();
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
    // gate a (isa-tiers.toml): each tier source compiles at its tier's isa and fails at its row's, before ninja builds the rest
    // tier sources are the host's: its own compiler (arches.toml cc), as their ninja compiles
    if let Some(c) = &cross {
        let probed = crate::isa_tiers::probe(&graph_out, &deps::binary(root, &c.cc)?)?;
        println!("native: isa tiers: {probed} source(s) probed");
    }
    phases.lap("ninja setup");
    let log = graph_out.join("ninja.log");
    // only ninja runs under the grant (build_pool.rs); the gates below run after it is released
    // a (re)converted graph is a cold-class build (native-full), a warm one native-incr
    let class = if manifest_mtime(&graph_out) == manifest_before {
        build_pool::Class::NativeIncr
    } else {
        build_pool::Class::NativeFull
    };
    let grant = phases.timed("pool wait", || build_pool::acquire(ws, root, class))?;
    println!(
        "native: pool {} cpu {} of {}",
        class.name(),
        grant.cpu,
        grant.cpu_of
    );
    let graph_file = graph_out.join("graph.json");
    let graph: triage::Graph = serde_json::from_str(
        &std::fs::read_to_string(&graph_file)
            .with_context(|| format!("reading {}", graph_file.display()))?,
    )
    .with_context(|| format!("parsing {}", graph_file.display()))?;
    phases.lap("graph load");
    if let Some(p) = &nb.pass2 {
        phases.timed("runtimes pass 2", || {
            runtimes_pass2(root, args, &graph_out, &tree, &graph, p, &env)
        })?;
    }
    let ninja_started = Instant::now();
    let ninja_from = ninja_log_len(&graph_out);
    let status = Command::new("ninja")
        .args(["-k", "0", "-j"])
        .arg(grant.cpu.to_string())
        .args(["-C"])
        .arg(&graph_out)
        .envs(env.iter().cloned())
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(graph_out.join("ninja.err"))?)
        .status()
        .context("running ninja")?;
    let edges = ninja_log_since(&graph_out, ninja_from)
        .map_or_else(|| "edges n/a".to_owned(), |p| format!("{} edges", p.len()));
    phases.span(&format!("ninja ({edges})"), ninja_started.elapsed());
    drop(grant);
    // a crate-source provider has no library: the converter builds the tree module, whose output the gates read as the build's own
    let provided_libs: BTreeSet<&Path> = provided
        .values()
        .chain(partial.values().filter_map(|p| p.provider.as_ref()))
        .filter(|p| p.krate.is_none())
        .map(|p| p.lib.as_path())
        .collect();
    // every check that fails the run, with its first error line: the last line names and quotes them (`verdict`)
    let mut failed: Vec<Failure> = Vec::new();
    let provided_libs: Vec<&Path> = provided_libs.into_iter().collect();
    if let Some(first) = gate(&graph_out, provided_libs.iter().copied())? {
        failed.push(Failure::step("provided gate", first));
    }
    if let Some(first) = runpath_gate(&graph_out)? {
        failed.push(Failure::step("runpath gate", first));
    }
    if let Some(first) = selfbind_gate(&graph_out, &graph)? {
        failed.push(Failure::step("selfbind gate", first));
    }
    // a foreign guest graph holds only guest-arch ELFs under nb/<arch> (no host-ISA product to check): the gate would fail on "nothing checked"
    if foreign {
        println!("native: isa gate: foreign guest graph holds no host-ISA products: not checked");
    } else if let Some(first) = isa_floor_gate(root, &graph_out, host, &tiers, &provided_libs)? {
        failed.push(Failure::step("isa gate", first));
    }
    // a foreign guest graph's own outputs: the guest row's class, machine and (arm) softfp EABI5 (isa_gate `guest_check`)
    if let Some(arch) = args.native_bridge.as_deref().filter(|_| foreign) {
        let bits = arches
            .get(arch)
            .with_context(|| format!("--native-bridge {arch}: no row in {}", arches::FILE))?
            .pointer_bits;
        if let Some(first) = guest_abi_gate(&graph_out, arch, bits)? {
            failed.push(Failure::step("guest abi gate", first));
        }
    }
    if let Some(first) = fronts_gate(&graph_out, &tree)? {
        failed.push(Failure::step("fronts gate", first));
    }
    if let Some(first) = licence_gate(&graph_out, status.success(), args.verbose)? {
        failed.push(Failure::step("licence gate", first));
    }
    phases.lap("gates");
    if status.success() {
        println!("native: built -> {}", graph_out.display());
        let packages_built = if foreign {
            println!("native: foreign guest graph: workspace packages are the host run's");
            Vec::new()
        } else {
            // the workspace packages are cargo runs: one cargo grant, its job count reaches cargo_command
            let _grant = phases.timed("pool wait", || {
                build_pool::acquire(ws, root, build_pool::Class::Cargo)
            })?;
            phases.timed("packages (cargo)", || packages::build(root, &out))?
        };
        for built in packages_built {
            for bin in built.bins {
                println!("native: package {} -> {}", built.package, bin.display());
            }
        }
        // the build machine's JDK and d8 (host_art.rs java_tools) compile it for any host; a foreign guest graph has no framework of its own
        if !foreign {
            match phases.timed("api-impl", || crate::host_art::build_api_impl(root, &out))? {
                Some(dex) => println!("native: api-impl -> {}", dex.display()),
                None => {
                    println!("native: api-impl not built: the goals built no framework header jar")
                }
            }
        }
        // the runtime gates read the converter packages and crates from here (build_state::gate_out); a failed ninja leaves the old packages, so no record
        build_state::record(&graph_out, &state, &built_goals)?;
        // a host run takes its foreign graphs to its state too: one source state per out dir, so no foreign gate or app refuses a stale graph (build_state.rs foreign_out)
        if !foreign {
            failed.extend(refresh_foreign(root, args, &out)?);
        }
        if let Some(first) = table_check(&table) {
            failed.push(Failure::step("table check", first));
        }
        phases.finish();
        return Ok(verdict(failed));
    }
    let text = std::fs::read_to_string(&log)?;
    let prefixes = [
        format!("{}/", graph_out.display()),
        format!("{}/", tree.display()),
    ];
    let failures = triage::failures(&text, &prefixes.each_ref().map(String::as_str));
    if failures.is_empty() {
        // ninja failed without a failed step: it could not load the graph (duplicate rule, missing input)
        let err = std::fs::read_to_string(graph_out.join("ninja.err")).unwrap_or_default();
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
    phases.finish();
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

/// The converter's goal prefix for a native bridge variant (tools/bp2ninja cc.go nbGoalPrefix).
pub(crate) const NB_GOAL: &str = "nb:";

/// Converter flags of the native bridge side, empty without --native-bridge: the guest row's arch, pointer width and Soong arch variant, and `-nb-provided` = the guest LLVM runtimes,
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
        "-native-bridge-arch-variant".to_owned(),
        row.soong_arch_variant.clone().unwrap_or_default(),
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
            // no cross_args: exportsOf a bionic side = tree include dirs; HostCross only picks the glibc sides' libc++ (libcxxProvider), which a bionic compile never searches (-nostdinc++)
            .args(compiler_args(args, root)?)
            .args(["-nb-exports", "libc_headers"]),
        "bp2ninja -nb-exports libc_headers",
    )?;
    // the JSON is the last line; the converter reports Soong's dropped global flags before it
    let bionic: Vec<String> = serde_json::from_str(printed.lines().last().unwrap_or_default())
        .context("bp2ninja -nb-exports output")?;
    let built = runtimes::build_guest(
        root,
        arch,
        &flag_or_row(&args.cc, root, "clang")?,
        &flag_or_row(&args.cxx, root, "clang-cxx")?,
        out,
        &bionic,
    )?;
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
    write_if_changed(&file, &serde_json::to_string_pretty(&provided)?)?;
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
    env: &[(&'static str, OsString)],
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
    // runs under the native grant (native's ninja, before the main one): its job count is the grant's
    let jobs = build_pool::held_jobs().context("runtimes pass 2 runs under the native grant")?;
    let status = Command::new("ninja")
        .args(["-j"])
        .arg(jobs.to_string())
        .args(["-C"])
        .arg(out)
        .args(&libs)
        .envs(env.iter().cloned())
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
        (
            &flag_or_row(&args.cc, root, "clang")?,
            &flag_or_row(&args.cxx, root, "clang-cxx")?,
        ),
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
) -> Result<Vec<(&'static str, OsString)>> {
    let layout = Layout::of(repo)?;
    let arches = arches::load(repo)?;
    let (build_name, build) = arches::build_machine(repo, &arches)?;
    let (jdk, _) = arches::java_tools(
        &tree::shared_tree(&layout),
        &prebuilt_os(build_name, build)?,
        &deps::aosp_project(repo, "jdk")?,
    );
    let mut env = CompilerCache::of(args, layout.workspace(), view)?
        .map_or_else(Vec::new, |c| c.env().to_vec());
    env.extend(java_env(&jdk));
    Ok(env)
}

/// The build machine's PrebuiltOS (Soong's HostPrebuiltTag): the prebuilts directory of the JDK and build tools the converter's host Java tools run with.
fn prebuilt_os(build_name: &str, build: &arches::Arch) -> Result<String> {
    build
        .host
        .as_ref()
        .and_then(|h| h.soong_prebuilt_os.clone())
        .with_context(|| {
            format!(
                "{build_name}: no soong_prebuilt_os (the JDK and build tools the converter's host Java tools need)"
            )
        })
}

/// The pinned JDK `jdk` in a ninja run's environment, as Soong's ninja gets it (build/soong/ui/build/config.go NewConfig: JAVA_HOME = prebuilts/jdk/jdk21/<PrebuiltOS>, its bin first on PATH).
/// Edges find `java` there, not on the build machine: java_binary_host wrappers (build/soong/scripts/jar-wrapper.sh) `exec java` from PATH, and rules (d8, kotlinc, sdkparcelables) carry no PATH of their own.
fn java_env(jdk: &Path) -> [(&'static str, OsString); 2] {
    let mut path = OsString::from(jdk.join("bin"));
    if let Some(p) = std::env::var_os("PATH") {
        path.push(":");
        path.push(p);
    }
    [("JAVA_HOME", jdk.into()), ("PATH", path)]
}

/// Where `--native-bridge <arch>` put module `m`'s outputs, under the graph dir `graph` (`graph_dir`).
pub fn nb_module_dir(graph: &Path, arch: &str, m: &str) -> PathBuf {
    graph.join(format!("{NB_DIR}{arch}/{m}"))
}

/// Name of a foreign guest's graph dir under the out dir (`graph_dir`).
const NB_GRAPH_DIR: &str = "nb-graph-";

/// The dir of the native-bridge graph of guest `arch` under out dir `out` of host `host`: `out` for the host's own arch (the identity guest is part of the host graph, `nb/<host>`), else `out/nb-graph-<arch>`.
/// Accepted constraint: one graph per foreign nb arch, not one multi-arch graph, since one converter run takes one `--native-bridge`. Its cost: each foreign graph has its own host closure of its nb goals and its own guest runtimes (compiles hit sccache, CompilerCache); one dir per arch per build dir.
/// What it buys: a foreign build writes only its own dir, so the host graph and `bp2ninja.args` are never rewritten, and keepset and check keep reading the host graph.
/// Retired when bp2ninja converts several nb arches into one graph (per-arch nb variants) and `Goals::nb` is keyed by ISA: then this dir, `build_state::foreign_out` and the `graph_out` split go.
pub fn graph_dir(out: &Path, host: &str, arch: &str) -> PathBuf {
    if arch == host {
        out.to_owned()
    } else {
        out.join(format!("{NB_GRAPH_DIR}{arch}"))
    }
}

/// Refreshes each foreign guest graph `out` holds at the host run's state: one inner run per graph. Its goals are the foreign set ([`Args::resolved`]), so it takes none of its own.
/// Returns a failure per graph that did not build; that run's verdict is printed above.
fn refresh_foreign(root: &Path, args: &Args, out: &Path) -> Result<Vec<Failure>> {
    let arches = arches::load(root)?;
    let mut failed = Vec::new();
    for arch in foreign_graphs(out)? {
        if arch == args.host || arches.get(arch.as_str()).is_none_or(|r| r.guest.is_none()) {
            continue;
        }
        println!("native: foreign graph {arch}: refresh at this run's state");
        let inner = Args {
            native_bridge: Some(arch.clone()),
            goals: Vec::new(),
            for_gates: Vec::new(),
            fetch_missing: false,
            shell: false,
            ..args.clone()
        };
        if run(root, &inner)? != ExitCode::SUCCESS {
            failed.push(Failure::step(
                &format!("native-bridge {arch} graph"),
                "not refreshed: see its verdict above",
            ));
        }
    }
    Ok(failed)
}

/// The guest arches whose graph dirs `out` holds (`nb-graph-<arch>`, [`graph_dir`]), sorted.
fn foreign_graphs(out: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(out).with_context(|| format!("reading {}", out.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(arch) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix(NB_GRAPH_DIR))
        {
            found.push(arch.to_owned());
        }
    }
    found.sort();
    Ok(found)
}

/// `goals` with each `nb:<m>` of `nb` added once, after the goals it already has.
fn with_nb_goals(goals: &mut Vec<String>, nb: &BTreeSet<String>) {
    for m in nb {
        let goal = format!("{NB_GOAL}{m}");
        if !goals.contains(&goal) {
            goals.push(goal);
        }
    }
}

/// Host `host`'s out dir: the checkout's build dir (or `explicit`). A cross host's outputs go under its arch's dir, so two arches in one build dir never write the same path; the build machine's stay at the out dir, where its gates read them.
pub fn host_out(root: &Path, explicit: Option<&Path>, host: &str) -> Result<PathBuf> {
    let arches = arches::load(root)?;
    let (build_name, _) = arches::build_machine(root, &arches)?;
    let cross = host != build_name;
    let out = crate::out::out_dir(root, explicit)?;
    Ok(if cross { out.join(host) } else { out })
}

/// The cross host whose out dir `out` is ([`host_out`]: its last component names a `[host]` row other than the build machine's); None for the build machine's.
pub fn cross_host_of(root: &Path, out: &Path) -> Result<Option<String>> {
    let arches = arches::load(root)?;
    let (build_name, _) = arches::build_machine(root, &arches)?;
    Ok(out
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n != build_name && arches.get(*n).is_some_and(|r| r.host.is_some()))
        .map(str::to_owned))
}

/// The dir under an out dir that `xtask remote` owns (records, the staged bundle: copies of this out's products, gated where they were built).
pub const REMOTE_DIR: &str = "remote";

/// Whether a dir entry under an out dir is a foreign guest's graph dir ([`graph_dir`]).
pub fn is_graph_dir(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| n.starts_with(NB_GRAPH_DIR))
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
pub struct CompilerCache {
    launcher: String,
    dir: PathBuf,
    socket: PathBuf,
    /// `SCCACHE_CACHE_SIZE`: sccache evicts least recently used entries past it.
    size: String,
}

impl CompilerCache {
    /// The workspace's shared cache, sized by `cache.sccache_size` (`ws_config`); None with `--no-compiler-cache`. `view` = the overlay set the build sees (`tree::describe`, empty for a plain build).
    fn of(args: &Args, workspace: &Path, view: &str) -> Result<Option<Self>> {
        if args.no_compiler_cache {
            return Ok(None);
        }
        let size = ws_config::load(workspace)?.sccache_size.value;
        Ok(Some(Self::for_view(
            &args.compiler_cache,
            workspace.join("cache").join("sccache"),
            view,
            size,
        )))
    }

    /// A new cache in `dir` of limit `size` as seen from source view `view` (task-bind gate: a scratch dir). Its server is shared with no other cache.
    pub fn for_view(launcher: &str, dir: PathBuf, view: &str, size: String) -> Self {
        Self {
            size,
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
    /// Prints this run's stats first: the server is the build's own, so its counters are this build's alone.
    fn stop(&self) {
        let stats = Command::new(&self.launcher)
            .args(["--show-stats", "--stats-format=json"])
            .envs(self.env())
            .output();
        match stats {
            Ok(out) if out.status.success() => {
                match stats_line(&String::from_utf8_lossy(&out.stdout)) {
                    Ok(line) => println!("native: compiler cache this run: {line}"),
                    Err(e) => println!("native: compiler cache stats unreadable: {e:#}"),
                }
            }
            Ok(out) => println!(
                "native: compiler cache stats unreadable: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => println!("native: compiler cache stats unreadable: {e}"),
        }
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
    pub fn env(&self) -> [(&'static str, std::ffi::OsString); 4] {
        [
            ("SCCACHE_DIRECT", "false".into()),
            ("SCCACHE_DIR", self.dir.clone().into()),
            ("SCCACHE_CACHE_SIZE", self.size.clone().into()),
            ("SCCACHE_SERVER_UDS", self.socket.clone().into()),
        ]
    }

    /// Connects to the server on this cache's socket, starting one when none answers: `--show-stats` does both (measured: with no server it starts one, with one it only reports).
    /// `--start-server` is not called: on a live socket it starts a second server that takes the socket and orphans the first (measured: every run left one behind).
    /// sccache has no fallback to a plain compile when its server cannot start or its dir is unwritable (measured: every compile fails), so that fails here, once, naming the way out.
    pub fn start(&self) -> Result<()> {
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
            "native: compiler cache {} in {} (limit {})",
            self.launcher,
            self.dir.display(),
            self.size
        );
        Ok(())
    }
}

impl Drop for CompilerCache {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One line from `sccache --show-stats --stats-format=json` (sccache 0.10 shape): compile requests, hits, misses (summed over languages), hit rate of hits + misses.
fn stats_line(json: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).context("sccache stats JSON")?;
    let s = &v["stats"];
    let sum = |key: &str| -> Result<u64> {
        let counts = s[key]["counts"]
            .as_object()
            .with_context(|| format!("stats.{key}.counts missing"))?;
        Ok(counts.values().filter_map(serde_json::Value::as_u64).sum())
    };
    let requests = s["compile_requests"]
        .as_u64()
        .context("stats.compile_requests missing")?;
    let (hits, misses) = (sum("cache_hits")?, sum("cache_misses")?);
    let rate = match hits + misses {
        0 => "-".to_owned(),
        n => format!("{:.1}%", hits as f64 * 100.0 / n as f64),
    };
    Ok(format!(
        "{requests} compile requests, {hits} hits, {misses} misses, hit rate {rate}"
    ))
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

/// rust-toolchain.toml's rustc, through rustup in the repo root (ninja runs elsewhere, where rustup would not see the pin).
pub(crate) fn rustc(root: &Path) -> Result<std::path::PathBuf> {
    let out = Command::new("rustup")
        .args(["which", "rustc"])
        .current_dir(root)
        .output()
        .context("running rustup which rustc")?;
    ensure!(out.status.success(), "rustup which rustc failed");
    Ok(std::path::PathBuf::from(
        String::from_utf8(out.stdout)?.trim(),
    ))
}

/// The pinned rustc (`rustc`) and its version.
fn rust_toolchain(root: &Path) -> Result<RustToolchain> {
    let rustc = rustc(root)?;
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
/// A cross host's crates are built for its target: cargo's `--target` and environment come from `cross` (cross::cargo_target).
/// What every machine's provided set is resolved from: the providers.toml rows, deps.toml, the checkouts, and the converter (its -upstream-toolchain flags).
struct ProvideCtx<'a> {
    root: &'a Path,
    providers: &'a providers::Providers,
    deps: &'a deps::Deps,
    layout: &'a Layout,
    checkouts: &'a [sources::Checkout],
    converter: &'a Path,
    cc: &'a str,
    cxx: &'a str,
    /// the `clang` row's libclang dir (libclang_dir): bindgen of an upstream's Rust bindings
    libclang: &'a Path,
}

/// The machine a provided set is built for: its output dir, cargo target, sysroot and converter cross flags; all empty for the build machine.
struct Machine<'a> {
    /// log prefix: "" for the ATL host, "build " for a cross host's build machine
    label: &'static str,
    out: PathBuf,
    cargo: Option<&'a crate::cross::CargoTarget>,
    target: Option<system::Sysroot<'a>>,
    flags: &'a [String],
    cross_processor: Option<&'a str>,
    /// arches.toml `[<arch>.host] rust` of the machine: an upstream's Rust bindings target
    rust_triple: &'a str,
}

/// The provided set for machine `m`: each provider built or resolved once (crates by cargo-c, upstreams by their own build system, system libraries as installed), then each module one entry joining its row's providers in order; `srcs` rows included (the caller splits them off).
fn provide<'a>(
    ctx: &ProvideCtx<'a>,
    m: &Machine,
    phases: &mut Phases,
) -> Result<BTreeMap<&'a str, Provided>> {
    let crates = build_crates(ctx.root, &m.out, ctx.providers, m.cargo, phases)?;
    let mut systems = BTreeMap::new();
    for (_, row) in providers::systems(ctx.providers) {
        if systems.contains_key(row) {
            continue;
        }
        let pkg = ctx.deps[row]
            .pkgconfig
            .as_deref()
            .context("providers.toml checked the pkgconfig")?;
        let p = system::resolve(row, pkg, m.target)?;
        println!(
            "native: {}system {row} (pkg-config {pkg}) -> {}",
            m.label,
            p.lib.display()
        );
        systems.insert(row, p);
    }
    // the converter's own compilers, libc++ and target flags: an upstream compiles and links as the machine's tree modules do
    let flags: upstream::Flags = serde_json::from_str(&crate::host_art::output(
        Command::new(ctx.converter)
            .args(["-upstream-toolchain", "-cc", ctx.cc, "-cxx", ctx.cxx])
            .args(m.flags),
        "bp2ninja -upstream-toolchain",
    )?)
    .context("parsing bp2ninja -upstream-toolchain")?;
    let toolchain = upstream::Toolchain {
        flags: &flags,
        // CMake's cross mode (CMAKE_CROSSCOMPILING): the arch name is the clang triple's arch part (arches.toml `clang`)
        cross_processor: m.cross_processor,
        rust_triple: m.rust_triple,
        libclang: ctx.libclang,
    };
    // by module: a recipe installs one artifact per module it provides
    let mut upstreams = BTreeMap::new();
    for name in providers::upstreams(ctx.providers) {
        let recipe = upstream::recipe(name).context("providers.toml checked the recipe")?;
        let c = ctx
            .checkouts
            .iter()
            .find(|c| matches!(&c.pin, Pin::Git(n) if n == name))
            .with_context(|| format!("providers.toml upstream {name} has no sources.toml row"))?;
        let src = ctx.layout.locate(&c.dir).dir;
        println!(
            "native: {}building upstream {name} from {}",
            m.label,
            src.display()
        );
        upstreams.extend(phases.timed("upstreams", || {
            upstream::build(&src, &m.out, recipe, &toolchain)
        })?);
    }
    let mut provided = BTreeMap::new();
    // rust_os rows have no provider and module: rows a tree one: the tree builds them (-stand-in)
    let stood: BTreeSet<&str> = providers::stand_ins(ctx.providers)
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    for (module, row) in ctx
        .providers
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
            "native: {}provided {module}{part} = {provider} -> {}",
            m.label,
            p.lib.display()
        );
        provided.insert(module.as_str(), p);
    }
    Ok(provided)
}

fn build_crates<'a>(
    root: &Path,
    out: &Path,
    providers: &'a providers::Providers,
    cross: Option<&crate::cross::CargoTarget>,
    phases: &mut Phases,
) -> Result<BTreeMap<&'a str, Provided>> {
    let rows = providers::crates(providers);
    let crates: BTreeSet<&str> = rows.iter().map(|(_, krate)| *krate).collect();
    // A `srcs` row's crate takes the replaced files' place inside the module: a static archive the converter links whole into it (bp2ninja cc.go), so the module itself defines its symbols and
    // its version script exports them, as the dropped objects did (a shared object would leave them undefined there: lld refuses the version script, dependents cannot link them).
    // A whole module's crate stays a shared object its dependents link. One crate serves one of the two roles.
    let partial: BTreeSet<&str> = providers::partial(providers)
        .into_iter()
        .filter_map(|(_, row)| row.provider.as_deref()?.strip_prefix("crate:"))
        .collect();
    for (module, krate) in &rows {
        ensure!(
            !partial.contains(krate) || !providers[*module].srcs.is_empty(),
            "providers.toml: crate {krate} provides `srcs` rows and the whole module {module}: one crate is linked into modules or linked by dependents, not both"
        );
    }
    let mut built = BTreeMap::new();
    for krate in crates {
        let prefix = out.join("crates").join(krate);
        let include = out.join("gen").join(krate).join("include");
        let dest = out.join("cinstall").join(krate);
        remove_path(&dest)?;
        let (library_type, suffix) = if partial.contains(krate) {
            ("staticlib", "a")
        } else {
            ("cdylib", "so")
        };
        let mut cargo = Command::new("cargo");
        cargo.args([
            "cinstall",
            "--profile",
            crate::build_state::NATIVE_PROFILE,
            "--library-type",
            library_type,
            "-p",
            krate,
        ]);
        if let Some(target) = cross {
            cargo.arg("--target").arg(&target.triple);
            cargo.envs(target.env.iter().map(|(k, v)| (k, v)));
        }
        let status = phases
            .timed("cargo crates", || {
                crate::unit_use::status(
                    cargo
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
                        .current_dir(root),
                )
            })
            .context("running cargo cinstall (deps.toml row cargo-c)")?;
        ensure!(status.success(), "building crate {krate} failed");
        let installs = Instant::now();
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
        phases.span("installs", installs.elapsed());
        // cargo's library name: the package name with `-` as `_`
        let lib = prefix
            .join("lib")
            .join(format!("lib{}.{suffix}", krate.replace('-', "_")));
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
    let current = provided_gate::current_outputs(out, &["device"])?;
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

/// The runpath gate (runpath_gate.rs) over this graph's fresh link outputs: the device ones from the licence edge's list (`device_links`, rustc binaries too) and `<out>/nb/` by rule link (not in that list); a stale one's failed link is ninja's to report. Plus the converter-built Rust packages' libraries (`<out>/crates/*/lib`, packages.rs: cargo links them, not ninja); its first violation, None when it passes.
fn runpath_gate(out: &Path) -> Result<Option<String>> {
    let mut all = crate::runpath_gate::device_links(out)?;
    all.extend(crate::runpath_gate::link_outputs(out, &[out.join(NB_DIR)])?);
    let (mut artifacts, _) = crate::runpath_gate::fresh(out, all);
    artifacts.extend(crate::runpath_gate::package_libs(&out.join("crates"))?);
    let (read, violations) = crate::runpath_gate::check(out, &artifacts)?;
    println!(
        "native: runpath gate: {read} artifacts, {} runpath entries",
        violations.len()
    );
    for v in violations.iter().take(5) {
        println!("native:   runpath: {v}");
    }
    Ok(violations.first().cloned())
}

/// The selfbind gate (selfbind_gate.rs) over this graph's fresh link outputs under `<out>/device`, the glibc-side libraries app processes load from glibc's scope; its first violation, None when it passes.
/// Expected = graph.json's glibc-side shared artifacts (selfbind_gate.rs [`crate::selfbind_gate::expected`]), so the set comes from the goal set, not from the probe: each one not read as a shared object (and not a failed build's stale output, which fails elsewhere) fails the gate.
/// A graph with none (a foreign guest graph: nb/<arch> bionic guests only) says so with its guest count, not a silent 0.
fn selfbind_gate(out: &Path, graph: &triage::Graph) -> Result<Option<String>> {
    let (expected, guests) = crate::selfbind_gate::expected(graph, out);
    if expected.is_empty() {
        println!(
            "native: selfbind gate: no glibc-side shared library in this graph's goals ({guests} bionic guest artifacts, not in the class): nothing to check"
        );
        return Ok(None);
    }
    let all = crate::runpath_gate::device_links(out)?;
    let (artifacts, stale) = crate::runpath_gate::fresh(out, all);
    let (read, violations) = crate::selfbind_gate::check(out, &artifacts)?;
    let unread = crate::selfbind_gate::unread(&expected, &read, &stale);
    println!(
        "native: selfbind gate: {} of {} expected glibc-side shared objects read ({} stale), {} preemptible self-references",
        read.len(),
        expected.len(),
        stale.len(),
        violations.len()
    );
    for v in violations.iter().take(5) {
        println!("native:   selfbind: {v}");
    }
    if let Some(first) = unread.first() {
        return Ok(Some(format!(
            "{} expected glibc-side shared libraries not read as shared objects, first {}",
            unread.len(),
            first.display()
        )));
    }
    Ok(violations.first().cloned())
}

/// The NDK fronts gate: each built `device/<m>/<m>.so` of ndk_surface.rs `FRONT_SCRIPTS` exports exactly its version script's symbols under their nodes, the surface an NDK app links against; its first mismatch, None when it passes. A front this graph did not build is not checked; the count says how many were.
fn fronts_gate(out: &Path, tree: &Path) -> Result<Option<String>> {
    let mut checked = 0;
    let mut first = None;
    for (module, map) in ndk_surface::FRONT_SCRIPTS {
        let lib = out.join("device").join(module).join(format!("{module}.so"));
        if !lib.is_file() {
            continue;
        }
        checked += 1;
        let bad = ndk_surface::front_mismatch(&lib, &tree.join(map))?;
        for b in bad.iter().take(5) {
            println!("native:   fronts: {module}: {b}");
        }
        if first.is_none()
            && let Some(b) = bad.first()
        {
            first = Some(format!(
                "{module}: {b} ({} differences from {map})",
                bad.len()
            ));
        }
    }
    println!(
        "native: fronts gate: {checked} of {} NDK fronts checked against their version scripts",
        ndk_surface::FRONT_SCRIPTS.len()
    );
    Ok(first)
}

/// The isa gate (isa_gate.rs: ISA, glibc floor, machine per dir, DT_NEEDED) over this cross host's outputs; its first violation, None when it passes. The build machine's row has no isa: not checked.
fn isa_floor_gate(
    root: &Path,
    out: &Path,
    host: &arches::Host,
    tiers: &crate::isa_tiers::Tiers,
    provided: &[&Path],
) -> Result<Option<String>> {
    if host.isa.is_none() {
        return Ok(None);
    }
    let sysroot = crate::isa_gate::sysroot_dirs(root, host)?;
    // a provided archive (bssl-sys's librust_wrapper.a) is no ELF of its own: its members are copied into the outputs the gate reads
    let shared: Vec<&Path> = provided
        .iter()
        .copied()
        .filter(|p| p.extension().is_none_or(|e| e != "a"))
        .collect();
    let report = crate::isa_gate::check(
        out,
        host,
        tiers,
        &crate::isa_tiers::current_objects(out)?,
        &shared,
        &sysroot,
    )?;
    for line in crate::isa_gate::summary(&report) {
        println!("native: isa gate: {line}");
    }
    for (_, v) in report.violations.iter().take(5) {
        println!("native:   isa: {v}");
    }
    Ok(crate::isa_gate::first_violation(&report).map(str::to_owned))
}

/// The guest gate (isa_gate.rs `guest_check`) over a foreign native-bridge graph: every ELF of guest `arch` (pointer width `bits`) has the guest row's ABI. Its first violation, None when it passes.
fn guest_abi_gate(out: &Path, arch: &str, bits: u32) -> Result<Option<String>> {
    let report = crate::isa_gate::guest_check(out, arch, bits)?;
    println!(
        "native: guest abi gate: {arch}: {} ELF files, {} violations",
        report.files,
        report.violations.len()
    );
    for v in report.violations.iter().take(5) {
        println!("native:   guest abi: {v}");
    }
    Ok(report.violations.first().cloned())
}

/// The converter's argument file in the out dir (tools/bp2ninja regen.go): build.ninja's regeneration edge reads it, so a changed flag or goal reruns the conversion.
const CONVERTER_ARGS: &str = "bp2ninja.args";

/// The goals `<out>`'s converter argument file names, None without one: what its build.ninja was converted for. The file is one argument per line, flags first, then the goals (bp2ninja main.go `flag.Args()`); every flag [`convert`]'s argv holds takes a value, so flag and value lines pair up.
pub(crate) fn converter_goals(out: &Path) -> Result<Option<BTreeSet<String>>> {
    let file = out.join(CONVERTER_ARGS);
    if !file.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    Ok(Some(goals_of_args(&text)))
}

fn goals_of_args(text: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while lines.get(i).is_some_and(|l| l.starts_with('-')) {
        i += 2;
    }
    lines.into_iter().skip(i).map(str::to_owned).collect()
}

/// Bytes read from build.ninja's end to find the regeneration rule: bp2ninja writes it last (main.go after the graph, regen.go regenNinja); ~1.4 KB measured, the rest is headroom.
const REGEN_TAIL: u64 = 64 * 1024;

/// build.ninja's regeneration rule (`rule bp2ninja`, in its last [`REGEN_TAIL`] bytes) runs `converter -args <args>`. Not found = false: the caller bootstraps, which rewrites it.
fn regen_runs(graph: &Path, converter: &Path, args: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(graph).with_context(|| format!("opening {}", graph.display()))?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(REGEN_TAIL)))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let tail = String::from_utf8_lossy(&tail);
    let want = format!(
        "  command = {} -args {}",
        converter.display(),
        args.display()
    );
    Ok(tail
        .split("rule bp2ninja\n")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .is_some_and(|line| line == want))
}

/// Brings `<out>/build.ninja` up to date with `argv` (the converter's flags and goals): ninja decides whether to convert again, from the regeneration edge's depfile (every file the last conversion read, this argument file among them).
/// Bootstrap (`bp2ninja -bootstrap`: a build.ninja holding only that edge) when there is no build.ninja, the argument file is new (a build.ninja from before it cannot rerun on it),
/// or its regeneration edge runs another converter or argument file path: bp2ninja writes its own invocation path into the edge it emits, so a graph converted at another path (a build from before the canonical build path, out::CANONICAL) would regenerate itself with that path forever.
/// When `build.ninja` was last written; the converter rewrites it only when its output changes (write-if-changed).
fn manifest_mtime(out: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(out.join("build.ninja"))
        .and_then(|m| m.modified())
        .ok()
}

/// What [`convert`] did: `bootstrap` (bp2ninja -bootstrap ran), `reconvert` (the regeneration edge ran), `keep`, or `unknown` (the log was recompacted).
fn convert(
    converter: &Path,
    out: &Path,
    argv: &[String],
    env: &[(&'static str, OsString)],
) -> Result<&'static str> {
    let args_file = out.join(CONVERTER_ARGS);
    let graph = out.join("build.ninja");
    let bootstrap =
        !args_file.is_file() || !graph.is_file() || !regen_runs(&graph, converter, &args_file)?;
    let text: String = argv.iter().map(|a| format!("{a}\n")).collect();
    ensure!(
        argv.iter().all(|a| !a.contains('\n')),
        "a converter argument holds a newline"
    );
    write_if_changed(&args_file, &text)?;
    if bootstrap {
        let status = Command::new(converter)
            .arg("-bootstrap")
            .arg(&args_file)
            .status()
            .context("running bp2ninja -bootstrap")?;
        ensure!(status.success(), "bp2ninja -bootstrap failed");
    }
    let from = ninja_log_len(out);
    let status = Command::new("ninja")
        .arg("-C")
        .arg(out)
        .arg("build.ninja")
        .envs(env.iter().cloned())
        .status()
        .context("running ninja build.ninja")?;
    ensure!(status.success(), "bp2ninja failed (its output above)");
    // ninja logs the regeneration edge, whose output is build.ninja, only when it ran
    Ok(if bootstrap {
        "bootstrap"
    } else {
        match ninja_log_since(out, from) {
            Some(edges) if edges.iter().any(|e| e == "build.ninja") => "reconvert",
            Some(_) => "keep",
            None => "unknown",
        }
    })
}

/// Bytes in `out`'s `.ninja_log` (ninja's build log, one line per finished edge); 0 without one.
fn ninja_log_len(out: &Path) -> u64 {
    std::fs::metadata(out.join(".ninja_log")).map_or(0, |m| m.len())
}

/// Output paths of the edges ninja logged after byte `from` of `out`'s `.ninja_log`; None when the file is shorter than `from` (ninja recompacted it: the delta is unknown).
fn ninja_log_since(out: &Path, from: u64) -> Option<Vec<String>> {
    let text = std::fs::read(out.join(".ninja_log")).ok()?;
    entries_after(&text, usize::try_from(from).ok()?)
}

/// The output column of each `start end mtime output hash` line from byte `from` on (tab-separated; the `#` header is skipped).
fn entries_after(text: &[u8], from: usize) -> Option<Vec<String>> {
    let tail = String::from_utf8_lossy(text.get(from..)?);
    Some(
        tail.lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split('\t').nth(3).map(str::to_owned))
            .collect(),
    )
}

/// `native times:`, printed to stderr before the verdict line (stdout's last line). `lap` closes the phase since the last mark; `span` and `timed` record a measured stretch and keep it out of the lap around it. An error return prints the table from `Drop`.
struct Phases {
    rows: BTreeMap<String, (Duration, u32)>,
    at: Instant,
    shown: bool,
}

impl Phases {
    fn new() -> Self {
        Phases {
            rows: BTreeMap::new(),
            at: Instant::now(),
            shown: false,
        }
    }

    fn record(&mut self, name: &str, took: Duration) {
        let row = self.rows.entry(name.to_owned()).or_default();
        row.0 += took;
        row.1 += 1;
    }

    /// Closes phase `name`: the time since the last mark.
    fn lap(&mut self, name: &str) {
        self.record(name, self.at.elapsed());
        self.at = Instant::now();
    }

    /// A measured stretch `took` as phase `name`; the next lap starts after it.
    fn span(&mut self, name: &str, took: Duration) {
        self.record(name, took);
        self.at += took;
    }

    fn timed<T>(&mut self, name: &str, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        self.span(name, started.elapsed());
        out
    }

    /// Rows as `Timings` (`name (xN)` when a phase ran N > 1 times).
    fn render(&self) -> String {
        let mut times = Timings::default();
        for (name, (took, n)) in &self.rows {
            let label = if *n > 1 {
                format!("{name} (x{n})")
            } else {
                name.clone()
            };
            times.push(&label, *took);
        }
        times.table("native")
    }

    /// Closes the last phase as `other`, then prints once.
    fn finish(&mut self) {
        if self.shown {
            return;
        }
        self.lap("other");
        self.shown = true;
        eprintln!("{}", self.render().trim_end());
    }
}

impl Drop for Phases {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Writes `text` to `path` unless it already holds it: an unchanged input keeps its mtime, so the edges reading it stay clean.
fn write_if_changed(path: &Path, text: &str) -> Result<()> {
    if std::fs::read(path).ok().as_deref() == Some(text.as_bytes()) {
        return Ok(());
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// The licence gate edge's report in the out dir (tools/bp2ninja licence.go licenceReport).
pub(crate) const LICENCE_REPORT: &str = "licence-gate.json";

/// The directory probe check edges' lists and results in the out dir (tools/bp2ninja globs.go globDir).
pub(crate) const GLOB_DIR: &str = "globs";

/// The command of build.ninja's licence gate edge (bp2ninja -licence-gate): this xtask binary's hidden `licence-gate`, run from the checkout (xtask finds its root from the working directory).
fn licence_gate_command(root: &Path, out: &Path) -> Result<String> {
    let exe = crate::out::canonical(&std::env::current_exe().context("the xtask binary")?);
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    Ok(format!(
        "cd {} && {} licence-gate --out {} --artifacts $out.rsp --report $out --depfile $out.d",
        quote(root),
        quote(&exe),
        quote(out)
    ))
}

/// The licence gate's first problem from its edge's report, None when it passes. After a failed build the report counts only when ninja would not rerun the edge (`ninja -n`): otherwise an artifact it reads failed to build, so it did not run.
fn licence_gate(out: &Path, built: bool, verbose: bool) -> Result<Option<String>> {
    let file = out.join(LICENCE_REPORT);
    if !built {
        let dry = Command::new("ninja")
            .arg("-C")
            .arg(out)
            .arg("-n")
            .arg(&file)
            .output()
            .context("running ninja -n for the licence gate")?;
        if !String::from_utf8_lossy(&dry.stdout).contains("no work to do") {
            println!("native: licence gate: not run (an artifact it checks did not build)");
            return Ok(Some(
                "not run: an artifact it checks did not build".to_owned(),
            ));
        }
    }
    let report: licence_gate::Report = serde_json::from_str(
        &std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?,
    )
    .with_context(|| file.display().to_string())?;
    Ok(licence_report("licence gate", &report, verbose))
}

/// Prints a licence gate report under `label`; its first problem, None when it passes.
fn licence_report(label: &str, report: &licence_gate::Report, verbose: bool) -> Option<String> {
    println!(
        "native: {label}: {} artifacts ({} test-only skipped), {} files checked, not checked: {} generated, {} system, {} crate archive members; {} licences denied, {} files unresolved",
        report.artifacts,
        report.test_only,
        report.checked,
        report.generated,
        report.system,
        report.crates,
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

    /// The argv shape [`convert`] writes: flag/value pairs (an empty value, a value starting with `-`), then the goals.
    #[test]
    fn args_file_goals_follow_the_flags() {
        let text = "-tree\n/t\n-arch-variant\n\n-defines\n-DX=1\nlibbase\nnb:libc\n";
        assert_eq!(
            goals_of_args(text),
            BTreeSet::from(["libbase".to_owned(), "nb:libc".to_owned()])
        );
        assert!(goals_of_args("-tree\n/t\n").is_empty());
    }

    /// The host's own guest stays in the host graph dir; a foreign guest gets `nb-graph-<arch>` under it, and its modules sit under that dir's `nb/<arch>`.
    #[test]
    fn foreign_guest_has_its_own_graph_dir() {
        let out = Path::new("/w/out/build");
        assert_eq!(graph_dir(out, "x86_64", "x86_64"), out);
        let foreign = graph_dir(out, "x86_64", "arm64");
        assert_eq!(foreign, out.join("nb-graph-arm64"));
        assert_eq!(
            nb_module_dir(&foreign, "arm64", "libc"),
            foreign.join("nb/arm64/libc")
        );
    }

    /// The command run-apk's refusal names builds what it gates on: `--native-bridge <isa> --for <gate>` records every goal `build_state::foreign_out` checks, for each foreign guest of the identity host.
    #[test]
    fn foreign_for_builds_the_gated_set() {
        let root = crate::test_root();
        let arches = arches::load(&root).unwrap();
        let mut guests = 0;
        for (isa, row) in &arches {
            if row.guest.is_none() || isa == "x86_64" {
                continue;
            }
            guests += 1;
            let args = Args {
                goals: Vec::new(),
                for_gates: vec!["run-apk".to_owned()],
                host: "x86_64".to_owned(),
                native_bridge: Some(isa.clone()),
                keys: None,
                cc: None,
                cxx: None,
                ar: None,
                compiler_cache: String::new(),
                no_compiler_cache: false,
                shell: false,
                fetch_missing: false,
                out: None,
                verbose: false,
            };
            let built = args.resolved(&root).unwrap().recorded_goals();
            let gated = crate::build_state::foreign_goals(&root, "run-apk", isa).unwrap();
            assert!(!gated.is_empty(), "{isa}");
            let lacking: Vec<_> = gated.difference(&built).collect();
            assert!(lacking.is_empty(), "{isa}: {lacking:?}");
        }
        assert!(guests > 0, "arches.toml names no foreign guest");
    }

    /// A foreign graph takes every gate's nb goal once: the run's own `nb:libm` stays first, the missing `nb:libc` is added after it, and no goal repeats.
    #[test]
    fn foreign_graph_takes_the_all_gates_nb_goals_once() {
        let nb = BTreeSet::from(["libc".to_owned(), "libm".to_owned()]);
        let mut goals = vec!["nb:libm".to_owned(), "libbase".to_owned()];
        with_nb_goals(&mut goals, &nb);
        assert_eq!(goals, ["nb:libm", "libbase", "nb:libc"]);
    }

    /// Only `nb-graph-<arch>` directories count as foreign graphs: a file of that name, and the host's own dirs, do not.
    #[test]
    fn foreign_graphs_are_the_nb_graph_dirs() {
        let out = std::env::temp_dir().join(format!("xtask-foreign-graphs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        for dir in ["nb-graph-arm64", "nb-graph-riscv64", "nb", "bp2ninja"] {
            std::fs::create_dir_all(out.join(dir)).expect("creating a test dir");
        }
        std::fs::write(out.join("nb-graph-x86_64"), "").expect("writing a test file");
        assert_eq!(foreign_graphs(&out).expect("listing"), ["arm64", "riscv64"]);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Soong's ninja environment (config.go NewConfig): JAVA_HOME is the JDK, whose bin precedes the inherited PATH, so `java` resolves to the JDK's.
    #[test]
    fn java_env_puts_jdk_first() {
        let jdk = Path::new("/t/prebuilts/jdk/jdk21/linux-x86");
        let [(home_key, home), (path_key, path)] = java_env(jdk);
        assert_eq!((home_key, home.as_os_str()), ("JAVA_HOME", jdk.as_os_str()));
        assert_eq!(path_key, "PATH");
        let path = path.into_string().unwrap();
        let mut dirs = path.split(':');
        assert_eq!(dirs.next(), Some("/t/prebuilts/jdk/jdk21/linux-x86/bin"));
        assert_eq!(
            dirs.collect::<Vec<_>>().join(":"),
            std::env::var("PATH").unwrap_or_default()
        );
    }

    /// Each check `run` can fail by name exits nonzero alone; only none failed exits 0.
    #[test]
    fn verdict_fails_on_every_check() {
        for check in ["ninja", "provided gate", "licence gate", "table check"] {
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

    /// Every converter run (main graph and `-nb-exports`) takes `-cxx` from the clang-cxx row, never the converter's PATH `clang++` default; a flag overrides it.
    #[test]
    fn compiler_args_name_the_rows() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            native: Args,
        }
        let parse = |extra: &[&str]| {
            let argv = [&["native", "--host", "x86_64"], extra, &["libfoo"]].concat();
            <Cli as clap::Parser>::try_parse_from(argv).unwrap().native
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root");
        // expected = deps.toml rows clang / clang-cxx `binary`, read by hand from the table
        assert_eq!(
            compiler_args(&parse(&[]), root).expect("rows"),
            ["-cc", "clang-19", "-cxx", "clang++-19"]
        );
        assert_eq!(
            compiler_args(&parse(&["--cc", "c", "--cxx", "cxx"]), root).expect("flags"),
            ["-cc", "c", "-cxx", "cxx"]
        );
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
        // no /ws/workspace.toml: the cache takes the default size
        let ws = Path::new("/ws");
        let of =
            |argv: &[&str], view: &str| CompilerCache::of(&parse(argv), ws, view).expect("config");
        let on = of(&[], "").expect("on by default");
        assert_eq!(on.converter_args(), ["-launcher", "sccache"]);
        assert_eq!(on.dir, Path::new("/ws/cache/sccache"));
        let env: BTreeMap<_, _> = on.env().into_iter().collect();
        assert_eq!(env["SCCACHE_DIR"], "/ws/cache/sccache");
        assert_eq!(env["SCCACHE_DIRECT"], "false");
        // the spec's default limit (maintainer 2026-11-11)
        assert_eq!(env["SCCACHE_CACHE_SIZE"], "5G");
        // view hash = FNV-1a 64 offset basis for the empty view (the plain build); then this cache's token
        let uds = env["SCCACHE_SERVER_UDS"].to_string_lossy().into_owned();
        assert!(uds.starts_with("/ws/cache/sccache/server-cbf29ce484222325-"));
        assert!(uds.ends_with(".sock"));
        // another source view -> another server, same on-disk cache
        let task = of(&[], "bionic=/ws/src-wt/t/bionic").expect("on");
        assert_ne!(task.socket, on.socket);
        assert_eq!(task.dir, on.dir);
        // the same view again -> still another server: no build inherits an earlier build's
        let again = of(&[], "").expect("on");
        assert_ne!(again.socket, on.socket);
        let other = of(&["--compiler-cache", "/opt/sccache"], "").expect("on");
        assert_eq!(other.converter_args(), ["-launcher", "/opt/sccache"]);
        assert_eq!(of(&["--no-compiler-cache"], ""), None);

        // a workspace file's size reaches the clients' environment
        let sized = std::env::temp_dir().join(format!("atl-cc-size-{}", std::process::id()));
        std::fs::create_dir_all(&sized).expect("scratch dir");
        std::fs::write(
            sized.join(ws_config::FILE),
            "[cache]\nsccache_size = \"7G\"\n",
        )
        .expect("file");
        let cache = CompilerCache::of(&parse(&[]), &sized, "")
            .expect("config")
            .expect("on");
        std::fs::remove_dir_all(&sized).expect("cleanup");
        let env: BTreeMap<_, _> = cache.env().into_iter().collect();
        assert_eq!(env["SCCACHE_CACHE_SIZE"], "7G");
    }

    /// Fixture = sccache 0.10 `--show-stats --stats-format=json` shape (counts per language); hits 30 + 5 = 35, misses 9 + 1 = 10, rate 35/45 = 77.8%.
    #[test]
    fn stats_line_sums_languages() {
        let json = r#"{"stats":{"compile_requests":50,"cache_hits":{"counts":{"C/C++":30,"Assembler":5},"adv_counts":{}},"cache_misses":{"counts":{"C/C++":9,"Assembler":1},"adv_counts":{}}},"version":"0.10.0"}"#;
        assert_eq!(
            super::stats_line(json).expect("parses"),
            "50 compile requests, 35 hits, 10 misses, hit rate 77.8%"
        );
        // a no-compile run: empty counts, no rate
        let idle = r#"{"stats":{"compile_requests":0,"cache_hits":{"counts":{}},"cache_misses":{"counts":{}}}}"#;
        assert_eq!(
            super::stats_line(idle).expect("parses"),
            "0 compile requests, 0 hits, 0 misses, hit rate -"
        );
        assert!(super::stats_line("{}").is_err());
    }

    /// sccache leaves its socket file behind; a dropped cache's file must go with it.
    #[test]
    fn dropping_a_cache_removes_its_socket_file() {
        let dir = std::env::temp_dir().join(format!("atl-cc-socket-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        // no such launcher: the stop fails quietly and the file must still go
        let cache = CompilerCache::for_view(
            "atl-no-such-compiler-launcher",
            dir.clone(),
            "",
            ws_config::DEFAULT_SCCACHE_SIZE.to_owned(),
        );
        let socket = cache.socket.clone();
        std::fs::write(&socket, b"").expect("stand-in socket file");
        drop(cache);
        let gone = !socket.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the socket file of a dropped cache is still there");
    }

    /// The regeneration edge is reused only when it runs this converter on this argument file; another path (a graph converted before the canonical build path) or no edge -> bootstrap.
    #[test]
    fn regen_edge_must_name_this_converter() {
        let dir = std::env::temp_dir().join(format!("xtask-regen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        let graph = dir.join("build.ninja");
        let (conv, args) = (
            Path::new("/w/out/build/bp2ninja/bp2ninja"),
            Path::new("/w/out/build/bp2ninja.args"),
        );
        // padding past REGEN_TAIL: the edge is found in the tail only, where bp2ninja writes it
        let pad = "#\n".repeat(usize::try_from(REGEN_TAIL).unwrap());
        let edge = |c: &str, a: &str| {
            format!(
                "{pad}rule bp2ninja_globs\n  command = x\nrule bp2ninja\n  command = {c} -args {a}\n  generator = 1\n"
            )
        };
        std::fs::write(
            &graph,
            edge(
                "/w/out/build/bp2ninja/bp2ninja",
                "/w/out/build/bp2ninja.args",
            ),
        )
        .unwrap();
        assert!(regen_runs(&graph, conv, args).unwrap());
        std::fs::write(
            &graph,
            edge(
                "/w/out/pool/ws/x2/bp2ninja/bp2ninja",
                "/w/out/pool/ws/x2/bp2ninja.args",
            ),
        )
        .unwrap();
        assert!(!regen_runs(&graph, conv, args).unwrap());
        std::fs::write(&graph, &pad).unwrap();
        assert!(!regen_runs(&graph, conv, args).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn ninja_log_entries_after_the_offset() {
        // ninja log v5: `start end mtime output hash`, tab-separated, after a `# ninja log v5` header
        let header = "# ninja log v5\n";
        let text = format!("{header}0\t12\t1700\tobj/a.o\t1f\n3\t9\t1700\tlib/b.so\t2e\n");
        let want = ["obj/a.o", "lib/b.so"];
        assert_eq!(entries_after(text.as_bytes(), header.len()).unwrap(), want);
        assert_eq!(entries_after(text.as_bytes(), 0).unwrap(), want);
        assert!(
            entries_after(text.as_bytes(), text.len())
                .unwrap()
                .is_empty()
        );
        // shorter than the offset: recompacted, so the delta is unknown
        assert!(entries_after(text.as_bytes(), text.len() + 1).is_none());
    }

    #[test]
    fn phases_add_repeats_and_sum() {
        let mut p = Phases::new();
        p.span("cargo crates", Duration::from_secs(2));
        p.span("cargo crates", Duration::from_secs(3));
        p.span("ninja (0 edges)", Duration::from_secs(5));
        let text = p.render();
        assert!(text.starts_with("native times:\n"), "{text}");
        assert!(text.contains("cargo crates (x2)"), "{text}");
        // 2 + 3 + 5 s
        assert!(text.trim_end().ends_with("10.0s"), "{text}");
        // prints to the captured stderr and marks the table shown, so Drop stays quiet
        p.finish();
    }
}
