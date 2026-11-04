// SPDX-License-Identifier: MIT
//! `xtask check`: the static + unit test tiers.

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::verdict::{self, Failure, Verdict};
use crate::{
    aosp, arches, converter, cts_table, deps, libcxx_gate, licenses, modules, packages, patch,
    providers, prune, sources, spdx, workspace,
};
use atl_layout::Layout;

/// Read size of the cargo tee: 8 KiB, the usual pipe buffer. A chunk size, no measured value behind it.
const TEE_CHUNK: usize = 8192;

#[derive(clap::Args)]
pub struct Args {
    /// Run only these steps (repeatable).
    #[arg(long, value_enum)]
    only: Vec<Step>,
    /// Native build directory for converter-packages (default `<workspace>/out/native/<build machine>`, as `xtask native`); without its graph.json that step is skipped.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Step {
    /// rustfmt, check mode
    Fmt,
    /// SPDX header on every tracked or new, non-ignored file
    Spdx,
    /// repo tables (arches, sources, deps, providers, licenses, prune, cts) parse with their schema and invariants
    Tables,
    /// modules.toml equals the real cross-module crate graph
    Modules,
    /// fetched upstream checkouts match their pins and patches/ (no unexported or uncommitted work)
    Drift,
    /// libbinder's driver I/O goes only through BinderDriver.h (patches/aosp/frameworks/native 0003), in the tree this checkout builds
    Seam,
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
    /// clippy on the packages that link converter modules (feature `converter`), against the native out dir's graph; skipped without one
    ConverterPackages,
}

const ALL: [Step; 12] = [
    Step::Fmt,
    Step::Spdx,
    Step::Tables,
    Step::Modules,
    Step::Drift,
    Step::Seam,
    Step::Clippy,
    Step::Deny,
    Step::Test,
    Step::Converter,
    Step::Libcxx,
    Step::ConverterPackages,
];

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Fmt => "fmt",
            Step::Spdx => "spdx",
            Step::Tables => "tables",
            Step::Modules => "modules",
            Step::Drift => "drift",
            Step::Seam => "seam",
            Step::Clippy => "clippy",
            Step::Deny => "deny",
            Step::Test => "test",
            Step::Converter => "converter",
            Step::Libcxx => "libcxx",
            Step::ConverterPackages => "converter-packages",
        }
    }

    /// Why the step has nothing to check here, when its input is absent: printed and counted, not a failure.
    fn skip(self, out: &Path) -> Option<String> {
        match self {
            Step::ConverterPackages if !out.join("graph.json").is_file() => Some(format!(
                "no {}: run `cargo xtask native` first",
                out.join("graph.json").display()
            )),
            _ => None,
        }
    }

    fn run(self, root: &Path, out: &Path) -> Result<()> {
        match self {
            Step::Fmt => cargo(root, &["fmt", "--all", "--check"]),
            Step::Spdx => spdx::check(root),
            Step::Tables => {
                let arches = arches::load(root)?;
                let layout = Layout::of(root)?;
                arches::check_kernel_arch(&layout, &arches)?;
                arches::check_prebuilt_os(&layout, &arches)?;
                eprintln!("tables: {} ok ({} arches)", arches::FILE, arches.len());
                let sources = sources::load(root)?;
                eprintln!(
                    "tables: {} ok ({} aosp projects, {} other upstreams)",
                    sources::FILE,
                    sources.aosp.projects.len(),
                    sources.git.len()
                );
                let workspace = workspace::load(root)?;
                let consumers = deps::Consumers::new(&workspace, &sources);
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
            ),
            Step::Deny => {
                // a missing tool fails the step; skipping would report a check that never ran
                cargo(root, &["deny", "--version"])
                    .context("cargo-deny not installed: cargo install --locked cargo-deny")?;
                cargo(root, &["deny", "--locked", "check"])
            }
            Step::Test => cargo(root, &["test", "--workspace", "--locked"]),
            Step::Converter => converter::check(root),
            Step::Libcxx => libcxx_gate::check(root),
            Step::ConverterPackages => packages::lint(root, out),
        }
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let steps: Vec<Step> = ALL
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(s))
        .collect();
    let out = match &args.out {
        Some(dir) => std::path::absolute(dir)?,
        None => {
            let arches = arches::load(root)?;
            let (machine, _) = arches::build_machine(root, &arches)?;
            Layout::of(root)?
                .workspace()
                .join("out/native")
                .join(machine)
        }
    };
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for step in &steps {
        eprintln!("== check: {}", step.name());
        if let Some(why) = step.skip(&out) {
            eprintln!("== check: {} skipped: {why}", step.name());
            skipped.push(step.name());
        } else if let Err(e) = step.run(root, &out) {
            eprintln!("== check: {} FAILED: {e:#}", step.name());
            failures.push(Failure::step(step.name(), format!("{e:#}")));
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
fn cargo(root: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args(args).current_dir(root);
    let (status, output) =
        run_teed(cmd).with_context(|| format!("spawning cargo {}", args.join(" ")))?;
    if status.success() {
        return Ok(());
    }
    let exit = format!("cargo {} exited with {status}", args.join(" "));
    match verdict::first_failure_line(&output) {
        Some(line) => bail!("{line}\n{exit}"),
        None => bail!("{exit}"),
    }
}

/// Runs `cmd` with its stdout and stderr shown as they arrive and kept: returns the status and the captured stdout then stderr.
/// Colour and progress stay on only when this process's stderr is a terminal, so cargo's output looks as it did when inherited.
fn run_teed(mut cmd: Command) -> Result<(ExitStatus, String)> {
    let when = if std::io::stderr().is_terminal() {
        "always"
    } else {
        "never"
    };
    let mut child = cmd
        .env("CARGO_TERM_COLOR", when)
        .env("CARGO_TERM_PROGRESS_WHEN", when)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = tee(child.stdout.take().context("cargo stdout")?, true);
    let err = tee(child.stderr.take().context("cargo stderr")?, false);
    let status = child.wait()?;
    let mut text = out.join().unwrap_or_default();
    text.push_str(&err.join().unwrap_or_default());
    Ok((status, text))
}

/// Copies `from` to our stdout (or stderr) as it arrives and returns everything read, lossily decoded.
/// A failed write stops the showing, never the capture: the first error line must survive a closed terminal.
fn tee<R: Read + Send + 'static>(mut from: R, to_stdout: bool) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut chunk = [0u8; TEE_CHUNK];
        let mut showing = true;
        loop {
            match from.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    captured.extend_from_slice(&chunk[..n]);
                    if showing {
                        let shown = if to_stdout {
                            let mut out = std::io::stdout().lock();
                            out.write_all(&chunk[..n]).and_then(|()| out.flush())
                        } else {
                            std::io::stderr().write_all(&chunk[..n])
                        };
                        showing = shown.is_ok();
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    })
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
}
