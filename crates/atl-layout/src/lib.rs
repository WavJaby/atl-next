// SPDX-License-Identifier: MIT
//! Where an upstream path lives for one repo checkout -- the one resolver, used by xtask (fetch, patch, native, check) and by every build script reading upstream sources (through `atl-bridgegen::buildscript`).
//! `src/<up>` is shared by every repo checkout of the workspace and holds the pin + origin/main's patches; a task editing an upstream works in its own copy `src-wt/<task>/<up>` (`xtask patch --edit`; `<task>` = the repo checkout's branch), a git worktree of `src/<up>` on branch `atl-patches-<task>`.
//! A path is resolved through its owner, the shared checkout holding it: the task copy when this repo checkout has one of the owner, else `src/`. So a file of a nested checkout belongs to the nested one, never to a task copy of its parent.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// The trunk: a repo checkout on it has no task copies, its patches are what `src/` holds.
pub const MAIN: &str = "main";
const SHARED: &str = "src";
const TASKS: &str = "src-wt";

pub struct Layout {
    workspace: PathBuf,
    src: PathBuf,
    /// Task name (the branch) and `src-wt/<task>`; None on main or a detached HEAD.
    task: Option<(String, PathBuf)>,
    /// The repo checkout's HEAD file: its branch picks the task. None for `new`.
    head: Option<PathBuf>,
}

/// A path as the resolver places it.
pub struct Located {
    pub dir: PathBuf,
    /// The task owning it; None = the shared `src/` checkout.
    pub task: Option<String>,
}

impl Layout {
    /// The layout of the repo checkout at `repo` (any directory inside it). The workspace is the parent of the main checkout, found through the git common dir, so every worktree resolves the same `src/`, `out/`, `cache/`.
    pub fn of(repo: &Path) -> Result<Self> {
        let common = git(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = PathBuf::from(common);
        // <workspace>/repo/.git -> <workspace>
        let ws = common.parent().and_then(Path::parent).with_context(|| {
            format!(
                "git common dir {} has no workspace parent",
                common.display()
            )
        })?;
        let branch = git(repo, &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
        let head = git(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-path", "HEAD"],
        )?;
        let mut layout = Self::new(ws, branch.as_deref());
        layout.head = Some(head.into());
        Ok(layout)
    }

    /// Workspace `ws` as a repo checkout on `branch` sees it (None = detached HEAD).
    pub fn new(ws: &Path, branch: Option<&str>) -> Self {
        Layout {
            workspace: ws.to_path_buf(),
            src: ws.join(SHARED),
            task: branch
                .filter(|b| *b != MAIN)
                .map(|b| (b.to_owned(), ws.join(TASKS).join(b))),
            head: None,
        }
    }

    /// The workspace root (`~/atl-next`): `out/`, `cache/` and the checkouts live under it.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The shared checkouts' root. Only for what is not one upstream's file: fetch's dry-run summary, the shared AOSP tree's root.
    pub fn src(&self) -> &Path {
        &self.src
    }

    /// The task (branch) and its directory `src-wt/<task>`; None on main or a detached HEAD.
    pub fn task(&self) -> Option<(&str, &Path)> {
        self.task.as_ref().map(|(n, d)| (n.as_str(), d.as_path()))
    }

    /// `path` (relative to `src/`: `qemu`, `aosp/bionic/libc/include`) for this repo checkout: inside the task copy of its owner if one exists, else under `src/`.
    pub fn locate(&self, path: &Path) -> Located {
        if let Some((name, task_dir)) = &self.task
            && let Some(owner) = self.owner(path)
            && task_dir.join(owner).join(".git").exists()
        {
            return Located {
                dir: task_dir.join(path),
                task: Some(name.clone()),
            };
        }
        Located {
            dir: self.src.join(path),
            task: None,
        }
    }

    /// What a build script watches besides the located path so cargo reruns it when `locate(path)` would answer differently: the repo checkout's HEAD (a branch switch), and on a task branch the owner's git dir in `src/` (`xtask patch --edit` / `--done` add and remove its worktrees there).
    pub fn resolution_inputs(&self, path: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.head.iter().cloned().collect();
        if self.task.is_some()
            && let Some(owner) = self.owner(path)
        {
            out.push(self.src.join(owner).join(".git"));
        }
        out
    }

    /// The nearest ancestor of `path` (itself included) that is a shared checkout.
    fn owner<'p>(&self, path: &'p Path) -> Option<&'p Path> {
        path.ancestors()
            .filter(|a| !a.as_os_str().is_empty())
            .find(|a| self.src.join(a).join(".git").exists())
    }

    /// For `xtask patch --edit / --done` only: checkout `dir`'s shared place `src/<dir>`, its task copy's place, the task name.
    pub fn edit_paths(&self, dir: &Path) -> Result<(PathBuf, PathBuf, &str)> {
        let (name, task_dir) = self.task.as_ref().with_context(|| {
            format!(
                "the repo checkout is on {MAIN} or detached: task copies belong to a task branch"
            )
        })?;
        ensure!(
            !name.contains('/'),
            "branch {name} holds `/`, and task copies live in {TASKS}/<branch>/: rename the branch"
        );
        Ok((self.src.join(dir), task_dir.join(dir), name))
    }

    /// Remove the empty directories between a removed task copy and `src-wt/` (the task's own included).
    pub fn remove_empty_parents(&self, copy: &Path) {
        let Some((_, task_dir)) = &self.task else {
            return;
        };
        let stop = task_dir.parent().unwrap_or(task_dir);
        for dir in copy.ancestors().skip(1).take_while(|d| *d != stop) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }
}

/// `git <args>` in `dir`, stdout trimmed.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atl-layout-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A shared checkout `src/<dir>` (a `.git` directory is all the resolver looks at).
    fn shared(ws: &Path, dir: &str) {
        std::fs::create_dir_all(ws.join("src").join(dir).join(".git")).unwrap();
    }

    /// A task copy `src-wt/<task>/<dir>` (a worktree's `.git` is a file).
    fn copy(ws: &Path, task: &str, dir: &str) {
        let d = ws.join("src-wt").join(task).join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".git"), "gitdir: x\n").unwrap();
    }

    #[test]
    fn resolver_prefers_the_task_copy_of_the_owner() {
        let ws = temp("res");
        let p = Path::new("aosp/a/b/inc/x.h");
        shared(&ws, "aosp/a/b");
        let task = Layout::new(&ws, Some("t"));
        assert_eq!(task.locate(p).dir, ws.join("src/aosp/a/b/inc/x.h"));
        // a directory without .git is no task copy
        std::fs::create_dir_all(ws.join("src-wt/t/aosp/a/b")).unwrap();
        assert!(task.locate(p).task.is_none());
        copy(&ws, "t", "aosp/a/b");
        let l = task.locate(p);
        assert_eq!(
            (l.dir, l.task.as_deref()),
            (ws.join("src-wt/t/aosp/a/b/inc/x.h"), Some("t"))
        );
        // the owner itself resolves the same way
        assert_eq!(
            task.locate(Path::new("aosp/a/b")).dir,
            ws.join("src-wt/t/aosp/a/b")
        );
        // a nested checkout owns its files: the parent's task copy does not take them
        shared(&ws, "aosp/a/b/n");
        assert!(task.locate(Path::new("aosp/a/b/n/f")).task.is_none());
        // main and a detached HEAD never see task copies
        assert!(Layout::new(&ws, Some(MAIN)).locate(p).task.is_none());
        assert!(Layout::new(&ws, None).locate(p).task.is_none());
        assert!(Layout::new(&ws, None).edit_paths(Path::new("x")).is_err());
        let e = format!(
            "{:#}",
            Layout::new(&ws, Some("feat/x"))
                .edit_paths(Path::new("x"))
                .unwrap_err()
        );
        assert!(e.contains("rename the branch"), "{e}");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn resolution_inputs_name_the_owner_git_dir_on_a_task_branch() {
        let ws = temp("inputs");
        shared(&ws, "aosp/a");
        let p = Path::new("aosp/a/inc");
        assert_eq!(
            Layout::new(&ws, Some("t")).resolution_inputs(p),
            vec![ws.join("src/aosp/a/.git")]
        );
        // on main a task copy never applies: nothing but HEAD (absent for `new`)
        assert!(Layout::new(&ws, None).resolution_inputs(p).is_empty());
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// `of` on a real worktree: workspace = the main checkout's parent, task = the worktree's branch, HEAD = the worktree's own.
    #[test]
    fn of_a_worktree() {
        let ws = temp("of");
        let repo = ws.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ident = ["-c", "user.name=t", "-c", "user.email=t@t"];
        git(&repo, &["init", "-q", "-b", MAIN]).unwrap();
        git(
            &repo,
            &[
                ident[0],
                ident[1],
                ident[2],
                ident[3],
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        )
        .unwrap();
        let wt = ws.join("wt/x");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "x",
                &wt.display().to_string(),
            ],
        )
        .unwrap();
        let l = Layout::of(&wt).unwrap();
        let ws = ws.canonicalize().unwrap();
        assert_eq!(l.workspace(), ws);
        assert_eq!(l.task().map(|(n, _)| n), Some("x"));
        assert_eq!(l.head, Some(ws.join("repo/.git/worktrees/x/HEAD")));
        assert!(Layout::of(&repo).unwrap().task().is_none());
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
