// SPDX-License-Identifier: MIT
//! `xtask test task-bind`: the task-copy namespace (tree.rs) gives javac and ninja the paths a plain build sees. Each check has a control that must fail:
//! - javac: a source under a `--patch-module=java.base` directory compiles when the directory is a bind mount; the control, the same directory reached through a symlink (the removed link farm's shape), fails with "package exists in another module".
//! - verify: run inside a namespace with the right copy bound passes; the wrong copy bound, and no namespace at all, both fail.
//! - git: a worktree bound over its own shared checkout answers `git status` at both paths; without the shadow binds (tree::GitShadow) it fails.
//! - compiler cache: with a plain view's sccache server running, a compile in the namespace through the task view's socket yields the task copy's object; the control (the plain server) yields the unpatched one.
//! - `--plain <out> --task <out>`: the two native out dirs' build.ninja differ only on lines naming a bound project (`src/aosp/<project>/`); one other differing line fails.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::native::CompilerCache;
use crate::tree::{self, Overlay};

#[derive(clap::Args)]
pub struct Args {
    /// A plain (no task copies) native out dir, compared with `--task`.
    #[arg(long, requires = "task")]
    plain: Option<PathBuf>,
    /// A task build's native out dir of the same checkout and goals.
    #[arg(long, requires = "plain")]
    task: Option<PathBuf>,
    /// Internal: run `tree::verify` on `<copy>=<target>` and exit (the gate runs this inside its namespaces).
    #[arg(long, hide = true, num_args = 2, value_names = ["COPY", "TARGET"])]
    verify_only: Option<Vec<PathBuf>>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if let Some(v) = &args.verify_only {
        tree::verify(&[overlay(&v[0], &v[1])])?;
        return Ok(ExitCode::SUCCESS);
    }
    let scratch = std::env::temp_dir().join(format!("xtask-task-bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let result = checks(root, &scratch, args);
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    Ok(ExitCode::SUCCESS)
}

fn overlay(copy: &Path, target: &Path) -> Overlay {
    Overlay {
        project: "probe".into(),
        copy: copy.to_path_buf(),
        target: target.to_path_buf(),
        git: None,
    }
}

fn checks(root: &Path, scratch: &Path, args: &Args) -> Result<()> {
    let layout = Layout::of(root)?;
    let arches = crate::arches::load(root)?;
    let (name, arch) = crate::arches::build_machine(root, &arches)?;
    let os = arch
        .host
        .as_ref()
        .and_then(|h| h.soong_prebuilt_os.as_deref())
        .with_context(|| format!("{name}: no soong_prebuilt_os"))?;
    let javac = crate::arches::java_tools(&tree::shared_tree(&layout), os)
        .0
        .join("bin/javac");
    ensure!(
        javac.is_file(),
        "{} missing (cargo xtask fetch --execute)",
        javac.display()
    );
    // libcore's shape: javac runs at the tree root with `--patch-module=java.base=.` and a source inside a project, `lib/java/lang/Probe.java`.
    // The project is a task copy (copy/, a second one other/), shown as tree/lib (a bind mount point) or farm/lib (a symlink, the removed farm's shape).
    let (copy, other) = (scratch.join("copy"), scratch.join("other"));
    let (tree_root, farm_root) = (scratch.join("tree"), scratch.join("farm"));
    let target = tree_root.join("lib");
    for d in [&copy, &other] {
        std::fs::create_dir_all(d.join("java/lang"))?;
        std::fs::write(
            d.join("java/lang/Probe.java"),
            "package java.lang;\nclass Probe {}\n",
        )?;
    }
    std::fs::create_dir_all(&target)?;
    std::fs::create_dir_all(&farm_root)?;
    std::os::unix::fs::symlink(&copy, farm_root.join("lib"))?;
    let javac_in = |cwd: &Path, ns: Option<&Path>| -> Result<(bool, String)> {
        let mut cmd = Command::new("bwrap");
        if let Some(bound) = ns {
            cmd.args(tree::bwrap_args(&[overlay(bound, &target)]));
        } else {
            cmd.args(tree::bwrap_args(&[]));
        }
        let o = cmd
            .arg("--chdir")
            .arg(cwd)
            .arg("--")
            .arg(&javac)
            .arg("--patch-module=java.base=.")
            .arg("-d")
            .arg(scratch.join(format!("classes-{}", ns.is_some())))
            .arg("lib/java/lang/Probe.java")
            .output()
            .context("running javac under bwrap")?;
        Ok((
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ))
    };
    let (ok, err) = javac_in(&farm_root, None)?;
    ensure!(
        !ok && err.contains("package exists in another module"),
        "control: javac in the link-farm shape was expected to fail with 'package exists in another module' (the probe does not respond): ok={ok} {err}"
    );
    println!(
        "task-bind: PASS javac control: link-farm shape refused (package exists in another module)"
    );
    let (ok, err) = javac_in(&tree_root, Some(&copy))?;
    ensure!(ok, "javac in the bind-mounted tree failed: {err}");
    println!(
        "task-bind: PASS javac: --patch-module=java.base=. over a bind-mounted project compiles"
    );

    let exe = std::env::current_exe()?;
    let verify_in = |bound: Option<&Path>| -> Result<bool> {
        let mut cmd = Command::new("bwrap");
        match bound {
            Some(b) => cmd.args(tree::bwrap_args(&[overlay(b, &target)])),
            None => cmd.args(tree::bwrap_args(&[])),
        };
        let o = cmd
            .arg("--")
            .arg(&exe)
            .args(["test", "task-bind", "--verify-only"])
            .args([&copy, &target])
            .current_dir(root)
            .output()?;
        Ok(o.status.success())
    };
    ensure!(
        verify_in(Some(&copy))?,
        "verify failed with the right copy bound"
    );
    ensure!(
        !verify_in(Some(&other))?,
        "control: verify passed with the wrong copy bound"
    );
    ensure!(
        !verify_in(None)?,
        "control: verify passed with nothing bound"
    );
    ensure!(
        tree::verify(&[overlay(&copy, &target)]).is_err(),
        "control: verify passed outside any namespace"
    );
    println!(
        "task-bind: PASS verify: right copy passes; wrong copy, empty namespace, no namespace refused"
    );

    // a task copy is a git worktree whose admin dir the bind hides: git at both paths works only with the shadow binds
    let (shared, wt) = (scratch.join("git/shared"), scratch.join("git/wt"));
    std::fs::create_dir_all(&shared)?;
    let git = |dir: &Path, a: &[&str]| -> Result<()> {
        let o = Command::new("git").arg("-C").arg(dir).args(a).output()?;
        ensure!(
            o.status.success(),
            "git {a:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        Ok(())
    };
    git(&shared, &["init", "-q"])?;
    git(
        &shared,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "p",
        ],
    )?;
    git(
        &shared,
        &["worktree", "add", "-q", &wt.display().to_string()],
    )?;
    let with_git = tree::overlays_for_test(&wt, &shared, &scratch.join("git/.shadow"));
    ensure!(
        with_git.git.is_some(),
        "the worktree's .git pointer was not recognised"
    );
    tree::prepare(std::slice::from_ref(&with_git))?;
    let git_in = |o: &Overlay| -> Result<bool> {
        let mut ok = true;
        for dir in [&o.target, &o.copy] {
            ok &= Command::new("bwrap")
                .args(tree::bwrap_args(std::slice::from_ref(o)))
                .args(["--", "git", "-C"])
                .arg(dir)
                .args(["status", "--porcelain"])
                .output()?
                .status
                .success();
        }
        Ok(ok)
    };
    ensure!(
        git_in(&with_git)?,
        "git fails in the namespace with the shadow binds"
    );
    let bare = Overlay {
        git: None,
        ..with_git
    };
    ensure!(
        !git_in(&bare)?,
        "control: git worked with the worktree admin dir hidden"
    );
    println!(
        "task-bind: PASS git: task-copy worktree works in the namespace; control without the shadow fails"
    );

    compiler_cache(scratch, &target, &copy)?;

    if let (Some(plain), Some(task)) = (&args.plain, &args.task) {
        let read = |out: &Path| -> Result<String> {
            let p = out.join("build.ninja");
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))
        };
        let overlays = tree::overlays_of(root)?;
        ensure!(
            !overlays.is_empty(),
            "this checkout has no AOSP task copies: --task cannot differ by one"
        );
        let (plain, task) = (read(plain)?, read(task)?);
        let (n, stray) = ninja_diff(&plain, &task, &overlays);
        if !stray.is_empty() {
            for l in stray.iter().take(5) {
                println!(
                    "task-bind: stray: {}",
                    l.chars().take(300).collect::<String>()
                );
            }
            bail!(
                "{} differing build.ninja lines name no bound project",
                stray.len()
            );
        }
        println!("task-bind: PASS build.ninja: {n} differing lines, all on bound projects' inputs");
    }
    Ok(())
}

/// The sccache server runs the compiler, so it must see the build's view. With a plain build's server running on the same cache dir, a compile inside the namespace through the task view's
/// socket must yield the task copy's object (its tag string); the control sends the same compile to the plain server (the shared server before per-view sockets) and must get the unpatched one.
fn compiler_cache(scratch: &Path, target: &Path, copy: &Path) -> Result<()> {
    const SRC: &str = "probe.c";
    let tag = |name: &str| format!("ATL_TASK_BIND_{name}_{}", std::process::id());
    // target = the unpatched tree's file, copy = the task copy's: same name, different content
    std::fs::write(
        target.join(SRC),
        format!("const char t[] = \"{}\";\n", tag("PLAIN")),
    )?;
    std::fs::write(
        copy.join(SRC),
        format!("const char t[] = \"{}\";\n", tag("PATCHED")),
    )?;
    let task_ov = [overlay(copy, target)];
    let dir = scratch.join("sccache");
    let plain = CompilerCache::for_view("sccache", dir.clone(), &tree::describe(&[]));
    let task = CompilerCache::for_view("sccache", dir, &tree::describe(&task_ov));
    let result = (|| -> Result<()> {
        // the plain build's server, started outside the namespace
        plain.start()?;
        let compile = |cache: &CompilerCache, ns: bool, obj: &str| -> Result<String> {
            let mut cmd = Command::new("bwrap");
            cmd.args(tree::bwrap_args(if ns { &task_ov } else { &[] }));
            let out = scratch.join(obj);
            let o = cmd
                .arg("--")
                .args(["sccache", "cc", "-c"])
                .arg(target.join(SRC))
                .arg("-o")
                .arg(&out)
                .envs(cache.env())
                .output()
                .context("running sccache cc under bwrap")?;
            ensure!(
                o.status.success(),
                "sccache cc: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            Ok(String::from_utf8_lossy(&std::fs::read(&out)?).into_owned())
        };
        let has = |obj: &str, name: &str| obj.contains(&tag(name));
        let o = compile(&plain, false, "plain.o")?;
        ensure!(has(&o, "PLAIN"), "plain compile lacks the plain tag");
        let o = compile(&plain, true, "shared.o")?;
        ensure!(
            has(&o, "PLAIN") && !has(&o, "PATCHED"),
            "control: the plain server compiled the task copy (the probe does not tell views apart)"
        );
        let o = compile(&task, true, "task.o")?;
        ensure!(
            has(&o, "PATCHED"),
            "task build's compile through the compiler cache yielded the unpatched object (server outside the build's view)"
        );
        Ok(())
    })();
    plain.stop();
    task.stop();
    result?;
    println!(
        "task-bind: PASS compiler cache: task view's server compiles the task copy; control via the plain server gets the unpatched one"
    );
    Ok(())
}

/// Lines in only one of the two build.ninja files: (their count, those that name no bound project's `src/aosp/<project>/`).
fn ninja_diff<'a>(plain: &'a str, task: &'a str, overlays: &[Overlay]) -> (usize, Vec<&'a str>) {
    let a: BTreeSet<&str> = plain.lines().collect();
    let b: BTreeSet<&str> = task.lines().collect();
    let marks: Vec<String> = overlays
        .iter()
        .map(|o| format!("{}/", o.target.display()))
        .collect();
    let diff: Vec<&str> = a.symmetric_difference(&b).copied().collect();
    let stray = diff
        .iter()
        .copied()
        .filter(|l| !marks.iter().any(|m| l.contains(m.as_str())))
        .collect();
    (diff.len(), stray)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ninja_diff_names_stray_lines() {
        let o = [overlay(
            Path::new("/w/src-wt/t/aosp/a"),
            Path::new("/w/src/aosp/a"),
        )];
        let plain = "rule cc\nbuild x.o: cc /w/src/aosp/a/x.c\nbuild y.o: cc /w/src/aosp/b/y.c\n";
        // the patch adds a source to project a: allowed
        let task = "rule cc\nbuild x.o: cc /w/src/aosp/a/x.c /w/src/aosp/a/new.c\nbuild y.o: cc /w/src/aosp/b/y.c\n";
        assert_eq!(ninja_diff(plain, task, &o), (2, vec![]));
        // the deliberate break: a project nothing binds differs
        let broken =
            "rule cc\nbuild x.o: cc /w/src/aosp/a/x.c\nbuild y.o: cc /w/src-wt/t/aosp/b/y.c\n";
        let (n, stray) = ninja_diff(plain, broken, &o);
        assert_eq!(n, 2);
        assert_eq!(stray.len(), 2);
    }
}
