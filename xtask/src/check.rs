// SPDX-License-Identifier: MIT
//! `xtask check`: the static + unit test tiers.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::build_pool::{self, Class};
use crate::harness::StepDone;
use crate::timing::{self, Timings};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{self, Failure, Verdict};
use crate::{
    aosp, app_ops_table, arches, build_state, converter, cts_table, deps, gtk_baseline,
    libcxx_gate, licenses, modules, out, packages, patch, providers, prune, service_mirrors,
    sources, spdx, test_apks, workspace,
};
use atl_layout::Layout;

#[derive(clap::Args)]
pub struct Args {
    /// Run only these steps (repeatable).
    #[arg(long, value_enum)]
    only: Vec<Step>,
    /// Native build directory for converter-packages (default: this checkout's build dir, see `xtask out`); without its graph.json that step is skipped.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Step {
    /// rustfmt, check mode
    Fmt,
    /// SPDX header on every tracked or new, non-ignored file
    Spdx,
    /// repo tables (arches, sources, deps, providers, licenses, prune, cts, test-apks, isa-tiers, gtk-baseline) parse with their schema and invariants; the tracked APK scenarios (xtask-harness scenarios.rs `check`) name a row and pass their static rules
    Tables,
    /// deps.toml rows against this host: installed at the required version (`deps --check`)
    Deps,
    /// every [aosp.sparse] row covers the reads of this checkout's build dir graph, converted for every gate's goals (keepset.rs); skipped without a graph
    Keepset,
    /// launcher-closure.toml's rules, then every host library of the launcher's DT_NEEDED closure (its PT_INTERP's `ld.so --list` over the runtime library path) is a row and every row is in it, then the loader, installer and runtime define no name libatl_bionic.so exports (launcher_closure.rs); skipped without a built launcher
    LauncherClosure,
    /// modules.toml equals the real cross-module crate graph
    Modules,
    /// a fresh checkout builds xtask: its build closure is definition crates only, and it builds offline under bwrap with src, src-wt and out hidden while a control crate that reads the tree fails there (bootstrap_gate.rs)
    Bootstrap,
    /// fetched upstream checkouts match their pins and patches/ (no unexported or uncommitted work)
    Drift,
    /// libbinder's driver I/O goes only through BinderDriver.h (patches/aosp/frameworks/native 0002), in the tree this checkout builds
    Seam,
    /// Android path literals (`/system`, `/data`, ...) only in crate atl-android-paths, others allowed by android-paths.toml with a reason
    AndroidPaths,
    /// bwrap flag literals only in atl-broker's bwrap.rs, besides the golden argv tests
    BwrapFlags,
    /// tracked code never invokes Soong (`soong_ui`, `lunch`; Soong retired a84d9b6): comments and Markdown pass
    Soong,
    /// no crate logs a free-text `refused` / `unimplemented` line: API outcomes go through atl-diag; lines of another subject listed with a reason (diag_shape.rs)
    DiagShape,
    /// the WebView provider throws UnsupportedOperationException only for the class-C methods listed with a reason (provider_refusals.rs)
    ProviderRefusals,
    /// product code reads no environment-variable switch: each read is cargo's, or listed with its class and reason (env_reads.rs); no Cargo.toml enables a `break-*` / `tier-*` feature
    EnvReads,
    /// clippy on all targets, warnings denied
    Clippy,
    /// cargo-deny: advisories, bans, licences, sources
    Deny,
    /// cargo test, whole workspace
    Test,
    /// the Blueprint converter (tools/bp2ninja): go build, go vet, go test against the pinned blueprint, offline
    Converter,
    /// the distro libc++ headers under the converter's device C++ flags: -D__ANDROID__ changes no macro beyond the expected and no object byte
    Libcxx,
    /// AOSP llvm_android's builders.py agrees with runtimes.rs on every define it sets as a literal
    Runtimes,
    /// clippy + unit tests on the packages that link converter modules (feature `converter`), against the native out dir's graph; skipped without one
    ConverterPackages,
    /// the guest GL proxies cover the NDK maps and the host fronts export every bound stub (xtask-harness gl_proxies.rs), on the native out dir; skipped without its guest proxies and fronts
    GlProxies,
    /// the widgets fixture on the native out dir under the nested compositor: first screenshot non-uniform + one tap that changes pixels (xtask-harness drive_gate.rs `quick`); skipped without a native out dir
    DriveTap,
    /// the runtime gates listed in xtask-harness check_gates.rs `ROWS` (deterministic on a build machine, each well under a minute), each as `cargo xtask test` runs it, on the native out dir; skipped without a native out dir
    Gates,
    /// a CJK and an emoji code point each resolve to a real font file through the product's font list over this desktop's own fontconfig (xtask-harness host_fonts.rs); a host with no such face is skipped and named (`host lacks a CJK font`)
    HostFonts,
    /// the UI module cdylibs (release builds from this checkout's sources) import no GTK/GDK/GSK name outside the GTK 4.14 baseline (gtk-baseline.toml; gtk_imports.rs)
    GtkImports,
    /// atl-bionic's generation (its build script, `cargo check --target`) for every cross host whose sysroot and rust std are present, so a cross-only refusal shows before a cross build; a host without them is skipped and named
    CrossBionic,
}

const ALL: [Step; 29] = [
    Step::Fmt,
    Step::Spdx,
    Step::Tables,
    Step::Deps,
    Step::Keepset,
    Step::LauncherClosure,
    Step::Modules,
    Step::Bootstrap,
    Step::Drift,
    Step::Seam,
    Step::AndroidPaths,
    Step::BwrapFlags,
    Step::Soong,
    Step::DiagShape,
    Step::ProviderRefusals,
    Step::EnvReads,
    Step::Clippy,
    Step::Deny,
    Step::Test,
    Step::Converter,
    Step::Libcxx,
    Step::Runtimes,
    Step::ConverterPackages,
    Step::GlProxies,
    Step::DriveTap,
    Step::Gates,
    Step::HostFonts,
    Step::GtkImports,
    Step::CrossBionic,
];

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Fmt => "fmt",
            Step::Spdx => "spdx",
            Step::Tables => "tables",
            Step::Deps => "deps",
            Step::Keepset => "keepset",
            Step::LauncherClosure => "launcher-closure",
            Step::Modules => "modules",
            Step::Bootstrap => "bootstrap",
            Step::Drift => "drift",
            Step::Seam => "seam",
            Step::AndroidPaths => "android-paths",
            Step::BwrapFlags => "bwrap-flags",
            Step::Soong => "soong",
            Step::DiagShape => "diag-shape",
            Step::ProviderRefusals => "provider-refusals",
            Step::EnvReads => "env-reads",
            Step::Clippy => "clippy",
            Step::Deny => "deny",
            Step::Test => "test",
            Step::Converter => "converter",
            Step::Libcxx => "libcxx",
            Step::Runtimes => "runtimes",
            Step::ConverterPackages => "converter-packages",
            Step::GlProxies => "gl-proxies",
            Step::DriveTap => "drive-tap",
            Step::Gates => "gates",
            Step::HostFonts => "host-fonts",
            Step::GtkImports => "gtk-imports",
            Step::CrossBionic => "cross-bionic",
        }
    }

    /// The pool class the step's leaf work takes (build_pool.rs): cargo/go compiles `cargo`, runtime gates `harness`, the rest `light`. None: `gates`, whose rows each take their own grant (check_gates.rs), so the step holds none (a holder above them would refuse their nested admission).
    fn class(self) -> Option<Class> {
        Some(match self {
            Step::Gates => return None,
            Step::Clippy
            | Step::Test
            | Step::Bootstrap
            | Step::Converter
            | Step::ConverterPackages
            | Step::GtkImports
            | Step::CrossBionic => Class::Cargo,
            // tables: its scenario half builds and runs the harness
            Step::Tables | Step::GlProxies | Step::DriveTap | Step::HostFonts => Class::Harness,
            _ => Class::Light,
        })
    }

    /// Reads the build dir through `build_state::ensure_current` once it runs (skip first): refused at entry under `--only` when stale. LauncherClosure's guard sits behind its built-launcher check, so it is not listed.
    fn reads_build_dir(self) -> bool {
        matches!(
            self,
            Step::ConverterPackages | Step::GlProxies | Step::DriveTap | Step::Gates
        )
    }

    /// Why the step has nothing to check here, when its input is absent: printed and counted, not a failure.
    fn skip(self, out: &Path) -> Option<String> {
        match self {
            // both read a native out dir; a checkout without one (main's land verify) has nothing to check
            Step::ConverterPackages | Step::GlProxies | Step::DriveTap | Step::Gates
                if !out.join("graph.json").is_file() =>
            {
                Some(format!(
                    "no {}: run `cargo xtask native` first",
                    out.join("graph.json").display()
                ))
            }
            _ => None,
        }
    }

    /// Runs the step; `skipped` when it checked only part of its input (named skipped, as [`Step::skip`]), `undecided` its gates that could not judge on this host.
    fn run(self, root: &Path, out: &Path) -> Result<StepDone> {
        if self == Step::Keepset {
            // reads the checkout's own build dir (keepset.rs own_graph), not an explicit --out
            return crate::keepset::check_own(root).map(Into::into);
        }
        if self == Step::LauncherClosure {
            return crate::launcher_closure::check_own(root).map(Into::into);
        }
        if self == Step::Tables {
            self.run_whole(root)?;
            // the scenario scripts' parser is the harness's (drive.rs), which this definition-only tool cannot link: its half runs as a harness step; reads no build dir
            return crate::harness::check_step(root, "scenarios", out);
        }
        if self == Step::ConverterPackages {
            // the packages link the build dir's converter rlibs: a build from before a rebase fails late (E0432 on main's new parcelables), so refuse it by name, as the runtime gates do
            crate::build_state::ensure_current(root, out)?;
            let unchecked = packages::check(root, out)?;
            return Ok((!unchecked.is_empty())
                .then(|| {
                    format!(
                        "not checked: {}: run `cargo xtask native` first",
                        unchecked.join(", ")
                    )
                })
                .into());
        }
        if self == Step::GlProxies {
            // reads built binaries: a build from another source state would check stale proxies
            crate::build_state::ensure_current(root, out)?;
            // the binding table is generated by atl-native-bridge's build script: a harness step, run as a process (harness.rs)
            return crate::harness::check_step(root, "gl-proxies", out);
        }
        if self == Step::DriveTap {
            // runs the built runtime: a build from another source state would test stale binaries
            crate::build_state::ensure_current(root, out)?;
            return crate::harness::check_step(root, "drive-tap", out);
        }
        if self == Step::Gates {
            // runs the built runtime, as drive-tap
            crate::build_state::ensure_current(root, out)?;
            return crate::harness::check_step(root, "gates", out);
        }
        if self == Step::HostFonts {
            // product logic (atl-fonts over fontconfig) the definition-only xtask cannot link: a harness step; reads the desktop and the pinned tree, no build dir
            return crate::harness::check_step(root, "host-fonts", out);
        }
        if self == Step::GtkImports {
            // builds the module cdylibs from this checkout's sources, so it reads current binaries and needs no native build dir
            return crate::gtk_imports::check(root, out).map(|()| StepDone::default());
        }
        if self == Step::CrossBionic {
            return cross_bionic(root).map(Into::into);
        }
        self.run_whole(root).map(|()| StepDone::default())
    }

    fn run_whole(self, root: &Path) -> Result<()> {
        match self {
            Step::Fmt => cargo(root, &["fmt", "--all", "--check"], Units::Plain),
            Step::Spdx => spdx::check(root),
            Step::Tables => {
                let arches = arches::load(root)?;
                let layout = Layout::of(root)?;
                arches::check_kernel_arch(&layout, &arches)?;
                arches::check_uapi_records(&layout, &arches)?;
                arches::check_prebuilt_os(&layout, &arches)?;
                arches::check_cross(root, &arches)?;
                arches::check_qemu_target(&layout, &arches)?;
                arches::check_guest_rust(root, &arches)?;
                crate::isa_tiers::check(root, &arches)?;
                eprintln!("tables: {} ok ({} arches)", arches::FILE, arches.len());
                let sources = sources::load(root)?;
                eprintln!(
                    "tables: {} ok ({} aosp projects, {} other upstreams)",
                    sources::FILE,
                    sources.aosp.projects.len(),
                    sources.git.len()
                );
                let workspace = workspace::load(root)?;
                let consumers = deps::Consumers::new(&workspace, &sources, &arches);
                let deps = deps::load(root, &consumers)?;
                eprintln!("tables: {} ok ({} rows)", deps::FILE, deps.len());
                gtk_baseline::check(root, &deps)?;
                eprintln!(
                    "tables: {} ok (row checked against [gtk4] min)",
                    gtk_baseline::FILE
                );
                let providers = providers::load(
                    root,
                    &providers::Targets::new(&workspace.crates, &sources, &deps),
                )?;
                eprintln!("tables: {} ok ({} rows)", providers::FILE, providers.len());
                let licenses = licenses::load(root, &consumers.upstreams)?;
                eprintln!(
                    "tables: {} ok ({} artifacts, deny.toml within runtime)",
                    licenses::FILE,
                    licenses.artifact.len()
                );
                let prune = prune::load(root, &sources)?;
                eprintln!("tables: {} ok ({} rows)", prune::FILE, prune.len());
                let cts = cts_table::load(root)?;
                // the rows against the module index of the pinned tree and the modules' own sources
                let index = aosp::Index::load(root, &sources.aosp)?;
                cts_table::check(&cts, &index, &|tree_path| {
                    layout.locate(&Path::new("aosp").join(tree_path)).dir
                })?;
                eprintln!("tables: {} ok ({} rows)", cts_table::FILE, cts.len());
                let apk_rows = test_apks::check(root)?;
                eprintln!("tables: {} ok ({apk_rows} rows)", test_apks::FILE);
                let mirrors = service_mirrors::load(root)?;
                let entries = service_mirrors::check(root, &mirrors, &sources.aosp, &|project| {
                    layout.locate(&Path::new("aosp").join(project)).dir
                })?;
                eprintln!(
                    "tables: {} ok ({} services, {entries} mirrored entries at the pin)",
                    service_mirrors::FILE,
                    mirrors.len()
                );
                // the OPS table and NUM_OP against AppOpsManager.sAppOpInfos at the pin, row by row
                let app_ops = app_ops_table::check(&sources.aosp, &|project| {
                    layout.locate(&Path::new("aosp").join(project)).dir
                })?;
                let flags = if app_ops.flags.is_empty() {
                    "none".to_owned()
                } else {
                    app_ops
                        .flags
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                eprintln!(
                    "tables: app_ops ok ({} ops at the pin; flags assumed off: {flags})",
                    app_ops.ops
                );
                // fonts.toml: rows = AOSP's named families at the pin
                let fonts = atl_fonts_table::Table::parse(&std::fs::read_to_string(
                    root.join("fonts.toml"),
                )?)?;
                let aosp_fonts = layout
                    .locate(Path::new("aosp/frameworks/base"))
                    .dir
                    .join("data/fonts/fonts.xml");
                fonts.check_against_aosp(
                    &std::fs::read_to_string(&aosp_fonts)
                        .with_context(|| aosp_fonts.display().to_string())?,
                )?;
                eprintln!(
                    "tables: fonts.toml ok ({} rows = AOSP's named families)",
                    fonts.rows.len()
                );
                Ok(())
            }
            Step::Deps => {
                let report = deps::host_problems(root, None)?;
                if !report.problems.is_empty() {
                    bail!(
                        "{} of {} rows not satisfied: {}",
                        report.problems.len(),
                        report.rows,
                        report.problems.join("; ")
                    );
                }
                eprintln!("deps: {}", report.ok_line());
                Ok(())
            }
            Step::Modules => {
                let workspace = workspace::load(root)?;
                let modules = modules::load(root, &workspace)?;
                eprintln!(
                    "modules: {} ok ({} modules, {} crate edges)",
                    modules::FILE,
                    modules.len(),
                    workspace.edges.len()
                );
                Ok(())
            }
            Step::Bootstrap => crate::bootstrap_gate::check(root),
            Step::Drift => patch::check(root),
            Step::Seam => crate::seam_gate::check(root),
            Step::AndroidPaths => crate::android_paths::check(root),
            Step::BwrapFlags => crate::bwrap_flags::check(root),
            Step::Soong => crate::soong::check(root),
            Step::DiagShape => crate::diag_shape::check(root),
            Step::ProviderRefusals => crate::provider_refusals::check(root),
            Step::EnvReads => crate::env_reads::check(root),
            Step::Clippy => cargo(
                root,
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
                Units::Recorded,
            ),
            Step::Deny => {
                // a missing tool fails the step; skipping would report a check that never ran
                cargo(root, &["deny", "--version"], Units::Plain)
                    .context("cargo-deny not installed: cargo install --locked cargo-deny")?;
                cargo(root, &["deny", "--locked", "check"], Units::Plain)
            }
            Step::Test => cargo(root, &["test", "--workspace", "--locked"], Units::Recorded),
            Step::Converter => converter::check(root),
            Step::Libcxx => libcxx_gate::check(root),
            Step::Runtimes => crate::runtimes::check(root),
            Step::Keepset => unreachable!("run handles keepset"),
            Step::LauncherClosure => unreachable!("run handles launcher-closure"),
            Step::ConverterPackages => unreachable!("run handles converter-packages"),
            Step::GlProxies => unreachable!("run handles gl-proxies"),
            Step::DriveTap => unreachable!("run handles drive-tap"),
            Step::Gates => unreachable!("run handles gates"),
            Step::HostFonts => unreachable!("run handles host-fonts"),
            Step::GtkImports => unreachable!("run handles gtk-imports"),
            Step::CrossBionic => unreachable!("run handles cross-bionic"),
        }
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let steps: Vec<Step> = ALL
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(s))
        .collect();
    let out = out::out_dir(root, args.out.as_deref())?;
    // refused before step 1, with the text the late guard prints (build_state): a full run needs every gate's goals (land's build stage), --only only the guards its steps reach
    let refusal = if args.only.is_empty() {
        build_state::build_dir_staleness(root, args.out.as_deref())?
    } else if steps
        .iter()
        .any(|s| s.reads_build_dir() && s.skip(&out).is_none())
    {
        build_state::ensure_current(root, &out)
            .err()
            .map(|e| format!("{e:#}"))
    } else {
        None
    };
    if let Some(why) = refusal {
        println!(
            "{}",
            Verdict {
                cmd: "check",
                total: None,
                skipped: Vec::new(),
                failures: vec![Failure::gate(&why)],
            }
            .line()
        );
        return Ok(ExitCode::FAILURE);
    }
    let ws = Layout::of(root)?.workspace().to_owned();
    let mut skipped = Vec::new();
    // gates that could not judge on this host: named on the verdict line, neither pass nor fail (verdict.rs Outcome::Undecided)
    let mut undecided: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    let mut times = Timings::default();
    for (i, step) in steps.iter().enumerate() {
        crate::verdict::step(&format!("check:{}/{}:{}", i + 1, steps.len(), step.name()));
        eprintln!("== check: {}", step.name());
        let mut started = Instant::now();
        let mut wait = Duration::ZERO;
        // every step ends with one `== check: <step> (<s>) <outcome>` line: the time sits on the line that names the outcome
        let outcome = if let Some(why) = step.skip(&out) {
            skipped.push(step.name());
            format!("skipped: {why}")
        } else {
            // one grant per step, around its own work only: check itself holds nothing between steps (no hold-and-wait)
            let run = match step.class() {
                None => step.run(root, &out),
                Some(class) => build_pool::acquire(&ws, root, class).and_then(|grant| {
                    wait = grant.waited;
                    started = Instant::now();
                    eprintln!(
                        "check: {} pool {} cpu {} of {}",
                        step.name(),
                        grant.class,
                        grant.cpu,
                        grant.cpu_of
                    );
                    step.run(root, &out)
                }),
            };
            match run {
                Ok(done) => {
                    let n = done.undecided.len();
                    undecided.extend(done.undecided.iter().map(ToString::to_string));
                    let open = if n == 0 {
                        String::new()
                    } else {
                        format!(" ({n} undecided)")
                    };
                    match done.skipped {
                        None => format!("ok{open}"),
                        Some(why) => {
                            skipped.push(step.name());
                            format!("skipped{open}: {why}")
                        }
                    }
                }
                Err(e) => {
                    failures.push(Failure::step(step.name(), format!("{e:#}")));
                    format!("FAILED: {e:#}")
                }
            }
        };
        let took = started.elapsed();
        times.push_waited(step.name(), took, wait);
        let waited = if wait.is_zero() {
            String::new()
        } else {
            format!(", pool wait {}", timing::secs(wait))
        };
        eprintln!(
            "== check: {} ({}{waited}) {outcome}",
            step.name(),
            timing::secs(took)
        );
    }
    // stderr like the step lines: stdout's last line stays the verdict line atl-out and land read
    eprintln!("{}", times.table("check").trim_end());
    let verdict = Verdict {
        cmd: "check",
        total: Some((steps.len(), "steps")),
        skipped,
        failures,
    };
    println!("{}", verdict.line_with(&undecided));
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The cross-bionic step: `cargo check -p atl-bionic --target <rust>` with the cross environment (cross::cargo_target) of every arches.toml host other than the build machine that names a sysroot. Its build script is the generation; `check` runs it without compiling the crate for the target.
/// A host whose rows are unmet (sysroot not fetched, rust std not added) is skipped and named, as `deps --check` skips cross-only rows.
fn cross_bionic(root: &Path) -> Result<Option<String>> {
    let arches = arches::load(root)?;
    let (build, _) = arches::build_machine(root, &arches)?;
    let mut skipped = Vec::new();
    for (arch, a) in &arches {
        if arch == build || a.host.as_ref().is_none_or(|h| h.sysroot.is_none()) {
            continue;
        }
        let problems = deps::host_rows_problems(root, arch)?;
        if !problems.is_empty() {
            skipped.push(format!("{arch} ({})", problems.join("; ")));
            continue;
        }
        let target = crate::cross::cargo_target(root, arch)?;
        cargo_with(
            root,
            &[
                "check",
                "-p",
                "atl-bionic",
                "--locked",
                "--target",
                &target.triple,
            ],
            Units::Recorded,
            &target.env,
        )
        .with_context(|| format!("atl-bionic generation for host {arch}"))?;
        eprintln!("cross-bionic: {arch} ok");
    }
    Ok((!skipped.is_empty()).then(|| format!("not checked: {}", skipped.join(", "))))
}

/// A cargo step: its output shown live as before, and kept, so a failure's first error line can name the cause.
fn cargo(root: &Path, args: &[&str], units: Units) -> Result<()> {
    cargo_with(root, args, units, &[])
}

/// [`cargo`] with extra environment (a cross host's linker, RUSTFLAGS and probe args).
fn cargo_with(root: &Path, args: &[&str], units: Units, env: &[(String, String)]) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args(args).current_dir(root);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    let run = unit_use::run(&mut cmd, units, Echo::Both)
        .with_context(|| format!("spawning cargo {}", args.join(" ")))?;
    if run.status.success() {
        return Ok(());
    }
    let exit = format!("cargo {} exited with {}", args.join(" "), run.status);
    match verdict::first_failure_line(&format!("{}{}", run.stdout, run.stderr)) {
        Some(line) => bail!("{line}\n{exit}"),
        None => bail!("{exit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_packages_skips_only_without_a_graph() {
        let dir = std::env::temp_dir().join(format!("xtask-check-skip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        assert!(Step::ConverterPackages.skip(&dir).is_some());
        assert!(Step::Fmt.skip(&dir).is_none());
        std::fs::write(dir.join("graph.json"), b"{}").expect("test graph");
        assert!(Step::ConverterPackages.skip(&dir).is_none());
        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    #[test]
    fn a_build_from_another_state_is_refused_by_name() {
        let out = std::env::temp_dir().join(format!("xtask-check-stale-{}", std::process::id()));
        std::fs::create_dir_all(&out).expect("test dir");
        std::fs::write(out.join("built-from"), "aaaa\n").expect("test record");
        let err = crate::build_state::ensure_matches("bbbb", &out).expect_err("another state");
        assert!(
            format!("{err:#}").contains("was built from another source state"),
            "{err:#}"
        );
        assert!(crate::build_state::ensure_matches("aaaa", &out).is_ok());
        std::fs::remove_dir_all(&out).expect("test dir cleanup");
    }
}
