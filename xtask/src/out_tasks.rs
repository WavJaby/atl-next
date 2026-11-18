// SPDX-License-Identifier: MIT
//! `cargo xtask out prune --tasks`: the task dirs `<workspace>/out/<task>/` (task = a worktree or branch name; a task's run outputs, data roots and logs) whose task is retired.
//! Every top-level entry of `out/` gets one class. Keep: a name in [`KEEP`] (pool, shared tools, locks), never removable. Live: the name is a checkout id ([`out::live_ids_of`]: worktree with its checkout dir present), a local or remote branch not merged into [`MAIN_REF`], or a process has its cwd, an open fd or a mapped file inside it. Orphan: a dir whose name is a known task id (any branch, a pool build dir or a driver-family checkout dir of that name) and which is not live: listed with its size, removed by `--execute`. Unknown: everything else (loose files, symlinks, dirs naming no task id, names that are not plain UTF-8): listed, never removed.
//! A deletion tool removes only what it can prove belongs to a retired task: a dir whose branch was deleted and that no build dir names is unknown, not orphan.
//! The process probe reads `/proc/<pid>/{cwd,fd/*,maps}` of the caller's uid only (other users' processes cannot hold this user's `out/`); a same-uid process whose entries cannot be read makes `--execute` refuse. `--execute` repeats the probe right before each removal.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::fetch::MAIN_REF;
use crate::git::git;
use crate::out;
use crate::out_prune;

/// Top-level `out/` entries that are no task's: never listed as removable. Explicit names only, never a pattern: an entry not here is judged as a task dir or left unknown.
pub const KEEP: [(&str, &str); 16] = [
    (
        "pool",
        "build dirs and bases (out.rs POOL): `out prune` owns them",
    ),
    (
        "build",
        "canonical build path (atl_layout::BUILD): empty on the host, sandboxed builds hold fds into it",
    ),
    (
        "vk-driver",
        "per-checkout vk-driver probe output: `out prune` sweeps its children by checkout liveness",
    ),
    (
        "egl-driver",
        "per-checkout egl-driver probe output: `out prune` sweeps its children by checkout liveness",
    ),
    (
        "gles2-fixture",
        "harness gles2-fixture default output, shared by checkouts",
    ),
    (
        "jni-fixture",
        "harness jni-fixture default output, shared by checkouts",
    ),
    (
        "host-fonts",
        "harness host-fonts default output, shared by checkouts",
    ),
    (
        "agent-tools-target",
        "cargo target of the host agent tools (atl-job, atl-*)",
    ),
    (
        "land-seq.sh",
        "land sequence wrapper the parent runs through atl-job",
    ),
    ("land-flow-test", "agent tool dir"),
    ("udev-fix", "agent tool dir"),
    ("laptop-sync", "agent tool dir"),
    ("laptop.lock", "host lock: one laptop run at a time"),
    ("sifive.lock", "host lock: one sifive run at a time"),
    ("rvvm.lock", "host lock: one rvvm run at a time"),
    ("land.lock", "land lock: one land at a time"),
];

/// Why a task dir is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    Worktree,
    Branch,
    Process(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    Keep(&'static str),
    Live(Live),
    Orphan,
    Unknown(&'static str),
}

#[derive(Debug)]
pub struct Entry {
    /// Lossy name, `{:?}`-printed where control characters may hide.
    pub name: String,
    pub path: PathBuf,
    pub class: Class,
    /// Apparent bytes, each inode once; None for keep entries (never sized: the pool alone is the volume).
    pub bytes: Option<u64>,
}

/// What the classification reads besides the dir itself.
#[derive(Debug, Default)]
pub struct Facts {
    /// Live checkout ids (main + worktrees whose checkout dir exists).
    pub checkouts: BTreeSet<String>,
    /// Branch short name (local, or remote with the remote stripped) -> merged into MAIN_REF (every ref of that name).
    pub branches: BTreeMap<String, bool>,
    /// Other task-id evidence: pool build dir ids, driver-family checkout dir names.
    pub known: BTreeSet<String>,
    /// (pid, path) a process holds: cwd, open fds, mapped files.
    pub held: Vec<(u32, PathBuf)>,
}

/// Paths held by processes of this uid, and how many such processes could not be read.
pub fn held_paths() -> Result<(Vec<(u32, PathBuf)>, usize)> {
    let uid = fs::metadata("/proc/self")
        .context("reading /proc/self")?
        .uid();
    let mut held = Vec::new();
    let mut unreadable = 0;
    for e in fs::read_dir("/proc").context("reading /proc")?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let dir = e.path();
        if !fs::metadata(&dir).is_ok_and(|m| m.uid() == uid) {
            continue;
        }
        match proc_paths(&dir) {
            Ok(paths) => held.extend(paths.into_iter().map(|p| (pid, p))),
            // the process ended mid-scan: it holds nothing
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => {
                let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
                if !UNREADABLE_OK.contains(&comm.trim_end()) {
                    unreadable += 1;
                }
            }
        }
    }
    Ok((held, unreadable))
}

/// Same-uid processes whose `/proc` entries are unreadable by design and that hold nothing under `out/`: the user's service manager (`systemd --user` and its PAM helper), non-dumpable, cwd `/` (seen 2026-11-19, WSL Debian 13). Any other unreadable process makes `--execute` refuse.
const UNREADABLE_OK: [&str; 2] = ["systemd", "(sd-pam)"];

fn proc_paths(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = vec![fs::read_link(dir.join("cwd"))?];
    for fd in fs::read_dir(dir.join("fd"))? {
        match fs::read_link(fd?.path()) {
            Ok(p) => paths.push(p),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // maps: `addr perms offset dev inode path`; the path is the sixth field and may hold spaces
    for line in fs::read_to_string(dir.join("maps"))?.lines() {
        if let Some(i) = line.find(" /") {
            paths.push(PathBuf::from(line[i + 1..].trim_end_matches(" (deleted)")));
        }
    }
    Ok(paths)
}

/// Branch short names -> merged into MAIN_REF, for `root`'s repository.
pub fn branches(root: &Path) -> Result<BTreeMap<String, bool>> {
    let list = |extra: &[&str]| -> Result<Vec<String>> {
        let mut args = vec!["for-each-ref", "--format=%(refname)"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["refs/heads", "refs/remotes"]);
        Ok(git(Some(root), &args)?.lines().map(str::to_owned).collect())
    };
    let merged: BTreeSet<String> = list(&["--merged", MAIN_REF])?.into_iter().collect();
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for r in list(&[])? {
        let Some(name) = short_name(&r) else { continue };
        let m = merged.contains(&r);
        out.entry(name).and_modify(|all| *all &= m).or_insert(m);
    }
    Ok(out)
}

/// `refs/heads/x` -> x; `refs/remotes/<remote>/x` -> x; a remote's HEAD -> None.
fn short_name(refname: &str) -> Option<String> {
    if let Some(n) = refname.strip_prefix("refs/heads/") {
        return Some(n.to_owned());
    }
    let (_, n) = refname.strip_prefix("refs/remotes/")?.split_once('/')?;
    (n != "HEAD").then(|| n.to_owned())
}

/// Every child name of `dir`; missing dir: none.
fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect()
}

/// Task-id evidence on disk: pool build dir ids and driver-family checkout dir names.
pub fn known_ids(ws: &Path) -> BTreeSet<String> {
    let mut known: BTreeSet<String> = names(&out::pool(ws).join("ws")).into_iter().collect();
    for family in out::FAMILIES {
        known.extend(names(&ws.join("out").join(family)));
    }
    known
}

fn keep_reason(name: &str) -> Option<&'static str> {
    KEEP.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

/// The first process holding a path inside `dir` (`dir` canonical).
fn holder(dir: &Path, held: &[(u32, PathBuf)]) -> Option<u32> {
    held.iter()
        .find(|(_, p)| p.starts_with(dir))
        .map(|(pid, _)| *pid)
}

fn class_of(path: &Path, name: Option<&str>, facts: &Facts) -> Result<Class> {
    let Some(name) = name.filter(|n| !n.chars().any(char::is_control)) else {
        return Ok(Class::Unknown("name is not plain UTF-8"));
    };
    if let Some(r) = keep_reason(name) {
        return Ok(Class::Keep(r));
    }
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !meta.is_dir() {
        return Ok(Class::Unknown(if meta.is_symlink() {
            "symlink"
        } else {
            "not a dir"
        }));
    }
    if facts.checkouts.contains(name) {
        return Ok(Class::Live(Live::Worktree));
    }
    if facts.branches.get(name) == Some(&false) {
        return Ok(Class::Live(Live::Branch));
    }
    let real = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    if let Some(pid) = holder(&real, &facts.held) {
        return Ok(Class::Live(Live::Process(pid)));
    }
    if facts.branches.contains_key(name) || facts.known.contains(name) {
        return Ok(Class::Orphan);
    }
    Ok(Class::Unknown("names no task id"))
}

/// Apparent bytes under `path`, each inode once; symlinks counted as links, never followed (run trees hold links into the build); unreadable entries count 0.
fn bytes_under(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        let Ok(rd) = fs::read_dir(path) else { return 0 };
        rd.flatten().map(|e| bytes_under(&e.path(), seen)).sum()
    } else if seen.insert((meta.dev(), meta.ino())) {
        meta.len()
    } else {
        0
    }
}

/// One entry per top-level child of `out`, sorted by name.
pub fn classify(out: &Path, facts: &Facts) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for e in fs::read_dir(out).with_context(|| format!("reading {}", out.display()))? {
        let e = e?;
        let path = e.path();
        let os = e.file_name();
        let class = class_of(&path, os.to_str(), facts)?;
        let bytes = match class {
            Class::Keep(_) => None,
            _ => Some(bytes_under(&path, &mut HashSet::new())),
        };
        entries.push(Entry {
            name: os.to_string_lossy().into_owned(),
            path,
            class,
            bytes,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// A fresh process probe of real dir `dir`: the pid of a process holding a path inside it. Refused when a process of this user cannot be read, or `dir` is no real dir.
pub fn held_inside(dir: &Path) -> Result<Option<u32>> {
    let meta = fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    ensure!(
        meta.is_dir() && !meta.is_symlink(),
        "refused: {} is not a real dir",
        dir.display()
    );
    let (held, unreadable) = held_paths()?;
    ensure!(
        unreadable == 0,
        "refused: {unreadable} process(es) of this user unreadable in /proc"
    );
    Ok(holder(&fs::canonicalize(dir)?, &held))
}

/// Removes orphan `e` after a fresh process probe (`probe`: [`held_inside`]; a test passes its own, since a sandbox's `/proc` is not the host's); Ok(Some(pid)) when a process now holds it (skipped).
pub fn remove_orphan(
    out: &Path,
    e: &Entry,
    probe: impl Fn(&Path) -> Result<Option<u32>>,
) -> Result<Option<u32>> {
    ensure!(
        e.class == Class::Orphan,
        "refused: {} is not an orphan",
        e.name
    );
    ensure!(
        e.path.parent() == Some(out),
        "refused: {} is not directly under {}",
        e.path.display(),
        out.display()
    );
    if let Some(pid) = probe(&e.path)? {
        return Ok(Some(pid));
    }
    fs::remove_dir_all(&e.path).with_context(|| format!("removing {}", e.path.display()))?;
    Ok(None)
}

fn label(c: &Class) -> &'static str {
    match c {
        Class::Keep(_) => "keep",
        Class::Live(_) => "live",
        Class::Orphan => "orphan",
        Class::Unknown(_) => "unknown",
    }
}

/// Unknown entries printed by name, biggest first; the rest are counted.
const UNKNOWN_SHOWN: usize = 20;

/// `out prune --tasks [--execute]` for the checkout at `root`.
pub fn run(root: &Path, ws: &Path, execute: bool) -> Result<ExitCode> {
    let out = ws.join("out");
    let (held, unreadable) = held_paths()?;
    let facts = Facts {
        checkouts: out::live_ids_of(root)?,
        branches: branches(root)?,
        known: known_ids(ws),
        held,
    };
    let entries = classify(&out, &facts)?;
    let mut totals: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for e in &entries {
        let t = totals.entry(label(&e.class)).or_default();
        t.0 += 1;
        t.1 += e.bytes.unwrap_or(0);
    }
    let by_size = |c: fn(&Class) -> bool| {
        let mut v: Vec<&Entry> = entries.iter().filter(|e| c(&e.class)).collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.bytes));
        v
    };
    let unknown = by_size(|c| matches!(c, Class::Unknown(_)));
    for e in unknown.iter().take(UNKNOWN_SHOWN) {
        let Class::Unknown(why) = e.class else {
            unreachable!()
        };
        println!(
            "unknown {:?}: {} ({why}; never removed)",
            e.name,
            out_prune::human(e.bytes.unwrap_or(0))
        );
    }
    if unknown.len() > UNKNOWN_SHOWN {
        println!("unknown: {} more not shown", unknown.len() - UNKNOWN_SHOWN);
    }
    if unreadable > 0 {
        println!(
            "out prune --tasks: {unreadable} process(es) of this user unreadable in /proc: --execute refuses"
        );
    }
    let mut failed = 0;
    let (mut removed, mut freed) = (0, 0);
    for e in by_size(|c| *c == Class::Orphan) {
        let size = out_prune::human(e.bytes.unwrap_or(0));
        if !execute {
            println!("orphan task {}: {size}", e.name);
            continue;
        }
        match remove_orphan(&out, e, held_inside) {
            Ok(None) => {
                removed += 1;
                freed += e.bytes.unwrap_or(0);
                println!("orphan task {}: {size}: removed", e.name);
            }
            Ok(Some(pid)) => println!("orphan task {}: busy, skipped: pid {pid} holds it", e.name),
            Err(err) => {
                failed += 1;
                println!("orphan task {}: FAILED: {err:#}", e.name);
            }
        }
    }
    let t = |k: &str| totals.get(k).copied().unwrap_or_default();
    let (o, l, k, u) = (t("orphan"), t("live"), t("keep"), t("unknown"));
    let head = if execute {
        format!(
            "removed {removed} orphan task dir(s) ({})",
            out_prune::human(freed)
        )
    } else {
        format!(
            "dry run, would remove {} orphan task dir(s) ({}) (add --execute)",
            o.0,
            out_prune::human(o.1)
        )
    };
    println!(
        "out prune --tasks: {head}; live {} ({}), keep {}, unknown {} ({}, never removed)",
        l.0,
        out_prune::human(l.1),
        k.0,
        u.0,
        out_prune::human(u.1)
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-out-tasks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    fn dir_with(out: &Path, name: &str, bytes: usize) {
        fs::create_dir_all(out.join(name)).unwrap();
        fs::write(out.join(name).join("f"), vec![0u8; bytes]).unwrap();
    }

    fn class(entries: &[Entry], name: &str) -> Class {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .class
            .clone()
    }

    /// A probe that finds no holder.
    fn free(_: &Path) -> Result<Option<u32>> {
        Ok(None)
    }

    fn facts() -> Facts {
        Facts {
            checkouts: BTreeSet::from(["repo".to_owned(), "wt-live".to_owned()]),
            branches: BTreeMap::from([
                ("br-open".to_owned(), false),
                ("br-merged".to_owned(), true),
            ]),
            known: BTreeSet::from(["gone-co".to_owned()]),
            held: Vec::new(),
        }
    }

    #[test]
    fn each_top_level_entry_gets_its_class() {
        let out = scratch("classes");
        for n in [
            "wt-live",
            "br-open",
            "br-merged",
            "gone-co",
            "stray",
            "pool",
            "build",
            "land.lock",
        ] {
            dir_with(&out, n, 10);
        }
        fs::write(out.join("loose.log"), b"x").unwrap();
        fs::write(out.join("odd\r"), b"").unwrap();
        fs::write(out.join("other.lock"), b"").unwrap();
        std::os::unix::fs::symlink(out.join("br-merged"), out.join("br-link")).unwrap();
        let e = classify(&out, &facts()).unwrap();
        assert_eq!(class(&e, "wt-live"), Class::Live(Live::Worktree));
        assert_eq!(class(&e, "br-open"), Class::Live(Live::Branch));
        assert_eq!(class(&e, "br-merged"), Class::Orphan);
        assert_eq!(class(&e, "gone-co"), Class::Orphan);
        assert!(matches!(class(&e, "stray"), Class::Unknown(_)));
        assert!(matches!(class(&e, "loose.log"), Class::Unknown(_)));
        assert!(matches!(class(&e, "odd\r"), Class::Unknown(_)));
        // a lock file not named in KEEP is never removable, only unknown
        assert!(matches!(class(&e, "other.lock"), Class::Unknown(_)));
        assert!(matches!(class(&e, "br-link"), Class::Unknown(_)));
        for keep in ["pool", "build", "land.lock"] {
            assert!(matches!(class(&e, keep), Class::Keep(_)), "{keep}");
        }
        // sized: 10 bytes of content, one inode
        assert_eq!(
            e.iter().find(|x| x.name == "gone-co").unwrap().bytes,
            Some(10)
        );
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn a_process_with_its_cwd_inside_keeps_a_task_dir_live() {
        let out = scratch("process");
        dir_with(&out, "br-merged", 1);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(out.join("br-merged"))
            .spawn()
            .unwrap();
        let (held, _) = held_paths().unwrap();
        let f = Facts { held, ..facts() };
        let e = classify(&out, &f).unwrap();
        assert_eq!(
            class(&e, "br-merged"),
            Class::Live(Live::Process(child.id()))
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let (held, _) = held_paths().unwrap();
        let e = classify(&out, &Facts { held, ..facts() }).unwrap();
        assert_eq!(class(&e, "br-merged"), Class::Orphan);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn execute_removes_orphans_only() {
        let out = scratch("execute");
        for n in [
            "wt-live",
            "br-open",
            "br-merged",
            "gone-co",
            "stray",
            "pool",
        ] {
            dir_with(&out, n, 3);
        }
        fs::write(out.join("loose.log"), b"x").unwrap();
        let e = classify(&out, &facts()).unwrap();
        for x in e.iter().filter(|x| x.class == Class::Orphan) {
            assert_eq!(remove_orphan(&out, x, free).unwrap(), None);
        }
        for x in e.iter().filter(|x| x.class != Class::Orphan) {
            assert!(remove_orphan(&out, x, free).is_err(), "{} refused", x.name);
        }
        // an orphan a process holds by the time of removal is skipped, not removed
        dir_with(&out, "gone-co", 1);
        let e = classify(&out, &facts()).unwrap();
        let held = e.iter().find(|x| x.name == "gone-co").unwrap();
        assert_eq!(remove_orphan(&out, held, |_| Ok(Some(7))).unwrap(), Some(7));
        assert!(out.join("gone-co").is_dir());
        fs::remove_dir_all(out.join("gone-co")).unwrap();
        let mut left = names(&out);
        left.sort();
        assert_eq!(left, ["br-open", "loose.log", "pool", "stray", "wt-live"]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn branch_short_names_strip_the_remote() {
        assert_eq!(short_name("refs/heads/a/b").as_deref(), Some("a/b"));
        assert_eq!(short_name("refs/remotes/origin/x").as_deref(), Some("x"));
        assert_eq!(short_name("refs/remotes/origin/HEAD"), None);
    }
}
