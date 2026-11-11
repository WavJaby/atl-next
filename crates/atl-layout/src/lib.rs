// SPDX-License-Identifier: MIT
//! Where an upstream path lives for one repo checkout -- the one resolver, used by xtask (fetch, patch, native, check) and by every build script reading upstream sources (through `atl-bridgegen::buildscript`).
//! `src/<up>` is shared by every repo checkout of the workspace and holds the pin + origin/main's patches; a task editing an upstream works in its own copy `src-wt/<task>/<up>` (`xtask patch --edit`; `<task>` = the repo checkout's branch), a git worktree of `src/<up>` on branch `atl-patches-<task>`.
//! A path is resolved through its owner, the shared checkout holding it: the task copy when this repo checkout has one of the owner, else `src/`. So a file of a nested checkout belongs to the nested one, never to a task copy of its parent.
//! Every checkout's git dir lives outside the source tree, in `src/.git-dirs/<dir>.git` ([`Layout::git_dir`]); `src/<dir>/.git` is a gitfile pointing there. So a task copy's worktree admin dir (`<store>/worktrees/<n>`) is never under a path the task-copy namespace binds over, and git works at both paths with no extra binds.
#![forbid(unsafe_code)]
#![expect(
    clippy::disallowed_types,
    reason = "a build-time resolver: it runs git on the host for xtask and build scripts"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// The trunk: a repo checkout on it has no task copies, its patches are what `src/` holds.
pub const MAIN: &str = "main";
const SHARED: &str = "src";
const TASKS: &str = "src-wt";

#[derive(Clone)]
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

    /// Every directory fetched upstream trees live in: the shared tree and the task copies' root.
    pub fn fetched_roots(&self) -> [PathBuf; 2] {
        [self.workspace.join(SHARED), self.workspace.join(TASKS)]
    }

    /// The workspace root (`~/atl-next`): `out/`, `cache/` and the checkouts live under it.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The directory of deps.toml sysroot row `name`: `cache/sysroot/<name>`, holding the unpacked tree `root/` and `stamp.txt` (`xtask deps --fetch` writes both). The only place this path is built: xtask fetches and links against it, build scripts probe it.
    pub fn sysroot(&self, name: &str) -> PathBuf {
        self.workspace.join("cache").join("sysroot").join(name)
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

    /// The git dir of the shared checkout `src/<dir>`: `src/.git-dirs/<dir>.git` (the `.git` suffix keeps nested checkouts' dirs apart). The only place this path is built.
    pub fn git_dir(&self, dir: &Path) -> PathBuf {
        git_store(&self.src, dir)
    }

    /// What a build script watches besides the located path so cargo reruns it when `locate(path)` would answer differently: the repo checkout's HEAD (a branch switch), and on a task branch the owner's worktree list
    /// `<git dir>/worktrees` (`xtask patch --edit` / `--done` add and remove entries there). Before the owner's first worktree that dir does not exist, and cargo reruns a script on every build for a missing input, so the git dir itself stands in: creating `worktrees` changes it.
    pub fn resolution_inputs(&self, path: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.head.iter().cloned().collect();
        if self.task.is_some()
            && let Some(owner) = self.owner(path)
        {
            let store = self.git_dir(owner);
            let worktrees = store.join("worktrees");
            out.push(if worktrees.is_dir() { worktrees } else { store });
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

/// The git dir of checkout `<root>/<dir>` in the store under `root`: `<root>/.git-dirs/<dir>.git`. `src/` uses it through [`Layout::git_dir`]; xtask's own sparse caches (the AOSP blueprint index) take the same shape under their root.
pub fn git_store(root: &Path, dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(".git");
    root.join(".git-dirs").join(name)
}

/// A sysroot's libc++ headers: the one `c++/v1` directory holding `__config`, with `__config_site` beside it.
/// The one derivation: xtask native passes it to bp2ninja (`-host-libcxx`), atl-bridgegen's probes put it on the include path.
pub fn libcxx_dir(sysroot: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    config_dirs(sysroot, &mut found)?;
    ensure!(
        found.len() == 1,
        "sysroot {} holds {} c++/v1 directories with __config ({found:?}), want exactly one",
        sysroot.display(),
        found.len()
    );
    let dir = found.swap_remove(0);
    ensure!(
        dir.join("__config_site").exists(),
        "libc++ headers {} have no __config_site beside __config (a split install is not supported)",
        dir.display()
    );
    Ok(dir)
}

/// Each `c++/v1` directory under `dir` that holds a `__config`. Symlinked directories are not followed, as filepath.WalkDir does not.
fn config_dirs(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            config_dirs(&path, found)?;
        } else if entry.file_name().to_str() == Some("__config") {
            found.extend(
                path.parent()
                    .filter(|p| p.ends_with("c++/v1"))
                    .map(Path::to_path_buf),
            );
        }
    }
    Ok(())
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

    /// A shared checkout `src/<dir>`: its gitfile and its git dir in the store (all the resolver looks at).
    fn shared(ws: &Path, dir: &str) {
        let d = ws.join("src").join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let store = Layout::new(ws, None).git_dir(Path::new(dir));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(d.join(".git"), format!("gitdir: {}\n", store.display())).unwrap();
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
        let task = Layout::new(&ws, Some("t"));
        // no worktree yet: the git dir, whose entry list changes when `worktrees` appears
        assert_eq!(
            task.resolution_inputs(p),
            vec![ws.join("src/.git-dirs/aosp/a.git")]
        );
        std::fs::create_dir_all(ws.join("src/.git-dirs/aosp/a.git/worktrees")).unwrap();
        assert_eq!(
            task.resolution_inputs(p),
            vec![ws.join("src/.git-dirs/aosp/a.git/worktrees")]
        );
        // a nested checkout's dir does not collide with its parent's
        assert_eq!(
            task.git_dir(Path::new("aosp/a/b")),
            ws.join("src/.git-dirs/aosp/a/b.git")
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

    /// A fake sysroot under the temp dir holding empty `files` (paths relative to the root).
    fn fake_sysroot(name: &str, files: &[&str]) -> PathBuf {
        let root = temp(&format!("sysroot-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        root
    }

    #[test]
    fn libcxx_dir_is_the_one_c_plus_plus_v1_holding_config() {
        let v1 = "usr/lib/llvm-19/include/c++/v1";
        let root = fake_sysroot(
            "libcxx-one",
            &[
                &format!("{v1}/__config"),
                &format!("{v1}/__config_site"),
                // experimental/__config is not the libc++ root: not counted
                &format!("{v1}/experimental/__config"),
            ],
        );
        assert_eq!(libcxx_dir(&root).unwrap(), root.join(v1));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn libcxx_dir_refuses_two_copies_and_a_split_install() {
        let two = fake_sysroot(
            "libcxx-two",
            &[
                "a/c++/v1/__config",
                "a/c++/v1/__config_site",
                "b/c++/v1/__config",
                "b/c++/v1/__config_site",
            ],
        );
        let err = format!("{:#}", libcxx_dir(&two).unwrap_err());
        assert!(err.contains("want exactly one"), "{err}");
        std::fs::remove_dir_all(&two).unwrap();
        let split = fake_sysroot("libcxx-split", &["c++/v1/__config"]);
        let err = format!("{:#}", libcxx_dir(&split).unwrap_err());
        assert!(err.contains("__config_site"), "{err}");
        std::fs::remove_dir_all(&split).unwrap();
    }
}
