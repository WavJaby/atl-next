// SPDX-License-Identifier: MIT
//! `xtask check`: the static + unit test tiers.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::unit_use::{self, Echo, Units};
use crate::verdict::{self, Failure, Verdict};
use crate::{
    aosp, arches, converter, cts_table, deps, libcxx_gate, licenses, modules, packages, patch,
    providers, prune, service_mirrors, slot, sources, spdx, test_apks, workspace,
};
use atl_layout::Layout;

#[derive(clap::Args)]
pub struct Args {
    /// Run only these steps (repeatable).
    #[arg(long, value_enum)]
    only: Vec<Step>,
    /// Native build directory for converter-packages (default: this checkout's slot, see `xtask slot`; else `<workspace>/out/native/<build machine>`); without its graph.json that step is skipped.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Step {
    /// rustfmt, check mode
    Fmt,
    /// SPDX header on every tracked or new, non-ignored file
    Spdx,
    /// repo tables (arches, sources, deps, providers, licenses, prune, cts, test-apks) parse with their schema and invariants
    Tables,
    /// deps.toml rows against this host: installed at the required version (`deps --check`)
    Deps,
    /// every [aosp.sparse] row covers the reads of every workspace out dir's build graph (keepset.rs); skipped without one
    Keepset,
    /// modules.toml equals the real cross-module crate graph
    Modules,
    /// fetched upstream checkouts match their pins and patches/ (no unexported or uncommitted work)
    Drift,
    /// libbinder's driver I/O goes only through BinderDriver.h (patches/aosp/frameworks/native 0003), in the tree this checkout builds
    Seam,
    /// Android path literals (`/system`, `/data`, ...) only in crate atl-android-paths, others allowed by android-paths.toml with a reason
    AndroidPaths,
    /// bwrap flag literals only in atl-broker's bwrap.rs, besides the golden argv tests
    BwrapFlags,
    /// tracked code never invokes Soong (`soong_ui`, `lunch`; Soong retired a84d9b6): comments and Markdown pass
    Soong,
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
    /// the guest GL proxies cover the NDK maps and the host fronts export every bound stub (gl_proxies.rs), on the native out dir; skipped without its guest proxies and fronts
    GlProxies,
}

const ALL: [Step; 19] = [
    Step::Fmt,
    Step::Spdx,
    Step::Tables,
    Step::Deps,
    Step::Keepset,
    Step::Modules,
    Step::Drift,
    Step::Seam,
    Step::AndroidPaths,
    Step::BwrapFlags,
    Step::Soong,
    Step::Clippy,
    Step::Deny,
    Step::Test,
    Step::Converter,
    Step::Libcxx,
    Step::Runtimes,
    Step::ConverterPackages,
    Step::GlProxies,
];

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Fmt => "fmt",
            Step::Spdx => "spdx",
            Step::Tables => "tables",
            Step::Deps => "deps",
            Step::Keepset => "keepset",
            Step::Modules => "modules",
            Step::Drift => "drift",
            Step::Seam => "seam",
            Step::AndroidPaths => "android-paths",
            Step::BwrapFlags => "bwrap-flags",
            Step::Soong => "soong",
            Step::Clippy => "clippy",
            Step::Deny => "deny",
            Step::Test => "test",
            Step::Converter => "converter",
            Step::Libcxx => "libcxx",
            Step::Runtimes => "runtimes",
            Step::ConverterPackages => "converter-packages",
            Step::GlProxies => "gl-proxies",
        }
    }

    /// Why the step has nothing to check here, when its input is absent: printed and counted, not a failure.
    fn skip(self, out: &Path) -> Option<String> {
        match self {
            // both read a native out dir; a checkout without one (main's land verify) has nothing to check
            Step::ConverterPackages | Step::GlProxies if !out.join("graph.json").is_file() => {
                Some(format!(
                    "no {}: run `cargo xtask native` first",
                    out.join("graph.json").display()
                ))
            }
            _ => None,
        }
    }

    /// Runs the step; `Some(why)` when it checked only part of its input (named skipped, as [`Step::skip`]).
    fn run(self, root: &Path, out: &Path) -> Result<Option<String>> {
        if self == Step::Keepset {
            // reads the workspace's graphs (every out/slot-<n>), not this checkout's out dir
            return crate::keepset::check_workspace(root);
        }
        if self == Step::ConverterPackages {
            // the packages link the slot's converter rlibs: a slot built before a rebase fails late (E0432 on main's new parcelables), so refuse it by name, as the runtime gates do
            crate::build_state::ensure_current(root, out)?;
            let unchecked = packages::check(root, out)?;
            return Ok((!unchecked.is_empty()).then(|| {
                format!(
                    "not checked: {}: run `cargo xtask native` first",
                    unchecked.join(", ")
                )
            }));
        }
        if self == Step::GlProxies {
            let arches = arches::load(root)?;
            let (arch, _) = arches::build_machine(root, &arches)?;
            // reads built binaries: a slot from another source state would check stale proxies
            crate::build_state::ensure_current(root, out)?;
            return crate::gl_proxies::check(root, out, arch);
        }
        self.run_whole(root).map(|()| None)
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
                // fonts.toml: rows = AOSP's named families at the pin
                let fonts =
                    atl_fonts::Table::parse(&std::fs::read_to_string(root.join("fonts.toml"))?)?;
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
                // gl-drivers.toml: the owner list atld's install step and atl-services read, parsed by the same code
                let gl = atl_services::gl_driver::owner_table(&std::fs::read_to_string(
                    root.join("gl-drivers.toml"),
                )?)
                .map_err(anyhow::Error::msg)?;
                eprintln!("tables: gl-drivers.toml ok ({} rows)", gl.len());
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
            Step::Drift => patch::check(root),
            Step::Seam => crate::binder::seam_check(root),
            Step::AndroidPaths => crate::android_paths::check(root),
            Step::BwrapFlags => crate::bwrap_flags::check(root),
            Step::Soong => crate::soong::check(root),
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
            Step::ConverterPackages => unreachable!("run handles converter-packages"),
            Step::GlProxies => unreachable!("run handles gl-proxies"),
        }
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let steps: Vec<Step> = ALL
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(s))
        .collect();
    let arches = arches::load(root)?;
    let (machine, _) = arches::build_machine(root, &arches)?;
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |workspace| {
        workspace.join("out/native").join(machine)
    })?;
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for step in &steps {
        eprintln!("== check: {}", step.name());
        if let Some(why) = step.skip(&out) {
            eprintln!("== check: {} skipped: {why}", step.name());
            skipped.push(step.name());
        } else {
            match step.run(root, &out) {
                Ok(None) => {}
                Ok(Some(why)) => {
                    eprintln!("== check: {} skipped: {why}", step.name());
                    skipped.push(step.name());
                }
                Err(e) => {
                    eprintln!("== check: {} FAILED: {e:#}", step.name());
                    failures.push(Failure::step(step.name(), format!("{e:#}")));
                }
            }
        }
    }
    let verdict = Verdict {
        cmd: "check",
        total: Some((steps.len(), "steps")),
        skipped,
        failures,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// A cargo step: its output shown live as before, and kept, so a failure's first error line can name the cause.
fn cargo(root: &Path, args: &[&str], units: Units) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args(args).current_dir(root);
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
    fn a_slot_built_from_another_state_is_refused_by_name() {
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
