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
/// The build dir every build sees, workspace-relative: xtask's build namespace binds the checkout's own build dir over it (xtask `out::CANONICAL`), so one path names each checkout's own.
pub const BUILD: &str = "out/build";
/// [`Layout::resolution_key`] as xtask last wrote it, in the build dir.
const STAMP: &str = "layout.stamp";

#[derive(Clone)]
pub struct Layout {
    workspace: PathBuf,
    src: PathBuf,
    /// Task name (the branch) and `src-wt/<task>`; None on main or a detached HEAD.
    task: Option<(String, PathBuf)>,
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
        Ok(Self::new(ws, branch.as_deref()))
    }

    /// Workspace `ws` as a repo checkout on `branch` sees it (None = detached HEAD).
    pub fn new(ws: &Path, branch: Option<&str>) -> Self {
        Layout {
            workspace: ws.to_path_buf(),
            src: ws.join(SHARED),
            task: branch
                .filter(|b| *b != MAIN)
                .map(|b| (b.to_owned(), ws.join(TASKS).join(b))),
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

    /// The directory of deps.toml sysroot row `name` pinning `pins`: `cache/sysroot/<name>-<sysroot_key(pins)>`, holding the unpacked tree `root/` and `stamp.txt` (`xtask deps --fetch` writes both). The only place this path is built: xtask fetches and links against it, build scripts probe it.
    /// Content-keyed: checkouts pinning different lists share `cache/` without refetching over each other (keyed by name alone, one checkout's fetch broke another's land).
    pub fn sysroot(&self, name: &str, pins: &[String]) -> PathBuf {
        self.sysroots()
            .join(format!("{name}-{}", sysroot_key(pins)))
    }

    /// `cache/sysroot`: every row's trees, each pin set its own dir ([`Layout::sysroot`]).
    pub fn sysroots(&self) -> PathBuf {
        self.workspace.join("cache").join("sysroot")
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

    /// What `locate` answers depend on, as text: one line per task copy this checkout resolves into (`src-wt/<task>/<owner>`, workspace-relative), sorted. Empty on main, detached and on a task without copies: those resolve alike, so a build dir copied from main's base stays fresh for them.
    /// The task name alone is not in it: a branch switch that moves no path must not rerun every upstream-reading build script.
    pub fn resolution_key(&self) -> Result<String> {
        let Some((_, task_dir)) = &self.task else {
            return Ok(String::new());
        };
        let mut owners = Vec::new();
        stores(&self.src.join(".git-dirs"), Path::new(""), &mut owners)?;
        owners.sort();
        let mut out = String::new();
        for owner in owners {
            let copy = task_dir.join(&owner);
            if copy.join(".git").exists() {
                let rel = copy.strip_prefix(&self.workspace).unwrap_or(&copy);
                out.push_str(&format!("{}\n", rel.display()));
            }
        }
        Ok(out)
    }

    /// The resolution stamp at the build path every build sees ([`BUILD`]): inside xtask's build namespace the checkout's own; outside it names the empty mount point.
    pub fn resolution_stamp(&self) -> PathBuf {
        self.workspace.join(BUILD).join(STAMP)
    }

    /// For xtask, before any build: write [`Layout::resolution_key`] to the stamp in `build_dir` (the checkout's build dir, which its namespace binds at [`BUILD`]) when it differs. An equal stamp keeps its mtime, so a build dir copied from a base keeps its build scripts fresh. True = written.
    pub fn write_resolution_stamp(&self, build_dir: &Path) -> Result<bool> {
        let stamp = build_dir.join(STAMP);
        let key = self.resolution_key()?;
        if std::fs::read_to_string(&stamp).is_ok_and(|had| had == key) {
            return Ok(false);
        }
        std::fs::write(&stamp, key).with_context(|| format!("writing {}", stamp.display()))?;
        Ok(true)
    }

    /// For build scripts: the one input that changes when `locate` would answer differently -- the stamp xtask writes. Never the checkout's git HEAD: a build dir copied to another checkout would watch the HEAD of the checkout that built it and rerun on every move of it.
    /// Missing or not equal to [`Layout::resolution_key`] is an error, not a fallback: plain `cargo` outside xtask is no build entry, so nothing would keep the stamp current.
    pub fn resolution_input(&self) -> Result<PathBuf> {
        let stamp = self.resolution_stamp();
        let fix = "build through `cargo xtask` (it writes the stamp in its build namespace; plain cargo is no build entry)";
        let had = std::fs::read_to_string(&stamp)
            .ok()
            .with_context(|| format!("{} missing: {fix}", stamp.display()))?;
        ensure!(
            had == self.resolution_key()?,
            "{} is stale (task copies changed since xtask wrote it): {fix}",
            stamp.display()
        );
        Ok(stamp)
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

/// Each store under `.git-dirs` (`<rel>.git`), as the shared checkout's path `<rel>`. Not descended into: a nested checkout's store is a sibling (`a.git`, `a/b.git`).
fn stores(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let dir = root.join(rel);
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        match name.to_str().and_then(|n| n.strip_suffix(".git")) {
            Some(owner) => out.push(rel.join(owner)),
            None => stores(root, &rel.join(&name), out)?,
        }
    }
    Ok(())
}

/// The repo checkout holding a package dir (a build script's `CARGO_MANIFEST_DIR`): the nearest ancestor with arches.toml.
pub fn checkout(manifest_dir: &Path) -> Result<PathBuf> {
    manifest_dir
        .ancestors()
        .find(|d| d.join("arches.toml").is_file())
        .map(Path::to_path_buf)
        .with_context(|| format!("no arches.toml above {}", manifest_dir.display()))
}

/// Prints `cargo::rerun-if-changed` for `path` from the build script of the package at `CARGO_MANIFEST_DIR` ([`rerun_arg`]). The one way a build script names a path input.
#[expect(
    clippy::print_stdout,
    reason = "a build script's stdout is cargo's instruction channel"
)]
pub fn rerun_if_changed(path: &Path) {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    println!(
        "cargo::rerun-if-changed={}",
        rerun_arg(&manifest, path).display()
    );
}

/// `path` as a rerun input of package `manifest`: relative to the package dir when inside the repo checkout holding it (cargo resolves a relative path there), so the recorded inputs never name the checkout's path and a build dir copied to another checkout stays fresh.
/// As given otherwise: `src/`, `cache/` and [`BUILD`] are one path for every checkout.
pub fn rerun_arg(manifest: &Path, path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix(manifest) {
        return rest.to_owned();
    }
    let Ok(root) = checkout(manifest) else {
        return path.to_owned();
    };
    match (path.strip_prefix(&root), manifest.strip_prefix(&root)) {
        (Ok(rest), Ok(pkg)) => pkg
            .components()
            .map(|_| Path::new(".."))
            .collect::<PathBuf>()
            .join(rest),
        _ => path.to_owned(),
    }
}

/// The git dir of checkout `<root>/<dir>` in the store under `root`: `<root>/.git-dirs/<dir>.git`. `src/` uses it through [`Layout::git_dir`]; xtask's own sparse caches (the AOSP blueprint index) take the same shape under their root.
pub fn git_store(root: &Path, dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(".git");
    root.join(".git-dirs").join(name)
}

/// The key of a sysroot pin list: FNV-1a 64 over the pins in row order, each `\n`-terminated (order matters: later debs unpack over earlier ones), 16 hex digits. Stable across Rust versions, unlike `DefaultHasher`; not a security hash -- `stamp.txt` checks each deb's SHA256.
pub fn sysroot_key(pins: &[String]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut h = OFFSET;
    for byte in pins.iter().flat_map(|p| p.bytes().chain(*b"\n")) {
        h = (h ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    format!("{h:016x}")
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

    /// Two pin sets of one row get two dirs; the same set (another checkout) the same dir. Expected FNV-1a 64 values: the published test vectors ("" -> cbf29ce484222325, "a" -> af63dc4c8601ec8c); "a\n" is that "a" state stepped once more with 0x0a. Break: keying by name alone collides the first pair.
    #[test]
    fn sysroot_dirs_are_keyed_by_pins() {
        let l = Layout::new(Path::new("/ws"), Some("t"));
        let pins = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(sysroot_key(&[]), "cbf29ce484222325");
        let a = (0xaf63_dc4c_8601_ec8c_u64 ^ 0x0a).wrapping_mul(0x0100_0000_01b3);
        assert_eq!(sysroot_key(&pins(&["a"])), format!("{a:016x}"));
        let one = l.sysroot("r", &pins(&["a=1", "b=1"]));
        assert_ne!(one, l.sysroot("r", &pins(&["a=1", "b=2"])));
        assert_ne!(one, l.sysroot("r", &pins(&["b=1", "a=1"])));
        assert_eq!(
            one,
            Layout::new(Path::new("/ws"), None).sysroot("r", &pins(&["a=1", "b=1"]))
        );
        assert!(one.starts_with("/ws/cache/sysroot"));
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

    /// The key lists task copies only: main, detached and a copy-less task share one key (a base's stamp stays valid for them); a copy, or the same copy under another task, changes it. A nested checkout's store is found beside its parent's.
    #[test]
    fn resolution_key_names_the_task_copies() {
        let ws = temp("key");
        shared(&ws, "aosp/a");
        shared(&ws, "aosp/a/b");
        shared(&ws, "qemu");
        let task = Layout::new(&ws, Some("t"));
        assert_eq!(task.resolution_key().unwrap(), "");
        assert_eq!(Layout::new(&ws, None).resolution_key().unwrap(), "");
        copy(&ws, "t", "qemu");
        copy(&ws, "t", "aosp/a/b");
        assert_eq!(
            task.resolution_key().unwrap(),
            "src-wt/t/aosp/a/b\nsrc-wt/t/qemu\n"
        );
        copy(&ws, "u", "qemu");
        assert_eq!(
            Layout::new(&ws, Some("u")).resolution_key().unwrap(),
            "src-wt/u/qemu\n"
        );
        // main never sees task copies
        assert_eq!(Layout::new(&ws, Some(MAIN)).resolution_key().unwrap(), "");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A build script's input: missing and stale are errors naming xtask; xtask's write leaves an equal stamp untouched (its mtime is what cargo compares).
    #[test]
    fn resolution_input_is_the_stamp_xtask_keeps_current() {
        let ws = temp("stamp");
        shared(&ws, "qemu");
        std::fs::create_dir_all(ws.join(BUILD)).unwrap();
        let task = Layout::new(&ws, Some("t"));
        let e = format!("{:#}", task.resolution_input().unwrap_err());
        assert!(e.contains("missing") && e.contains("cargo xtask"), "{e}");
        let dir = ws.join(BUILD);
        assert!(task.write_resolution_stamp(&dir).unwrap());
        let stamp = task.resolution_input().unwrap();
        assert_eq!(stamp, ws.join("out/build/layout.stamp"));
        let mtime = || std::fs::metadata(&stamp).unwrap().modified().unwrap();
        let before = mtime();
        assert!(!task.write_resolution_stamp(&dir).unwrap());
        assert_eq!(mtime(), before);
        copy(&ws, "t", "qemu");
        let e = format!("{:#}", task.resolution_input().unwrap_err());
        assert!(e.contains("stale"), "{e}");
        assert!(task.write_resolution_stamp(&dir).unwrap());
        assert!(task.resolution_input().is_ok());
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Inside the checkout a rerun path is package-relative (also through `..`, and the checkout's root files); outside it stays as given.
    #[test]
    fn rerun_args_are_package_relative_inside_the_checkout() {
        let co = temp("rerun");
        let pkg = co.join("crates/p");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(co.join("arches.toml"), "").unwrap();
        let arg = |p: &Path| rerun_arg(&pkg, p);
        assert_eq!(arg(&pkg.join("src/x.rs")), Path::new("src/x.rs"));
        assert_eq!(arg(&pkg.join("../q/src")), Path::new("../q/src"));
        assert_eq!(arg(&co.join("deps.toml")), Path::new("../../deps.toml"));
        assert_eq!(
            arg(Path::new("/ws/src/aosp/x.h")),
            Path::new("/ws/src/aosp/x.h")
        );
        std::fs::remove_dir_all(&co).unwrap();
    }

    /// `of` on a real worktree: workspace = the main checkout's parent, task = the worktree's branch.
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
