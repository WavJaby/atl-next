// SPDX-License-Identifier: MIT
//! Where a checkout's git dir lives: `src/.git-dirs/<dir>.git` (atl_layout::Layout::git_dir), `src/<dir>/.git` a relative gitfile. A task copy's worktree admin dir is then `<store>/worktrees/<n>`, outside every path the task-copy namespace binds over, so git works there with no binds of its own.
//! `cargo xtask fetch --migrate-git-dirs [--execute]` moves in-tree git dirs there once per workspace: per checkout rename (same filesystem, no copy, no download -- promisor/sparse config moves with the dir), gitfile, `git worktree repair` of its task copies, then HEAD / branch / status / sparse file compared before and after.
//! Resumable by state, not by a journal: `.git` dir + no store -> migrate; gitfile -> store -> done; no `.git` + store (killed between rename and gitfile) -> write the gitfile, repair; both -> refused, by hand.
//! A task copy whose pointer still leads to the old place is repaired on every run, so a run killed before its repair finishes on the next.
//! Quiet window, enforced: `--execute` holds every build-pool token for the run and refuses while another process of this user has a cwd under `src/` / `src-wt/` or is inside a task-copy namespace of this workspace (tree::ENV naming `src-wt/`), and while any git lock exists.
//! Delete this subcommand (not the gitfile helper) once every workspace is migrated; `fetch` refuses an in-tree git dir until then.

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::git_store;

use crate::build_pool;
use crate::git::git;

/// Write `dir/.git` = `gitdir: <git_dir relative to dir>`: relative, so the pointer survives the workspace moving. Atomic (written in `git_dir`, renamed into place): a kill never leaves a truncated pointer, which would read as pointing elsewhere.
pub fn write_gitfile(dir: &Path, git_dir: &Path) -> Result<()> {
    let file = dir.join(".git");
    let tmp = git_dir.join("atl-gitfile.tmp");
    std::fs::write(
        &tmp,
        format!("gitdir: {}\n", relative(dir, git_dir).display()),
    )
    .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &file)
        .with_context(|| format!("moving {} to {}", tmp.display(), file.display()))
}

/// `to` as a path relative to directory `from` (both absolute, lexical).
fn relative(from: &Path, to: &Path) -> PathBuf {
    let (f, t): (Vec<Component>, Vec<Component>) =
        (from.components().collect(), to.components().collect());
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out: PathBuf = f[common..].iter().map(|_| Component::ParentDir).collect();
    out.extend(&t[common..]);
    out
}

/// The git dir a gitfile at `dir/.git` names (relative pointers resolved against `dir`); None when `.git` is no gitfile.
fn gitfile_target(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join(".git")).ok()?;
    let p = Path::new(text.strip_prefix("gitdir:")?.trim());
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    })
}

/// Same file or directory (canonical paths; false when either is missing).
fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[derive(Debug, PartialEq)]
enum State {
    /// Not checked out.
    Absent,
    Migrate,
    /// The rename happened, the gitfile did not.
    Resume,
    Done,
    Refused(String),
}

fn state(src: &Path, dir: &Path) -> State {
    let tree = src.join(dir);
    let store = git_store(src, dir);
    let git = std::fs::symlink_metadata(tree.join(".git"));
    let has_store = store.is_dir();
    if !tree.exists() {
        return if has_store {
            State::Refused("git dir in the store, no checkout".into())
        } else {
            State::Absent
        };
    }
    match git {
        Ok(m) if m.is_dir() && has_store => {
            State::Refused("git dir both in the tree and in the store".into())
        }
        Ok(m) if m.is_dir() => State::Migrate,
        Ok(_) if has_store => match gitfile_target(&tree) {
            Some(t) if same(&t, &store) => State::Done,
            _ => State::Refused(format!(
                ".git does not point to {} although it exists",
                store.display()
            )),
        },
        Ok(_) => State::Refused("gitfile, no git dir in the store".into()),
        Err(_) if has_store => State::Resume,
        Err(_) => State::Refused("directory without .git".into()),
    }
}

/// One worktree of a checkout: its admin dir in the git dir, the task copy it names.
struct Copy {
    admin: PathBuf,
    dir: PathBuf,
}

impl Copy {
    /// The copy's own `.git` leads to its admin dir.
    fn linked(&self) -> bool {
        gitfile_target(&self.dir).is_some_and(|t| same(&t, &self.admin))
    }
}

/// The worktrees registered in `git_dir` whose task copy exists, and the count of stale ones (copy gone; `git worktree prune` business, left alone).
fn copies(git_dir: &Path) -> Result<(Vec<Copy>, usize)> {
    let mut live = Vec::new();
    let mut stale = 0;
    let Ok(entries) = std::fs::read_dir(git_dir.join("worktrees")) else {
        return Ok((live, stale));
    };
    for e in entries {
        let admin = e?.path();
        let Ok(text) = std::fs::read_to_string(admin.join("gitdir")) else {
            stale += 1;
            continue;
        };
        // `gitdir` names the copy's `.git` file
        match Path::new(text.trim()).parent() {
            Some(dir) if dir.is_dir() => live.push(Copy {
                admin,
                dir: dir.to_path_buf(),
            }),
            _ => stale += 1,
        }
    }
    live.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok((live, stale))
}

/// What must not change: HEAD, branch, `status --porcelain=v2` (tracked files), the sparse-checkout file.
#[derive(Debug, PartialEq)]
struct Snapshot {
    head: String,
    branch: String,
    status: String,
    sparse: String,
}

fn snapshot(dir: &Path) -> Result<Snapshot> {
    let g = |a: &[&str]| git(Some(dir), a);
    let sparse = g(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "info/sparse-checkout",
    ])?;
    Ok(Snapshot {
        head: g(&["rev-parse", "-q", "--verify", "HEAD"]).unwrap_or_default(),
        branch: g(&["symbolic-ref", "-q", "--short", "HEAD"]).unwrap_or_default(),
        // read-only: no index refresh (git.rs `status` note)
        status: g(&[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--untracked-files=no",
        ])?,
        sparse: std::fs::read_to_string(sparse).unwrap_or_default(),
    })
}

/// Git lock files and half-written packs in `git_dir` (top level, refs/, worktree admin dirs, objects/pack): a git process is at work there, or one died mid-write.
fn locks(git_dir: &Path) -> Vec<PathBuf> {
    fn files(dir: &Path, deep: bool, hit: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() && deep => files(&p, deep, hit, out),
                Ok(t) if t.is_file() && e.file_name().to_str().is_some_and(hit) => out.push(p),
                _ => {}
            }
        }
    }
    let lock = |n: &str| n.ends_with(".lock");
    let mut out = Vec::new();
    files(git_dir, false, &lock, &mut out);
    files(&git_dir.join("refs"), true, &lock, &mut out);
    if let Ok(entries) = std::fs::read_dir(git_dir.join("worktrees")) {
        for e in entries.flatten() {
            files(&e.path(), false, &lock, &mut out);
        }
    }
    files(
        &git_dir.join("objects/pack"),
        false,
        &|n| n.starts_with("tmp_"),
        &mut out,
    );
    out
}

/// Other processes of this user that would race the renames: a cwd under `src/` or `src-wt/`, or a task-copy namespace of this workspace.
fn busy_processes(src: &Path, tasks: &Path) -> Vec<String> {
    let me = std::process::id();
    let Ok(uid) = std::fs::metadata("/proc/self").map(|m| m.uid()) else {
        return vec!["/proc unreadable".into()];
    };
    let marker = format!("{}=", crate::tree::ENV);
    let tasks_str = tasks.display().to_string();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me || e.metadata().map(|m| m.uid()).ok() != Some(uid) {
            continue;
        }
        let p = e.path();
        let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
        let who = format!("pid {pid} ({})", comm.trim());
        if let Ok(cwd) = std::fs::read_link(p.join("cwd"))
            && (cwd.starts_with(src) || cwd.starts_with(tasks))
        {
            out.push(format!("{who}: cwd {}", cwd.display()));
        }
        let env = std::fs::read(p.join("environ")).unwrap_or_default();
        if env.split(|b| *b == 0).any(|v| {
            std::str::from_utf8(v)
                .ok()
                .and_then(|v| v.strip_prefix(&marker))
                .is_some_and(|v| v.contains(&tasks_str))
        }) {
            out.push(format!(
                "{who}: inside a task-copy namespace ({})",
                crate::tree::ENV
            ));
        }
    }
    out
}

/// `src-wt/<task>/.shadow`: the git shadows of the removed shadow binds.
fn shadows(tasks: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(tasks)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(".shadow"))
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

/// Remove a shadow: its pointer files (`*.gitfile`) and its mount points (empty outside the namespace); anything else stays and fails the removal.
fn remove_shadow(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            remove_shadow(&p)?;
        } else if p.extension().is_some_and(|x| x == "gitfile") {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    std::fs::remove_dir(dir)
        .with_context(|| format!("{} holds more than a git shadow", dir.display()))
}

/// Retries for the build-pool tokens: a fork-to-exec window is microseconds to milliseconds, so 5 x 50 ms outlasts it, while a real build holds its token for minutes.
const TOKEN_TRIES: u32 = 5;
const TOKEN_RETRY: std::time::Duration = std::time::Duration::from_millis(50);

/// What one run found (and, executed, did).
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub absent: usize,
    pub migrate: usize,
    pub resume: usize,
    pub done: usize,
    /// Task copies whose pointer leads to the old place.
    pub repairs: usize,
    pub stale_worktrees: usize,
    pub shadows: usize,
    /// Refused checkouts, locks, busy processes, held tokens: any one stops `--execute` before it changes anything.
    pub problems: Vec<String>,
}

/// The paths a run works on: workspace `ws` (`src/`, `src-wt/`, the build pool), checkouts `dirs` (relative to `src/`).
pub struct Scope<'a> {
    pub ws: &'a Path,
    pub dirs: &'a [PathBuf],
}

/// Plan, and with `execute` migrate. A dry run changes nothing on disk.
pub fn migrate(scope: &Scope, execute: bool) -> Result<Summary> {
    let src = scope.ws.join("src");
    let tasks = scope.ws.join("src-wt");
    let mut s = Summary::default();
    // held for the whole run: no ninja build starts while the git dirs move.
    // A few tries: a token's flock is shared with every child the last holder's process forked until that child's exec, so a token released a moment ago can still read as held (seen with parallel tests).
    let tokens = build_pool::tokens(scope.ws)?;
    let take = || {
        build_pool::try_take_all(
            &build_pool::dir(scope.ws),
            tokens,
            &format!("migrate-git-dirs pid {}", std::process::id()),
        )
    };
    let mut taken = take()?;
    for _ in 0..TOKEN_TRIES {
        if taken.is_ok() {
            break;
        }
        std::thread::sleep(TOKEN_RETRY);
        taken = take()?;
    }
    let tokens = match taken {
        Ok(t) => t,
        Err(holders) => {
            s.problems.push(format!(
                "{holders} ({})",
                build_pool::dir(scope.ws).display()
            ));
            Vec::new()
        }
    };
    s.problems.extend(busy_processes(&src, &tasks));
    let mut plan = Vec::new();
    for dir in scope.dirs {
        let st = state(&src, dir);
        let git_dir = match st {
            State::Migrate => src.join(dir).join(".git"),
            _ => git_store(&src, dir),
        };
        if !matches!(st, State::Absent) {
            for l in locks(&git_dir) {
                s.problems.push(format!("git lock {}", l.display()));
            }
            let (live, stale) = copies(&git_dir)?;
            s.stale_worktrees += stale;
            s.repairs += live
                .iter()
                .filter(|c| !matches!(st, State::Done) || !c.linked())
                .count();
        }
        match &st {
            State::Absent => s.absent += 1,
            State::Migrate => s.migrate += 1,
            State::Resume => s.resume += 1,
            State::Done => s.done += 1,
            State::Refused(why) => s
                .problems
                .push(format!("{}: {why}", src.join(dir).display())),
        }
        plan.push((dir, st));
    }
    let shadows = shadows(&tasks);
    s.shadows = shadows.len();
    for (dir, st) in &plan {
        match st {
            State::Migrate | State::Resume => println!("git-dirs: {st:?} {}", dir.display()),
            _ => {}
        }
    }
    for p in &s.problems {
        println!("git-dirs: PROBLEM {p}");
    }
    println!(
        "git-dirs: {} to migrate, {} to resume, {} done, {} absent; {} task copies to repair, {} stale worktrees (left alone), {} shadows to remove; {} problems",
        s.migrate,
        s.resume,
        s.done,
        s.absent,
        s.repairs,
        s.stale_worktrees,
        s.shadows,
        s.problems.len()
    );
    if !execute {
        println!("git-dirs: dry run; --execute migrates");
        return Ok(s);
    }
    ensure!(
        s.problems.is_empty(),
        "refusing to migrate: {} problems (above)",
        s.problems.len()
    );
    for (dir, st) in &plan {
        migrate_one(&src, dir, st).with_context(|| format!("migrating {}", dir.display()))?;
    }
    for sh in &shadows {
        remove_shadow(sh)?;
    }
    drop(tokens);
    println!("git-dirs: migrated");
    Ok(s)
}

fn migrate_one(src: &Path, dir: &Path, st: &State) -> Result<()> {
    let tree = src.join(dir);
    let store = git_store(src, dir);
    match st {
        State::Absent => return Ok(()),
        State::Refused(why) => bail!("{why}"),
        State::Migrate => {
            let (live, _) = copies(&tree.join(".git"))?;
            let mut before = vec![(tree.clone(), snapshot(&tree)?)];
            for c in &live {
                before.push((c.dir.clone(), snapshot(&c.dir)?));
            }
            let parent = store.parent().context("store path has no parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev());
            ensure!(
                dev(&tree.join(".git"))? == dev(parent)?,
                "{} and {} are on different filesystems: a rename cannot move it",
                tree.display(),
                parent.display()
            );
            std::fs::rename(tree.join(".git"), &store).with_context(|| {
                format!("moving {}/.git to {}", tree.display(), store.display())
            })?;
            write_gitfile(&tree, &store)?;
            repair(&tree, &store)?;
            for (d, was) in before {
                let now = snapshot(&d)
                    .with_context(|| format!("git in {} after the move", d.display()))?;
                ensure!(
                    now == was,
                    "{} changed in the move:\nbefore {was:?}\nafter  {now:?}",
                    d.display()
                );
            }
        }
        State::Resume => {
            write_gitfile(&tree, &store)?;
            repair(&tree, &store)?;
            check(&tree, &store)?;
        }
        State::Done => {
            let (live, _) = copies(&store)?;
            if live.iter().any(|c| !c.linked()) {
                repair(&tree, &store)?;
                check(&tree, &store)?;
            }
        }
    }
    Ok(())
}

/// `git worktree repair` for every existing task copy: rewrites both pointers to the store.
fn repair(tree: &Path, store: &Path) -> Result<()> {
    let (live, _) = copies(store)?;
    if live.is_empty() {
        return Ok(());
    }
    let mut args = vec!["worktree".to_owned(), "repair".to_owned()];
    args.extend(live.iter().map(|c| c.dir.display().to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git(Some(tree), &args)?;
    Ok(())
}

/// After a resumed step (no before-snapshot survives a kill): git answers at the checkout and at every task copy, each copy linked to its admin dir in the store.
fn check(tree: &Path, store: &Path) -> Result<()> {
    snapshot(tree).with_context(|| format!("git in {}", tree.display()))?;
    for c in copies(store)?.0 {
        ensure!(
            c.linked(),
            "task copy {} still points outside {}",
            c.dir.display(),
            store.display()
        );
        snapshot(&c.dir).with_context(|| format!("git in {}", c.dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::*;

    const IDENT: [&str; 4] = ["-c", "user.name=t", "-c", "user.email=t@t"];

    /// A workspace laid out like `~/atl-next` before the migration: in-tree git dirs; `aosp/b` sparse + promisor-configured with a task copy `src-wt/t/aosp/b` on its own branch,
    /// `aosp/a/n` nested in `aosp/a`, `qemu` a non-AOSP upstream, `aosp/gone` pinned but never fetched, plus `extra` more small checkouts (for the kill test).
    struct Ws {
        root: PathBuf,
        dirs: Vec<PathBuf>,
    }

    impl Ws {
        fn new(tag: &str, extra: usize) -> Self {
            let root =
                std::env::temp_dir().join(format!("xtask-gitdirs-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let mut dirs: Vec<PathBuf> = ["aosp/a", "aosp/a/n", "aosp/b", "qemu", "aosp/gone"]
                .iter()
                .map(PathBuf::from)
                .collect();
            dirs.extend((0..extra).map(|i| PathBuf::from(format!("aosp/x/{i}"))));
            for d in dirs.iter().filter(|d| !d.ends_with("gone")) {
                let t = root.join("src").join(d);
                std::fs::create_dir_all(&t).unwrap();
                let g = |a: &[&str]| git(Some(&t), a).unwrap();
                g(&["init", "-q"]);
                std::fs::write(t.join("f.txt"), d.display().to_string()).unwrap();
                g(&["add", "f.txt"]);
                let mut c = IDENT.to_vec();
                c.extend(["commit", "-q", "-m", "x"]);
                g(&c);
            }
            // the nested checkout is untracked in its parent, as repo leaves it
            std::fs::write(root.join("src/aosp/a/.git/info/exclude"), "n/\n").unwrap();
            let b = root.join("src/aosp/b");
            let g = |a: &[&str]| git(Some(&b), a).unwrap();
            g(&["config", "remote.origin.promisor", "true"]);
            g(&["config", "remote.origin.partialclonefilter", "blob:none"]);
            g(&["sparse-checkout", "set", "--no-cone", "*.txt"]);
            // a dirty tracked file: status must survive the move unchanged
            std::fs::write(b.join("f.txt"), "edited").unwrap();
            let copy = root.join("src-wt/t/aosp/b");
            g(&[
                "worktree",
                "add",
                "-q",
                "-b",
                "atl-patches-t",
                &copy.display().to_string(),
            ]);
            std::fs::write(copy.join("f.txt"), "copy edit").unwrap();
            Ws { root, dirs }
        }

        fn scope(&self) -> Scope<'_> {
            Scope {
                ws: &self.root,
                dirs: &self.dirs,
            }
        }

        fn src(&self, d: &str) -> PathBuf {
            self.root.join("src").join(d)
        }

        /// Every path under `src/` and `src-wt/` with its file content (dirs as empty): equal listings = nothing changed there. The build pool's token files are not counted: a dry run takes the tokens too.
        fn listing(&self) -> Vec<(PathBuf, Vec<u8>)> {
            fn walk(d: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
                for e in std::fs::read_dir(d).unwrap().flatten() {
                    let p = e.path();
                    let t = e.file_type().unwrap();
                    if t.is_dir() {
                        out.push((p.clone(), Vec::new()));
                        walk(&p, out);
                    } else {
                        out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.root.join("src"), &mut out);
            walk(&self.root.join("src-wt"), &mut out);
            out.sort();
            out
        }
    }

    impl Drop for Ws {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn relative_pointer() {
        assert_eq!(
            relative(
                Path::new("/w/src/aosp/a/b"),
                Path::new("/w/src/.git-dirs/aosp/a/b.git")
            ),
            Path::new("../../../.git-dirs/aosp/a/b.git")
        );
    }

    #[test]
    fn dry_run_then_migrate_then_idempotent() {
        let ws = Ws::new("run", 0);
        let snap_b = snapshot(&ws.src("aosp/b")).unwrap();
        let copy = ws.root.join("src-wt/t/aosp/b");
        let snap_copy = snapshot(&copy).unwrap();
        // a leftover shadow of the removed shadow binds
        let shadow = ws.root.join("src-wt/t/.shadow");
        std::fs::create_dir_all(shadow.join("aosp/b")).unwrap();
        std::fs::write(shadow.join("aosp_b.gitfile"), "gitdir: x\n").unwrap();

        let before = ws.listing();
        let s = migrate(&ws.scope(), false).unwrap();
        assert_eq!(ws.listing(), before, "the dry run changed the disk");
        assert_eq!((s.migrate, s.absent, s.repairs, s.shadows), (4, 1, 1, 1));
        assert!(s.problems.is_empty(), "{:?}", s.problems);

        migrate(&ws.scope(), true).unwrap();
        for d in ["aosp/a", "aosp/a/n", "aosp/b", "qemu"] {
            assert!(ws.src(d).join(".git").is_file(), "{d}: no gitfile");
            assert!(
                git_store(&ws.root.join("src"), Path::new(d))
                    .join("HEAD")
                    .is_file()
            );
        }
        assert_eq!(
            std::fs::read_to_string(ws.src("aosp/a/n/.git")).unwrap(),
            "gitdir: ../../../.git-dirs/aosp/a/n.git\n"
        );
        // the copy's pointer leads into the store, both checkouts unchanged, config moved with the dir
        let store_b = ws.root.join("src/.git-dirs/aosp/b.git");
        assert!(same(
            &gitfile_target(&copy).unwrap(),
            &store_b.join("worktrees/b")
        ));
        assert_eq!(snapshot(&ws.src("aosp/b")).unwrap(), snap_b);
        assert_eq!(snapshot(&copy).unwrap(), snap_copy);
        assert_eq!(
            git(
                Some(&ws.src("aosp/b")),
                &["config", "remote.origin.promisor"]
            )
            .unwrap(),
            "true"
        );
        assert!(!shadow.exists());

        let again = migrate(&ws.scope(), true).unwrap();
        assert_eq!(
            (again.migrate, again.resume, again.done, again.repairs),
            (0, 0, 4, 0)
        );
    }

    #[test]
    fn crash_states_resume() {
        let ws = Ws::new("crash", 0);
        migrate(&ws.scope(), true).unwrap();
        // killed between rename and gitfile
        std::fs::remove_file(ws.src("qemu/.git")).unwrap();
        // killed before the copy's repair: its pointer still names the old in-tree place
        let copy = ws.root.join("src-wt/t/aosp/b");
        std::fs::write(
            copy.join(".git"),
            format!("gitdir: {}\n", ws.src("aosp/b/.git/worktrees/b").display()),
        )
        .unwrap();
        let s = migrate(&ws.scope(), false).unwrap();
        assert_eq!((s.resume, s.repairs), (1, 1));
        migrate(&ws.scope(), true).unwrap();
        assert!(ws.src("qemu/.git").is_file());
        snapshot(&ws.src("qemu")).unwrap();
        snapshot(&copy).unwrap();
        // a git dir both in the tree and in the store is left to a human
        std::fs::remove_file(ws.src("qemu/.git")).unwrap();
        git(Some(&ws.src("qemu")), &["init", "-q"]).unwrap();
        let e = format!("{:#}", migrate(&ws.scope(), true).unwrap_err());
        assert!(e.contains("refusing"), "{e}");
    }

    #[test]
    fn quiet_window_guards_each_refuse() {
        let ws = Ws::new("guards", 0);
        let refused = |what: &str| {
            let s = migrate(&ws.scope(), false).unwrap();
            assert!(
                s.problems.iter().any(|p| p.contains(what)),
                "{what} not reported: {:?}",
                s.problems
            );
            let e = format!("{:#}", migrate(&ws.scope(), true).unwrap_err());
            assert!(e.contains("refusing"), "{e}");
            assert!(ws.src("qemu/.git").is_dir(), "{what}: migrated anyway");
        };
        // a held build token
        let pool = build_pool::dir(&ws.root);
        let token =
            build_pool::acquire(&pool, 1, "builder", Duration::from_millis(10), |_| {}).unwrap();
        refused("builder");
        drop(token);
        // a git lock
        let lock = ws.src("qemu/.git/index.lock");
        std::fs::write(&lock, "").unwrap();
        refused("index.lock");
        std::fs::remove_file(&lock).unwrap();
        // a process with its cwd in src/, one inside this workspace's task-copy namespace
        // returned once `env` has exec'd `sleep`: before that its /proc entry may not be the settled process yet (seen once under `cargo test --workspace`)
        let spawn = |cmd: &mut Command| {
            let child = cmd
                .args(["sleep", "30"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let comm = format!("/proc/{}/comm", child.id());
            let settled = (0..400).any(|_| {
                std::thread::sleep(Duration::from_millis(5));
                std::fs::read_to_string(&comm).is_ok_and(|c| c.trim() == "sleep")
            });
            assert!(settled, "sleep never started");
            child
        };
        let mut sleeper = spawn(Command::new("env").current_dir(ws.src("aosp/a")));
        refused(&format!("pid {}", sleeper.id()));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let mut inside = spawn(Command::new("env").env(
            crate::tree::ENV,
            format!("aosp/b={}", ws.root.join("src-wt/t/aosp/b").display()),
        ));
        refused(&format!("pid {}", inside.id()));
        inside.kill().unwrap();
        inside.wait().unwrap();
        // all clear
        assert!(migrate(&ws.scope(), true).unwrap().problems.is_empty());
    }

    const CHILD_ENV: &str = "XTASK_GIT_DIRS_CHILD";

    /// The migration as a separate process, for the kill test: runs only with CHILD_ENV naming the workspace.
    #[test]
    #[ignore = "child of killed_mid_run_resumes"]
    fn child_migrate() {
        let Some(root) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let root = PathBuf::from(root);
        let dirs: Vec<PathBuf> = std::fs::read_to_string(root.join("dirs"))
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .collect();
        migrate(
            &Scope {
                ws: &root,
                dirs: &dirs,
            },
            true,
        )
        .unwrap();
    }

    /// SIGKILL the migration after its first rename, then run it again: it finishes, every checkout and the task copy work.
    #[test]
    fn killed_mid_run_resumes() {
        let ws = Ws::new("kill", 40);
        let list: String = ws
            .dirs
            .iter()
            .map(|d| format!("{}\n", d.display()))
            .collect();
        std::fs::write(ws.root.join("dirs"), list).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_dirs::tests::child_migrate",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD_ENV, &ws.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let store = ws.root.join("src/.git-dirs");
        let started = (0..2000).any(|_| {
            std::thread::sleep(Duration::from_millis(1));
            store.is_dir()
        });
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(started, "the child never started migrating");
        let s = migrate(&ws.scope(), false).unwrap();
        assert!(s.migrate > 0, "the kill came after the last rename: {s:?}");
        migrate(&ws.scope(), true).unwrap();
        let s = migrate(&ws.scope(), false).unwrap();
        assert_eq!((s.migrate, s.resume, s.done, s.repairs), (0, 0, 44, 0));
        snapshot(&ws.root.join("src-wt/t/aosp/b")).unwrap();
    }
}
