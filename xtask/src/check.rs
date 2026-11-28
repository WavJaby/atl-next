// SPDX-License-Identifier: MIT
//! `xtask check`: the static + unit test tiers.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::build_pool::{self, Class};
use crate::harness::StepDone;
use crate::timing::{self, PoolWaits, Timings};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{self, Failure, Verdict};
use crate::{
    aosp, app_ops_table, arches, build_state, converter, cts_table, deps, gtk_baseline,
    libcxx_gate, licenses, modules, out, packages, patch, providers, prune, service_mirrors,
    sources, test_apks, workspace,
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
    /// one walk of the tracked + new, non-ignored files, then each source rule over it, each with its own verdict line (source_scan.rs `RULES`): spdx (SPDX header on every file); seam (libbinder's driver I/O only through BinderDriver.h, patches/aosp/frameworks/native 0002, in the AOSP tree this checkout builds); android-paths (Android path literals only in atl-android-paths, others allowed by android-paths.toml); bwrap-flags (bwrap flag literals only in atl-bwrap, besides the golden argv tests); soong (code never invokes Soong, retired a84d9b6); diag-shape (no free-text `refused` / `unimplemented` log line: outcomes through atl-diag); provider-refusals (the WebView provider throws UnsupportedOperationException only for its listed class-C methods); env-reads (no environment-variable switch in product code; no Cargo.toml enables a `break-*` / `tier-*` feature)
    SourceScan,
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
    /// clippy on all targets, warnings denied
    Clippy,
    /// cargo-deny: advisories, bans, licences, sources
    Deny,
    /// the workspace's tests in two phases: `cargo test --no-run` under Cargo admission, then each test binary by its learned demand (run in a managed scope), the doc tests under Cargo admission (test_run.rs)
    Test,
    /// the Blueprint converter (tools/bp2ninja): go build, go vet, go test against the pinned blueprint, offline
    Converter,
    /// the distro libc++ headers under the converter's device C++ flags: -D__ANDROID__ changes no macro beyond the expected and no object byte
    Libcxx,
    /// AOSP llvm_android's builders.py agrees with runtimes.rs on every define it sets as a literal
    Runtimes,
    /// clippy + unit tests on the packages that link converter modules (feature `converter`), against the native out dir's graph; skipped without one
    ConverterPackages,
    /// the runtime gates listed in xtask-harness check_gates.rs `ROWS` (deterministic on a build machine, each well under a minute), each as `cargo xtask test` runs it, on the native out dir: among them gl-proxies, drive-tap, host-fonts and gtk-imports; a row with nothing to check here is named on the step's skip line; skipped without a native out dir
    Gates,
    /// atl-bionic's generation (its build script, `cargo check --target`) for every cross host whose sysroot and rust std are present, so a cross-only refusal shows before a cross build; a host without them is skipped and named
    CrossBionic,
}

const ALL: [Step; 18] = [
    Step::Fmt,
    Step::SourceScan,
    Step::Tables,
    Step::Deps,
    Step::Keepset,
    Step::LauncherClosure,
    Step::Modules,
    Step::Bootstrap,
    Step::Drift,
    Step::Clippy,
    Step::Deny,
    Step::Test,
    Step::Converter,
    Step::Libcxx,
    Step::Runtimes,
    Step::ConverterPackages,
    Step::Gates,
    Step::CrossBionic,
];

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Fmt => "fmt",
            Step::SourceScan => "source-scan",
            Step::Tables => "tables",
            Step::Deps => "deps",
            Step::Keepset => "keepset",
            Step::LauncherClosure => "launcher-closure",
            Step::Modules => "modules",
            Step::Bootstrap => "bootstrap",
            Step::Drift => "drift",
            Step::Clippy => "clippy",
            Step::Deny => "deny",
            Step::Test => "test",
            Step::Converter => "converter",
            Step::Libcxx => "libcxx",
            Step::Runtimes => "runtimes",
            Step::ConverterPackages => "converter-packages",
            Step::Gates => "gates",
            Step::CrossBionic => "cross-bionic",
        }
    }

    /// The pool class the step's leaf work takes (build_pool.rs): cargo/go compiles and the test run `cargo`, runtime gates `harness`, the rest `light`. None: `gates`, whose rows each take their own grant (check_gates.rs), so the step holds none (a holder above them would refuse their nested admission).
    fn class(self) -> Option<Class> {
        Some(match self {
            // each of these takes its grants in its own phases (check_gates.rs rows, test_run.rs compile, binaries and doc tests)
            Step::Gates | Step::Test => return None,
            // tables: its scenario half builds the harness
            Step::Tables
            | Step::Clippy
            | Step::Bootstrap
            | Step::Converter
            | Step::ConverterPackages
            | Step::CrossBionic => Class::Cargo,
            _ => Class::Light,
        })
    }

    /// Reads the build dir through `build_state::ensure_current` once it runs (skip first): refused at entry under `--only` when stale. LauncherClosure's guard sits behind its built-launcher check, so it is not listed.
    fn reads_build_dir(self) -> bool {
        matches!(self, Step::ConverterPackages | Step::Gates)
    }

    /// Why the step has nothing to check here, when its input is absent: printed and counted, not a failure.
    fn skip(self, out: &Path) -> Option<String> {
        match self {
            // both read a native out dir; a checkout without one (main's land verify) has nothing to check
            Step::ConverterPackages | Step::Gates if !out.join("graph.json").is_file() => {
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
        if self == Step::Gates {
            // runs the built runtime: a build from another source state would test stale binaries
            crate::build_state::ensure_current(root, out)?;
            return crate::harness::check_step(root, "gates", out);
        }
        if self == Step::CrossBionic {
            return cross_bionic(root).map(Into::into);
        }
        self.run_whole(root).map(|()| StepDone::default())
    }

    fn run_whole(self, root: &Path) -> Result<()> {
        match self {
            Step::Fmt => cargo(root, &["fmt", "--all", "--check"], Units::Plain),
            Step::SourceScan => crate::source_scan::check(root),
            Step::Tables => tables(root),
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
            // two phases: the compile under its own Cargo grant, released before the binaries take their demands (test_run.rs); no grant spans both
            Step::Test => {
                let ws = Layout::of(root)?.workspace().to_owned();
                let started = Instant::now();
                {
                    let grant = build_pool::acquire(&ws, root, Class::Cargo)?;
                    eprintln!(
                        "check: test pool {} cpu {} of {} (compile)",
                        grant.class, grant.cpu, grant.cpu_of
                    );
                    cargo(
                        root,
                        &["test", "--no-run", "--workspace", "--locked"],
                        Units::Recorded,
                    )?;
                }
                eprintln!("test: compile {}", timing::secs(started.elapsed()));
                crate::test_run::run(root)
            }
            Step::Converter => converter::check(root),
            Step::Libcxx => libcxx_gate::check(root),
            Step::Runtimes => crate::runtimes::check(root),
            Step::Keepset => unreachable!("run handles keepset"),
            Step::LauncherClosure => unreachable!("run handles launcher-closure"),
            Step::ConverterPackages => unreachable!("run handles converter-packages"),
            Step::Gates => unreachable!("run handles gates"),
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
    eprintln!("check: cpu from {}", timing::cpu_source());
    for (i, step) in steps.iter().enumerate() {
        crate::verdict::step(&format!("check:{}/{}:{}", i + 1, steps.len(), step.name()));
        eprintln!("== check: {}", step.name());
        let mut started = Instant::now();
        let pool_before = build_pool::pool_waits_so_far(&ws);
        // a harness step's grants are queued in its own process: its StepDone carries them
        let mut harness_waits = PoolWaits::default();
        // the step's processes are in check's scope; its managed rows are not: their CPU is the rows tally's delta plus the harness's own
        let mut harness_rows = Duration::ZERO;
        let meter = timing::CpuMeter::start();
        let rows_before = timing::row_cpu_so_far();
        // every step ends with one `== check: <step> (<s>) <outcome>` line: the time sits on the line that names the outcome
        let outcome = if let Some(why) = step.skip(&out) {
            skipped.push(step.name());
            format!("skipped: {why}")
        } else {
            // one grant per step, around its own work only: check itself holds nothing between steps (no hold-and-wait)
            let run = match step.class() {
                None => step.run(root, &out),
                Some(class) => build_pool::acquire(&ws, root, class).and_then(|grant| {
                    // the clock starts at the grant: a class step's queue time is outside its seconds
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
                    harness_waits = done.pool_waits;
                    harness_rows = done.row_cpu;
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
        let rows = timing::row_cpu_so_far().saturating_sub(rows_before) + harness_rows;
        let cpu = meter.stop(rows);
        let waits = build_pool::pool_waits_so_far(&ws).since(pool_before) + harness_waits;
        times.push_step(step.name(), took, waits, cpu);
        let waited = if waits.total.is_zero() {
            String::new()
        } else {
            format!(", pool wait {}", timing::secs(waits.total))
        };
        eprintln!(
            "== check: {} ({}{waited}, cpu {}) {outcome}",
            step.name(),
            timing::secs(took),
            timing::secs(cpu)
        );
    }
    // stderr like the step lines: stdout's last line stays the verdict line atl-out and land read
    eprintln!("{}", times.table("check").trim_end());
    eprintln!("{}", times.critical());
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

/// One independent table check of [`tables`]: its `tables:` lines on a pass.
type TableCheck<'a> = Box<dyn Fn() -> Result<Vec<String>> + Sync + 'a>;

/// The tables step's Rust half: the base tables (arches, sources, workspace) load first, then every check that reads only them runs on the step's grant width (`build_pool::held_jobs`).
/// Same verdict as one after another: lines printed in table order, the first failing check in that order is the step's error.
fn tables(root: &Path) -> Result<()> {
    let arches = arches::load(root)?;
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let workspace = workspace::load(root)?;
    let consumers = deps::Consumers::new(&workspace, &sources, &arches);
    let aosp_dir = |project: &str| layout.locate(&Path::new("aosp").join(project)).dir;
    let checks: Vec<TableCheck<'_>> = vec![
        Box::new(|| {
            arches::check_kernel_arch(&layout, &arches)?;
            arches::check_uapi_records(&layout, &arches)?;
            arches::check_prebuilt_os(&layout, &arches)?;
            arches::check_cross(root, &arches)?;
            arches::check_qemu_target(&layout, &arches)?;
            arches::check_guest_rust(root, &arches)?;
            arches::check_native_bridge(root, &arches)?;
            crate::isa_tiers::check(root, &arches)?;
            Ok(vec![
                format!("{} ok ({} arches)", arches::FILE, arches.len()),
                format!(
                    "{} ok ({} aosp projects, {} other upstreams)",
                    sources::FILE,
                    sources.aosp.projects.len(),
                    sources.git.len()
                ),
            ])
        }),
        Box::new(|| {
            let deps = deps::load(root, &consumers)?;
            let mut lines = vec![format!("{} ok ({} rows)", deps::FILE, deps.len())];
            gtk_baseline::check(root, &deps)?;
            lines.push(format!(
                "{} ok (row checked against [gtk4] min)",
                gtk_baseline::FILE
            ));
            let providers = providers::load(
                root,
                &providers::Targets::new(&workspace.crates, &sources, &deps),
            )?;
            lines.push(format!("{} ok ({} rows)", providers::FILE, providers.len()));
            Ok(lines)
        }),
        Box::new(|| {
            let licenses = licenses::load(root, &consumers.upstreams)?;
            Ok(vec![format!(
                "{} ok ({} artifacts, deny.toml within runtime)",
                licenses::FILE,
                licenses.artifact.len()
            )])
        }),
        Box::new(|| {
            let prune = prune::load(root, &sources)?;
            Ok(vec![format!("{} ok ({} rows)", prune::FILE, prune.len())])
        }),
        Box::new(|| {
            let cts = cts_table::load(root)?;
            // the rows against the module index of the pinned tree and the modules' own sources
            let index = aosp::Index::load(root, &sources.aosp)?;
            cts_table::check(&cts, &index, &aosp_dir)?;
            Ok(vec![format!("{} ok ({} rows)", cts_table::FILE, cts.len())])
        }),
        Box::new(|| {
            let apk_rows = test_apks::check(root)?;
            Ok(vec![format!("{} ok ({apk_rows} rows)", test_apks::FILE)])
        }),
        Box::new(|| {
            let mirrors = service_mirrors::load(root)?;
            let entries = service_mirrors::check(root, &mirrors, &sources.aosp, &aosp_dir)?;
            Ok(vec![format!(
                "{} ok ({} services, {entries} mirrored entries at the pin)",
                service_mirrors::FILE,
                mirrors.len()
            )])
        }),
        Box::new(|| {
            // the OPS table and NUM_OP against AppOpsManager.sAppOpInfos at the pin, row by row
            let app_ops = app_ops_table::check(&sources.aosp, &aosp_dir)?;
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
            Ok(vec![format!(
                "app_ops ok ({} ops at the pin; flags assumed off: {flags})",
                app_ops.ops
            )])
        }),
        Box::new(|| {
            // fonts.toml: rows = AOSP's named families at the pin
            let fonts =
                atl_fonts_table::Table::parse(&std::fs::read_to_string(root.join("fonts.toml"))?)?;
            let aosp_fonts = layout
                .locate(Path::new("aosp/frameworks/base"))
                .dir
                .join("data/fonts/fonts.xml");
            fonts.check_against_aosp(
                &std::fs::read_to_string(&aosp_fonts)
                    .with_context(|| aosp_fonts.display().to_string())?,
            )?;
            Ok(vec![format!(
                "fonts.toml ok ({} rows = AOSP's named families)",
                fonts.rows.len()
            )])
        }),
    ];
    // the grant is this thread's (thread-local): read its width here, not on the workers
    let workers = build_pool::held_jobs().map_or(1, |j| j as usize);
    for result in crate::par::map(&checks, workers, |check| check()) {
        for line in result? {
            eprintln!("tables: {line}");
        }
    }
    Ok(())
}

/// The cross-bionic step:`cargo check -p atl-bionic --target <rust>` with the cross environment (cross::cargo_target) of every arches.toml host other than the build machine that names a sysroot. Its build script is the generation; `check` runs it without compiling the crate for the target.
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
pub(crate) fn cargo(root: &Path, args: &[&str], units: Units) -> Result<()> {
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
