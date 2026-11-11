// SPDX-License-Identifier: MIT
//! `cargo xtask slot prune`: deletes the cargo units of the target trees that no recorded build used for `--older-than` days, and the incremental sessions no build will load.
//! Trees (a profile dir holding `.fingerprint`, or `<triple>/<profile>`) under these roots: each slot's `cargo`, `cargo-converter`, `target` (the default of a slotted checkout) and `gl/cargo*`; with `--all` also `repo/target` and `out/egl-driver/<checkout>/cargo*`, which no slot owns.
//! Use is the record each recorded build writes (`unit_use`): a unit's last reported build, fresh or rebuilt. A unit with no entry is judged by its fingerprint dir's mtime, which cargo moves when it rebuilds the unit: approximate, and the worst case is one rebuild.
//! A unit is `.fingerprint/<pkg>-<hash>/`, `build/<pkg>-<hash>/` (build-script output), the `deps/` and `examples/` files `*-<hash>` or `*-<hash>.<ext>`, and the uplifted hard links (`<tree>/<name>`) of those files; `<hash>` is 16 lowercase hex digits.
//! Incremental (`<tree>/incremental/<crate>/`): keeps the newest finalized session, the one rustc loads (`rustc_incremental/src/persist/fs.rs`, `find_source_directory_in_iter`, rustc 1.99.1; rustc reads an older one only when copying the newest fails). Removed: every other finalized session, `-working` sessions older than 10 s (rustc's own grace), and lock files whose session is gone. A held lock is never touched.
//! Kept, never deleted: entries that are no unit, `deps/` and `examples/` files no unit owns (counted as orphans). A tree whose record cannot be read refuses its slot.
//! Busy: a slot's build holds the slot lock (`build_pool::hold_slot`); prune takes it exclusive without waiting and skips the slot. Every tree also has cargo's own build-dir lock (`<tree>/.cargo-lock`), taken without waiting and held from plan to removal: a tree a build holds is skipped and reported.
//! Symlinks are refused, never followed: a link under a root could point the prune outside it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, flock};

use crate::build_pool;
use crate::slot::{self, Holder};
use crate::unit_use::{self, BUILD, DEPS, EXAMPLES, FINGERPRINT};

/// Default `--older-than`, in days: a unit no cargo build used for this long is stale. First guess, not measured. A shorter cutoff frees more but recompiles stable dependencies more often; a rebuilt dependency also recompiles its dependents (2026-11-08: pruning one unit recompiled it and its dependent crate). Re-fit from the rebuild counts after a few lands.
pub const PRUNE_AFTER_DAYS: u64 = 14;

const SECS_PER_DAY: u64 = 86_400;
const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;
/// Target roots directly under a slot: xtask's `--target-dir` names, and `target`, the default a slotted checkout links to (slot.rs).
const SLOT_ROOTS: [&str; 3] = ["cargo", "cargo-converter", "target"];
/// `<slot>/gl/cargo*`: the GL probes' target roots.
const GL_DIR: &str = "gl";
/// `<ws>/out/egl-driver/<checkout>/cargo*`: EGL driver probes, not under a slot.
const EGL_DIR: &str = "egl-driver";
/// Prefix of the root names under `gl/` and `egl-driver/<checkout>/`.
const CARGO_ROOT: &str = "cargo";
/// cargo's lock on a build dir; a build holds it for its whole run.
const CARGO_LOCK: &str = ".cargo-lock";
/// rustc's incremental names: `s-<t>-<sess>-<svh|working>`, lock `s-<t>-<sess>.lock`; `t` is base-36 micros since the epoch.
const SESSION: &str = "s";
const INCREMENTAL: &str = "incremental";
const WORKING: &str = "-working";
const LOCK_EXT: &str = ".lock";
/// rustc's grace before it collects a `-working` session (fs.rs `is_old_enough_to_be_collected`), in micros.
const SESSION_GRACE_MICROS: u64 = 10_000_000;

/// What one slot's prune found: no out dir, a build holds it, or one plan per target tree.
#[derive(Debug)]
pub enum Outcome {
    NotBuilt,
    Busy,
    Done(Vec<TreePlan>),
}

/// One target tree: its units and incremental sessions, and what the stale ones take. `busy`: a cargo build holds the tree; nothing is planned.
#[derive(Debug, Default)]
pub struct TreePlan {
    /// The root it sits under, relative to the workspace, e.g. `out/slot-1/cargo`.
    pub root: String,
    /// The tree relative to the workspace, e.g. `out/slot-1/cargo/release`.
    pub tree: String,
    pub dir: PathBuf,
    pub busy: bool,
    /// Fingerprint entries in the tree.
    pub units: usize,
    pub stale: Vec<Unit>,
    pub kept: usize,
    /// Units with no record entry; judged by mtime. Counted in `kept` or `stale` too.
    pub unrecorded: usize,
    /// Fingerprint entries that are no unit. Kept.
    pub unattributed: usize,
    /// `deps/` and `examples/` files no unit owns. Kept.
    pub orphans: usize,
    pub orphan_bytes: u64,
    /// Incremental sessions and lock files to remove.
    pub sessions: Vec<Session>,
    /// Incremental sessions and lock files kept (held, newest, young, or not a session).
    pub kept_sessions: usize,
}

impl TreePlan {
    fn stale_bytes(&self) -> u64 {
        self.stale.iter().map(|u| u.bytes).sum()
    }

    fn session_bytes(&self) -> u64 {
        self.sessions.iter().map(|s| s.bytes).sum()
    }
}

/// A stale unit: its hash, the paths its removal takes (the fingerprint dir first), and the bytes they hold, each inode once.
#[derive(Debug)]
pub struct Unit {
    pub hash: String,
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
}

/// A stale incremental session: its dir (None for a lock file whose session is gone), its lock file if any, and the bytes its dir holds. `_held`: the lock's flock, taken when planned and kept until the plan is dropped, so no process takes it before removal.
#[derive(Debug)]
pub struct Session {
    pub dir: Option<PathBuf>,
    pub lock: Option<PathBuf>,
    pub bytes: u64,
    _held: Option<File>,
}

/// Totals of one sweep: stale units, stale sessions and their bytes, and the slots or trees that failed.
#[derive(Debug, Default)]
pub struct Sweep {
    pub units: usize,
    pub sessions: usize,
    pub bytes: u64,
    pub failed: usize,
}

/// A target tree found under a root.
struct Found {
    root: PathBuf,
    dir: PathBuf,
}

/// `cargo xtask slot prune`. Exactly one of `slot` / `all` names the scope; `all` adds the trees no slot owns. A dry run unless `execute`.
pub fn run(
    ws: &Path,
    holders: &[Holder],
    pool: u32,
    slot: Option<u32>,
    all: bool,
    older_than: u64,
    execute: bool,
) -> Result<ExitCode> {
    ensure!(
        slot.is_some() != all,
        "name one slot (`slot prune 2`) or every slot (`slot prune --all`)"
    );
    let slots: Vec<u32> = match slot {
        Some(n) => {
            ensure!(
                (1..=pool).contains(&n),
                "slot {n} is outside 1..={pool} (`build.slots`)"
            );
            vec![n]
        }
        None => (1..=pool).collect(),
    };
    let sweep = sweep(ws, holders, &slots, all, older_than, execute)?;
    Ok(if sweep.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Prunes each slot of `slots`, then with `unslotted` the trees no slot owns. Prints one line per target tree, one per root and a summary. Busy and failed scopes are reported; a failure is counted in `failed`, not returned.
pub fn sweep(
    ws: &Path,
    holders: &[Holder],
    slots: &[u32],
    unslotted: bool,
    older_than: u64,
    execute: bool,
) -> Result<Sweep> {
    let cutoff = older_than
        .checked_mul(SECS_PER_DAY)
        .with_context(|| format!("--older-than {older_than} days is too large"))?;
    let now = unit_use::now()?;
    let mut total = Sweep::default();
    let mut roots: BTreeMap<String, Sweep> = BTreeMap::new();
    for &n in slots {
        match prune_slot(ws, n, cutoff, now, execute) {
            Ok(Outcome::NotBuilt) => println!("slot {n}: not built"),
            Ok(Outcome::Busy) => {
                let record: Vec<String> = holders
                    .iter()
                    .filter(|h| h.slot == n)
                    .map(|h| h.checkout.display().to_string())
                    .collect();
                let record = if record.is_empty() {
                    String::new()
                } else {
                    format!(" (record: {})", record.join(", "))
                };
                println!("slot {n}: busy, skipped: a build holds its lock{record}");
            }
            Ok(Outcome::Done(plans)) if plans.is_empty() => println!("slot {n}: no cargo trees"),
            Ok(Outcome::Done(plans)) => report(&plans, &mut total, &mut roots),
            Err(e) => {
                total.failed += 1;
                println!("slot {n}: FAILED: {e:#}");
            }
        }
    }
    if unslotted {
        match prune_unslotted(ws, cutoff, now, execute) {
            Ok(plans) if plans.is_empty() => println!("unslotted: no cargo trees"),
            Ok(plans) => report(&plans, &mut total, &mut roots),
            Err(e) => {
                total.failed += 1;
                println!("unslotted: FAILED: {e:#}");
            }
        }
    }
    for (root, sum) in &roots {
        println!(
            "root {root}: units {}, sessions {}, {}",
            sum.units,
            sum.sessions,
            human(sum.bytes)
        );
    }
    let verb = if execute {
        "removed"
    } else {
        "dry run, would remove (add --execute)"
    };
    println!(
        "slot prune: {verb} {} stale units and {} stale sessions, {}",
        total.units,
        total.sessions,
        human(total.bytes)
    );
    Ok(total)
}

/// Prints each plan's line and adds it to `total` and to its root's sum. Busy trees are reported and add nothing.
fn report(plans: &[TreePlan], total: &mut Sweep, roots: &mut BTreeMap<String, Sweep>) {
    for plan in plans {
        if plan.busy {
            println!("{}: busy, skipped: a cargo build holds its lock", plan.tree);
            continue;
        }
        println!(
            "{}: units {}, stale {} ({}), kept {} (unrecorded {}), unattributed {}, orphan deps {} ({}), sessions stale {} ({}), kept {}",
            plan.tree,
            plan.units,
            plan.stale.len(),
            human(plan.stale_bytes()),
            plan.kept,
            plan.unrecorded,
            plan.unattributed,
            plan.orphans,
            human(plan.orphan_bytes),
            plan.sessions.len(),
            human(plan.session_bytes()),
            plan.kept_sessions
        );
        let bytes = plan.stale_bytes() + plan.session_bytes();
        let sum = roots.entry(plan.root.clone()).or_default();
        for target in [&mut *total, sum] {
            target.units += plan.stale.len();
            target.sessions += plan.sessions.len();
            target.bytes += bytes;
        }
    }
}

/// Plans slot `n`'s prune and, with `execute`, removes what it names. Holds the slot's lock for the whole call.
pub fn prune_slot(ws: &Path, n: u32, cutoff: u64, now: u64, execute: bool) -> Result<Outcome> {
    let dir = slot::slot_dir(ws, n);
    match fs::symlink_metadata(&dir) {
        Ok(meta) => ensure!(
            meta.is_dir(),
            "refused: {} is not a directory (a symlink is never followed)",
            dir.display()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Outcome::NotBuilt),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    check_slot_dir(ws, &dir)?;
    let Some(_lock) = build_pool::try_prune_slot(ws, n)? else {
        return Ok(Outcome::Busy);
    };
    let found = trees_under(&slot_roots(&dir)?)?;
    Ok(Outcome::Done(prune_trees(
        ws, &found, cutoff, now, execute,
    )?))
}

/// The trees no slot owns (`--all`): `repo/target` and the EGL driver probes. Each is busy-checked by its own `.cargo-lock`.
pub fn prune_unslotted(ws: &Path, cutoff: u64, now: u64, execute: bool) -> Result<Vec<TreePlan>> {
    let found = trees_under(&unslotted_roots(ws)?)?;
    prune_trees(ws, &found, cutoff, now, execute)
}

/// The slot number `n` when `dir` is exactly `<ws>/out/slot-<n>`, `n` >= 1 in canonical spelling. Whether `n` is within `build.slots` is the callers' check (`slot::pool`, `run`), so this reads no config.
pub fn pool_slot(ws: &Path, dir: &Path) -> Option<u32> {
    let name = dir.strip_prefix(ws.join("out")).ok()?.to_str()?;
    let n: u32 = name.strip_prefix("slot-")?.parse().ok()?;
    (n >= 1 && slot::slot_dir(ws, n) == dir).then_some(n)
}

/// The slot dir must be a pool slot whose canonical path is directly under `<ws>/out`.
fn check_slot_dir(ws: &Path, dir: &Path) -> Result<()> {
    let out = fs::canonicalize(ws.join("out")).context("reading the out dir")?;
    let real = fs::canonicalize(dir).with_context(|| format!("reading {}", dir.display()))?;
    ensure!(
        pool_slot(ws, dir).is_some() && real.parent() == Some(out.as_path()),
        "refused: {} is not a slot of the pool under {}",
        dir.display(),
        out.display()
    );
    Ok(())
}

/// The roots of slot `slot`: `cargo`, `cargo-converter`, `target`, and `gl/cargo*`.
fn slot_roots(slot: &Path) -> Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = SLOT_ROOTS.iter().map(|name| slot.join(name)).collect();
    roots.extend(prefixed(&slot.join(GL_DIR), CARGO_ROOT)?);
    Ok(roots)
}

/// The roots no slot owns: the main checkout's default target, and each EGL driver checkout's `cargo*`.
fn unslotted_roots(ws: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![ws.join("repo").join("target")];
    for checkout in children(&ws.join("out").join(EGL_DIR))? {
        roots.extend(prefixed(&checkout, CARGO_ROOT)?);
    }
    Ok(roots)
}

/// The entries of `dir` whose name starts with `prefix`. Missing dir: none.
fn prefixed(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    Ok(children(dir)?
        .into_iter()
        .filter(|path| file_name(path).starts_with(prefix))
        .collect())
}

/// Every target tree under `roots`: a child of a root that holds `.fingerprint`, or a grandchild (`<triple>/<profile>`).
fn trees_under(roots: &[PathBuf]) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for root in roots {
        for child in children(root)? {
            if is_tree(&child)? {
                found.push(Found {
                    root: root.clone(),
                    dir: child,
                });
                continue;
            }
            for grand in children(&child)? {
                if is_tree(&grand)? {
                    found.push(Found {
                        root: root.clone(),
                        dir: grand,
                    });
                }
            }
        }
    }
    Ok(found)
}

fn is_tree(dir: &Path) -> Result<bool> {
    Ok(real_dir(dir)? && real_dir(&dir.join(FINGERPRINT))?)
}

/// Plans every tree of `found`, then with `execute` removes what the plans name. A tree whose `.cargo-lock` a build holds is reported busy. Each held lock lasts from its tree's plan to its removal, and a plan error stops the call before anything is removed.
fn prune_trees(
    ws: &Path,
    found: &[Found],
    cutoff: u64,
    now: u64,
    execute: bool,
) -> Result<Vec<TreePlan>> {
    let mut plans = Vec::new();
    let mut locks = Vec::new();
    for f in found {
        let root = relative(ws, &f.root);
        let tree = relative(ws, &f.dir);
        // a dry run only probes: it creates no lock file
        let busy = if execute {
            match hold_tree(&f.dir)? {
                Some(lock) => {
                    locks.push(lock);
                    false
                }
                None => true,
            }
        } else {
            lock_held(&f.dir.join(CARGO_LOCK))?
        };
        if busy {
            plans.push(TreePlan {
                root,
                tree,
                dir: f.dir.clone(),
                busy: true,
                ..TreePlan::default()
            });
        } else {
            plans.push(plan_tree(&f.dir, root, tree, now, cutoff)?);
        }
    }
    if execute {
        for plan in &plans {
            if !plan.busy {
                remove_plan(plan)?;
            }
        }
    }
    drop(locks);
    Ok(plans)
}

/// Removes what `plan` names: stale units (their record entries go after), then stale sessions.
fn remove_plan(plan: &TreePlan) -> Result<()> {
    for unit in &plan.stale {
        for path in &unit.paths {
            remove(&plan.dir, path)?;
        }
    }
    let hashes: Vec<String> = plan.stale.iter().map(|u| u.hash.clone()).collect();
    if !hashes.is_empty() {
        unit_use::forget(&plan.dir, &hashes)?;
    }
    for session in &plan.sessions {
        remove_session(&plan.dir, session)?;
    }
    Ok(())
}

/// cargo's build-dir lock of `tree`, taken without waiting: a build holds it for its whole run. None when one does.
fn hold_tree(tree: &Path) -> Result<Option<File>> {
    let path = tree.join(CARGO_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e).with_context(|| format!("flock {}", path.display())),
    }
}

/// The plan for one tree (its `.cargo-lock` held by the caller). Units are the fingerprint dirs; `build/`, `deps/`, `examples/` files are attributed by hash, uplifted links by inode. A unit's last use is its record entry; without one, its fingerprint dir's mtime. Stale when that is older than the cutoff.
fn plan_tree(tree: &Path, root: String, label: String, now: u64, cutoff: u64) -> Result<TreePlan> {
    let record = unit_use::load(tree)?;
    let mut plan = TreePlan {
        root,
        tree: label,
        dir: tree.to_owned(),
        ..TreePlan::default()
    };
    let mut units: Vec<(PathBuf, String)> = Vec::new();
    for dir in children(&tree.join(FINGERPRINT))? {
        plan.units += 1;
        match unit_use::unit_hash(&file_name(&dir)) {
            Some(hash) => units.push((dir, hash.to_owned())),
            None => plan.unattributed += 1,
        }
    }
    let owned: BTreeSet<String> = units.iter().map(|(_, hash)| hash.clone()).collect();
    let mut outputs: Vec<(PathBuf, Option<String>)> = Vec::new();
    for sub in [DEPS, EXAMPLES] {
        for file in children(&tree.join(sub))? {
            let owner =
                unit_use::output_hash(&file_name(&file)).filter(|hash| owned.contains(hash));
            outputs.push((file, owner));
        }
    }
    // uplifted links sit in the tree root and share the inode of the owned output they link
    let mut owner_of_inode: HashMap<(u64, u64), String> = HashMap::new();
    for (path, owner) in &outputs {
        if let Some(owner) = owner {
            let meta = fs::symlink_metadata(path)
                .with_context(|| format!("reading {}", path.display()))?;
            owner_of_inode.insert((meta.dev(), meta.ino()), owner.clone());
        }
    }
    let mut uplifted: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in children(tree)? {
        let meta =
            fs::symlink_metadata(&file).with_context(|| format!("reading {}", file.display()))?;
        if !meta.is_file() {
            continue;
        }
        if let Some(owner) = owner_of_inode.get(&(meta.dev(), meta.ino())) {
            uplifted.entry(owner.clone()).or_default().push(file);
        }
    }
    for (dir, hash) in units {
        let last_use = match record.as_ref().and_then(|r| r.units.get(&hash)) {
            Some(&at) => at,
            None => {
                plan.unrecorded += 1;
                mtime_secs(&dir)?
            }
        };
        if !is_stale(last_use, now, cutoff) {
            plan.kept += 1;
            continue;
        }
        let mut paths = vec![dir.clone()];
        let build = tree.join(BUILD).join(file_name(&dir));
        if exists(&build)? {
            paths.push(build);
        }
        paths.extend(
            outputs
                .iter()
                .filter(|(_, owner)| owner.as_deref() == Some(hash.as_str()))
                .map(|(path, _)| path.clone()),
        );
        paths.extend(uplifted.remove(&hash).unwrap_or_default());
        let bytes = unique_bytes(&paths)?;
        plan.stale.push(Unit { hash, paths, bytes });
    }
    for (path, owner) in &outputs {
        if owner.is_none() {
            plan.orphans += 1;
            plan.orphan_bytes += unique_bytes(std::slice::from_ref(path))?;
        }
    }
    let (sessions, kept) = stale_sessions(tree, now)?;
    plan.sessions = sessions;
    plan.kept_sessions = kept;
    Ok(plan)
}

/// Stale when the last use is more than `cutoff` seconds before `now`. A use in the future, or a sum that overflows, is kept.
fn is_stale(last_use: u64, now: u64, cutoff: u64) -> bool {
    last_use.saturating_add(cutoff) < now
}

/// Unix seconds of `path`'s mtime.
fn mtime_secs(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    u64::try_from(meta.mtime()).with_context(|| format!("{} is dated before 1970", path.display()))
}

/// The incremental sessions of `tree` no load reads, and how many entries are kept. Per crate dir: the newest finalized session stays, as rustc loads it; other finalized sessions go; `-working` sessions go once older than the grace; a lock held by a process keeps its session; lock files whose session is gone go once older than the grace. Session names that match no rustc shape are kept.
fn stale_sessions(tree: &Path, now: u64) -> Result<(Vec<Session>, usize)> {
    let now_micros = now.saturating_mul(1_000_000);
    let mut stale = Vec::new();
    let mut kept = 0;
    for crate_dir in children(&tree.join(INCREMENTAL))? {
        if !real_dir(&crate_dir)? {
            continue;
        }
        let (dirs, locks) = session_entries(&crate_dir)?;
        let newest = newest_finalized(&dirs);
        for dir in &dirs {
            let name = file_name(dir);
            let Some(t) = session_time(&name) else {
                kept += 1;
                continue;
            };
            let finalized = is_finalized(&name);
            if (finalized && newest == Some(dir)) || (!finalized && !old_enough(t, now_micros)) {
                kept += 1;
                continue;
            }
            let lock = crate_dir.join(lock_name(&name));
            let Lock::Free(held) = take_lock(&lock)? else {
                kept += 1;
                continue;
            };
            stale.push(Session {
                dir: Some(dir.clone()),
                lock: held.is_some().then_some(lock),
                bytes: unique_bytes(std::slice::from_ref(dir))?,
                _held: held,
            });
        }
        for lock in &locks {
            let name = file_name(lock);
            if dirs.iter().any(|dir| lock_name(&file_name(dir)) == name) {
                continue;
            }
            let Some(t) = lock_time(&name) else {
                kept += 1;
                continue;
            };
            if !old_enough(t, now_micros) {
                kept += 1;
                continue;
            }
            let Lock::Free(held @ Some(_)) = take_lock(lock)? else {
                // held, or gone since the listing
                kept += 1;
                continue;
            };
            stale.push(Session {
                dir: None,
                lock: Some(lock.clone()),
                bytes: 0,
                _held: held,
            });
        }
    }
    Ok((stale, kept))
}

/// The `s-` session dirs and lock files directly in a crate dir.
fn session_entries(crate_dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut dirs = Vec::new();
    let mut locks = Vec::new();
    for entry in children(crate_dir)? {
        let name = file_name(&entry);
        if !name.starts_with(SESSION) {
            continue;
        }
        if name.ends_with(LOCK_EXT) {
            locks.push(entry);
        } else if real_dir(&entry)? {
            dirs.push(entry);
        }
    }
    Ok((dirs, locks))
}

/// The finalized session rustc loads: the greatest timestamp (`find_source_directory_in_iter`; the first of equal ones wins).
fn newest_finalized(dirs: &[PathBuf]) -> Option<&PathBuf> {
    let mut best: Option<(u64, &PathBuf)> = None;
    for dir in dirs {
        let name = file_name(dir);
        if !is_finalized(&name) {
            continue;
        }
        let Some(t) = session_time(&name) else {
            continue;
        };
        if best.is_none_or(|(at, _)| t > at) {
            best = Some((t, dir));
        }
    }
    best.map(|(_, dir)| dir)
}

fn is_finalized(name: &str) -> bool {
    !name.ends_with(WORKING)
}

/// The timestamp of a session dir `s-<t>-<sess>-<svh|working>`, in micros since the epoch.
fn session_time(name: &str) -> Option<u64> {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 4 || parts[0] != SESSION {
        return None;
    }
    u64::from_str_radix(parts[1], 36).ok()
}

/// The timestamp of a lock file `s-<t>-<sess>.lock`, in micros since the epoch.
fn lock_time(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(LOCK_EXT)?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() != 3 || parts[0] != SESSION {
        return None;
    }
    u64::from_str_radix(parts[1], 36).ok()
}

/// The lock file of a session dir: its name up to the third dash, plus `.lock` (rustc's `lock_file_path`).
fn lock_name(session: &str) -> String {
    let head: Vec<&str> = session.split('-').take(3).collect();
    format!("{}{LOCK_EXT}", head.join("-"))
}

/// rustc's rule: a session started less than the grace ago may still be starting.
fn old_enough(t_micros: u64, now_micros: u64) -> bool {
    t_micros.saturating_add(SESSION_GRACE_MICROS) < now_micros
}

/// A lock file's state as [`take_lock`] found it.
enum Lock {
    /// Another open file holds the exclusive flock.
    Held,
    /// Free: the file holding the flock now, or None when the lock file does not exist.
    Free(Option<File>),
}

/// Takes `lock`'s exclusive flock without waiting. The caller keeps the returned file while it acts on the lock's state: a flock taken and dropped can still be held after the drop by a child this process forked meanwhile (the open file is shared until the child's exec), so a second probe of a lock this process freed may read it as held.
fn take_lock(lock: &Path) -> Result<Lock> {
    let file = match OpenOptions::new().read(true).open(lock) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Lock::Free(None)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", lock.display())),
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Lock::Free(Some(file))),
        Err(e) if std::io::Error::from(e).kind() == ErrorKind::WouldBlock => Ok(Lock::Held),
        Err(e) => Err(e).with_context(|| format!("flock {}", lock.display())),
    }
}

/// Whether a process holds `lock` (a dry run's busy probe): the flock is taken and dropped at once.
fn lock_held(lock: &Path) -> Result<bool> {
    Ok(matches!(take_lock(lock)?, Lock::Held))
}

/// Removes a stale session: a held lock leaves it alone; otherwise the dir, then its lock file.
fn remove_session(tree: &Path, session: &Session) -> Result<()> {
    // no re-probe: the plan holds the lock since it was planned (Session `_held`)
    if let Some(dir) = &session.dir {
        remove(tree, dir)?;
    }
    if let Some(lock) = &session.lock
        && exists(lock)?
    {
        remove(tree, lock)?;
    }
    Ok(())
}

/// Apparent bytes under `paths`, each inode once: an uplifted link and its deps file are one copy on disk.
fn unique_bytes(paths: &[PathBuf]) -> Result<u64> {
    let mut seen = HashSet::new();
    let mut total = 0;
    for path in paths {
        add_bytes(path, &mut seen, &mut total)?;
    }
    Ok(total)
}

fn add_bytes(path: &Path, seen: &mut HashSet<(u64, u64)>, total: &mut u64) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_dir() {
        for child in children(path)? {
            add_bytes(&child, seen, total)?;
        }
    } else if seen.insert((meta.dev(), meta.ino())) {
        *total += meta.len();
    }
    Ok(())
}

/// Removes a path under `tree`. Refused outside it; a symlink is removed as a link.
fn remove(tree: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(tree),
        "refused: {} is outside {}",
        path.display(),
        tree.display()
    );
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let removed = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    removed.with_context(|| format!("removing {}", path.display()))
}

/// Whether `path` is a directory of its own. Missing or not a directory: false. A symlink is an error.
fn real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                !meta.is_symlink(),
                "refused: {} is a symlink",
                path.display()
            );
            Ok(meta.is_dir())
        }
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The entries of `dir`, sorted. Missing or not a directory: none. A symlink entry is an error.
fn children(dir: &Path) -> Result<Vec<PathBuf>> {
    if !real_dir(dir)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        ensure!(
            !fs::symlink_metadata(&path)?.is_symlink(),
            "refused: {} is a symlink",
            path.display()
        );
        out.push(path);
    }
    out.sort();
    Ok(out)
}

/// `path` relative to the workspace, for display and for the record of what a root is.
fn relative(ws: &Path, path: &Path) -> String {
    path.strip_prefix(ws).unwrap_or(path).display().to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::os::unix::fs::symlink;
    use std::time::{Duration, UNIX_EPOCH};

    /// Fixture clock, literal seconds since the epoch. Every use and mtime below is relative to it.
    const NOW: u64 = 1_800_000_000;
    /// Day length and the default cutoff (14 days), written out rather than taken from the code under test.
    const DAY: u64 = 86_400;
    const CUTOFF: u64 = 14 * DAY;

    // Unit names; the hash ends each name. The record rows say when a recorded last use is; a unit without a row is judged by its fingerprint mtime.
    const STALE: &str = "atl-stale-0123456789abcdef"; // recorded 30 days ago: stale
    const JUST: &str = "atl-just-bbbbbbbbbbbbbbbb"; // recorded cutoff + 1 s ago: stale
    const EDGE: &str = "atl-edge-cccccccccccccccc"; // recorded exactly the cutoff ago: kept
    const LIVE: &str = "atl-live-dddddddddddddddd"; // recorded 1 day ago: kept
    const OLD: &str = "atl-old-eeeeeeeeeeeeeeee"; // superseded generation, recorded 20 days ago: stale
    const NEW: &str = "atl-old-ffffffffffffffff"; // its newer generation, recorded 2 days ago: kept
    const NEVER: &str = "atl-never-1111111111111111"; // no row, mtime 30 days ago: stale
    const FRESH: &str = "atl-fresh-7777777777777777"; // no row, mtime 1 day ago: kept
    const EXAMPLE: &str = "atl-ex-2222222222222222"; // examples/ file + uplifted link, recorded 30 days ago: stale
    const NOHASH: &str = "not-a-unit"; // fingerprint entry that is no unit: kept

    // Apparent bytes the fixture writes, summed by hand.
    const STALE_BYTES: u64 = 10 + 300 + 1000 + 200 + 50;
    const JUST_BYTES: u64 = 5 + 300;
    const OLD_BYTES: u64 = 6 + 600;
    const NEVER_BYTES: u64 = 8 + 800;
    const EXAMPLE_BYTES: u64 = 9 + 900;
    const ORPHAN_BYTES: u64 = 700 + 400;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-prune-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Writes `len` bytes at `path`, parents included; returns `len`.
    fn sized(path: &Path, len: usize) -> u64 {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, vec![b'x'; len]).expect("file");
        len as u64
    }

    /// Sets a directory's mtime to `secs` since the epoch.
    fn set_mtime(path: &Path, secs: u64) {
        let when = UNIX_EPOCH + Duration::from_secs(secs);
        fs::File::open(path)
            .expect("open dir")
            .set_times(fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    /// `cargo/release` of slot 1 with one unit per name above, two orphan `deps/` files, and no record. Every fingerprint dir's mtime is 30 days old, except FRESH's, a day old.
    fn tree_fixture(ws: &Path) -> PathBuf {
        let tree = ws.join("out/slot-1/cargo/release");
        let fp = tree.join(FINGERPRINT);
        let deps = tree.join(DEPS);
        sized(&fp.join(STALE).join("lib-atl_stale"), 10);
        sized(&tree.join(BUILD).join(STALE).join("output"), 300);
        sized(&deps.join("libatl_stale-0123456789abcdef.rlib"), 1000);
        sized(&deps.join("libatl_stale-0123456789abcdef.rmeta"), 200);
        sized(&deps.join("atl_stale-0123456789abcdef.d"), 50);
        sized(&fp.join(JUST).join("lib-atl_just"), 5);
        sized(&deps.join("libatl_just-bbbbbbbbbbbbbbbb.rlib"), 300);
        sized(&fp.join(EDGE).join("lib-atl_edge"), 3);
        sized(&deps.join("libatl_edge-cccccccccccccccc.rlib"), 400);
        sized(&fp.join(LIVE).join("lib-atl_live"), 4);
        sized(&deps.join("libatl_live-dddddddddddddddd.rlib"), 500);
        sized(&fp.join(OLD).join("lib-atl_old"), 6);
        sized(&deps.join("libatl_old-eeeeeeeeeeeeeeee.rlib"), 600);
        sized(&fp.join(NEW).join("lib-atl_old"), 7);
        sized(&deps.join("libatl_old-ffffffffffffffff.rlib"), 700);
        sized(&fp.join(NEVER).join("lib-atl_never"), 8);
        sized(&deps.join("libatl_never-1111111111111111.rlib"), 800);
        sized(&fp.join(FRESH).join("lib-atl_fresh"), 12);
        sized(&fp.join(EXAMPLE).join("lib-atl_ex"), 9);
        let example = tree.join(EXAMPLES).join("ex-2222222222222222");
        sized(&example, 900);
        fs::hard_link(&example, tree.join("ex")).expect("uplift");
        sized(&fp.join(NOHASH).join("lib-x"), 11);
        sized(&deps.join("libatl_gone-9999999999999999.rlib"), 700);
        sized(&deps.join("unrelated.rlib"), 400);
        for name in [STALE, JUST, EDGE, LIVE, OLD, NEW, NEVER, EXAMPLE, NOHASH] {
            set_mtime(&fp.join(name), NOW - 30 * DAY);
        }
        set_mtime(&fp.join(FRESH), NOW - DAY);
        tree
    }

    /// A record for `tree` holding (unit name, last use) pairs, keyed by each name's hash.
    fn write_record(tree: &Path, uses: &[(&str, u64)]) {
        let units: BTreeMap<String, u64> = uses
            .iter()
            .map(|(name, at)| (name.rsplit_once('-').expect("hash").1.to_owned(), *at))
            .collect();
        fs::create_dir_all(tree).expect("tree");
        let text = json!({"units": units}).to_string();
        fs::write(tree.join(".atl-unit-use.json"), text).expect("record");
    }

    /// The uses of the fixture that a record holds; NEVER and FRESH have no row.
    fn recent_uses() -> Vec<(&'static str, u64)> {
        vec![
            (STALE, NOW - 30 * DAY),
            (JUST, NOW - CUTOFF - 1),
            (EDGE, NOW - CUTOFF),
            (LIVE, NOW - DAY),
            (OLD, NOW - 20 * DAY),
            (NEW, NOW - 2 * DAY),
            (EXAMPLE, NOW - 30 * DAY),
        ]
    }

    /// Tries before a slot lock released a moment ago reads as free (the fork race in build_pool.rs tests): FREE_TRIES x FREE_POLL.
    const FREE_TRIES: u32 = 100;
    const FREE_POLL: Duration = Duration::from_millis(10);

    /// `prune_slot` again once the lock is free. A `Busy` answer while the previous hold drains is retried, never accepted as the answer.
    fn prune_once_free(ws: &Path, n: u32, cutoff: u64, now: u64, execute: bool) -> Outcome {
        for _ in 0..FREE_TRIES {
            match prune_slot(ws, n, cutoff, now, execute).expect("prune") {
                Outcome::Busy => std::thread::sleep(FREE_POLL),
                done => return done,
            }
        }
        panic!("slot {n} stayed busy after {FREE_TRIES} tries");
    }

    fn only_plan(outcome: Outcome) -> Vec<TreePlan> {
        match outcome {
            Outcome::Done(plans) => plans,
            _ => panic!("expected a plan"),
        }
    }

    fn stale_names(plan: &TreePlan) -> Vec<String> {
        let mut names: Vec<String> = plan.stale.iter().map(|u| file_name(&u.paths[0])).collect();
        names.sort();
        names
    }

    #[test]
    fn stale_units_are_planned_by_recorded_use_or_mtime() {
        let ws = scratch("plan");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(plan.tree, "out/slot-1/cargo/release");
        assert_eq!(plan.root, "out/slot-1/cargo");
        assert_eq!(stale_names(&plan), [EXAMPLE, JUST, NEVER, OLD, STALE]);
        assert_eq!(plan.units, 10, "every fingerprint entry");
        assert_eq!(plan.kept, 4, "EDGE, LIVE, NEW, FRESH");
        assert_eq!(plan.unrecorded, 2, "NEVER and FRESH: judged by mtime");
        assert_eq!(plan.unattributed, 1, "NOHASH");
        assert_eq!(plan.orphans, 2, "libatl_gone and unrelated");
        assert_eq!(plan.orphan_bytes, ORPHAN_BYTES);
        assert_eq!(
            plan.stale_bytes(),
            STALE_BYTES + JUST_BYTES + OLD_BYTES + NEVER_BYTES + EXAMPLE_BYTES
        );
        let by_name = |name: &str| {
            plan.stale
                .iter()
                .find(|u| file_name(&u.paths[0]) == name)
                .expect(name)
        };
        assert_eq!(
            by_name(STALE).paths.len(),
            5,
            "fingerprint, build, rlib, rmeta, d"
        );
        assert_eq!(
            by_name(EXAMPLE).paths.len(),
            3,
            "fingerprint, examples file, uplifted link"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn execute_removes_stale_units_outputs_and_their_record_entries() {
        let ws = scratch("execute");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let dry = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("dry run"));
        assert_eq!(dry[0].stale.len(), 5);
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "dry run removed"
        );
        only_plan(prune_once_free(&ws, 1, CUTOFF, NOW, true));
        let fp = tree.join(FINGERPRINT);
        let deps = tree.join(DEPS);
        for gone in [
            fp.join(STALE),
            fp.join(JUST),
            fp.join(OLD),
            fp.join(NEVER),
            fp.join(EXAMPLE),
            tree.join(BUILD).join(STALE),
            deps.join("libatl_stale-0123456789abcdef.rlib"),
            deps.join("libatl_stale-0123456789abcdef.rmeta"),
            deps.join("atl_stale-0123456789abcdef.d"),
            deps.join("libatl_just-bbbbbbbbbbbbbbbb.rlib"),
            deps.join("libatl_old-eeeeeeeeeeeeeeee.rlib"),
            tree.join(EXAMPLES).join("ex-2222222222222222"),
            tree.join("ex"),
        ] {
            assert!(!gone.exists(), "{} survived", gone.display());
        }
        for kept in [
            fp.join(EDGE),
            fp.join(LIVE),
            fp.join(NEW),
            fp.join(FRESH),
            fp.join(NOHASH),
            deps.join("libatl_edge-cccccccccccccccc.rlib"),
            deps.join("libatl_live-dddddddddddddddd.rlib"),
            deps.join("libatl_old-ffffffffffffffff.rlib"),
            deps.join("libatl_gone-9999999999999999.rlib"),
            deps.join("unrelated.rlib"),
        ] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        let record: Value = serde_json::from_str(
            &fs::read_to_string(tree.join(".atl-unit-use.json")).expect("record"),
        )
        .expect("record is JSON");
        let left: Vec<&str> = record["units"]
            .as_object()
            .expect("units")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            left,
            ["cccccccccccccccc", "dddddddddddddddd", "ffffffffffffffff"]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A reused unit is a use: cargo reports it `fresh: true` without a rebuild, so its last rebuild can be long ago. Break target: a build that drops fresh messages fails here.
    #[test]
    fn a_unit_cargo_reused_is_kept_though_its_last_rebuild_is_old() {
        let ws = scratch("reused");
        let tree = tree_fixture(&ws);
        write_record(&tree, &[]);
        let reused = json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/atl-stale#0.1.0",
            "filenames": [tree.join("deps/libatl_stale-0123456789abcdef.rlib").display().to_string()],
            "executable": null, "fresh": true});
        unit_use::record(&[reused], NOW).expect("record the reuse");
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        let stale = stale_names(&plan);
        assert!(
            !stale.contains(&STALE.to_owned()),
            "reused unit planned for removal: {stale:?}"
        );
        assert!(
            stale.contains(&JUST.to_owned()),
            "control: JUST has no row and an old mtime"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// With no row a unit is judged by its mtime alone: the record's age does not enter. Expected values: 9 hashed units, FRESH (mtime a day old) the only one kept.
    #[test]
    fn units_without_a_row_are_judged_by_mtime_whatever_the_record_age() {
        let ws = scratch("mtime");
        let tree = tree_fixture(&ws);
        write_record(&tree, &[]);
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(
            stale_names(&plan),
            [EDGE, EXAMPLE, JUST, LIVE, NEVER, OLD, NEW, STALE]
        );
        assert_eq!(plan.kept, 1, "FRESH");
        assert_eq!(plan.unrecorded, 9, "every hashed unit");
        assert_eq!(plan.unattributed, 1, "NOHASH");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn no_record_file_judges_every_unit_by_mtime_too() {
        let ws = scratch("no-record");
        tree_fixture(&ws);
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(plan.kept, 1, "FRESH");
        assert_eq!(plan.unrecorded, 9);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A dry run writes nothing: no `.cargo-lock` appears in a tree that has none. Break target: a dry run that takes the lock with create fails here.
    #[test]
    fn dry_run_creates_no_cargo_lock() {
        let ws = scratch("dry-lock");
        let tree = tree_fixture(&ws);
        only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("dry run"));
        assert!(
            !tree.join(CARGO_LOCK).exists(),
            "dry run created a lock file"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn busy_slot_is_skipped_and_untouched() {
        let ws = scratch("busy");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let build = build_pool::hold_slot(&ws, 1, || {}).expect("build holds slot 1");
        assert!(matches!(
            prune_slot(&ws, 1, CUTOFF, NOW, true).expect("prune"),
            Outcome::Busy
        ));
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "busy slot pruned"
        );
        drop(build);
        assert!(matches!(
            prune_once_free(&ws, 1, CUTOFF, NOW, false),
            Outcome::Done(_)
        ));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A cargo build that does not go through xtask holds only its tree's `.cargo-lock`: that tree is reported busy and left alone, while the slot is pruned as usual.
    #[test]
    fn a_tree_whose_cargo_lock_is_held_is_busy_and_untouched() {
        let ws = scratch("tree-busy");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let build = fs::File::create(tree.join(CARGO_LOCK)).expect("cargo lock");
        flock(&build, FlockOperation::NonBlockingLockExclusive).expect("hold");
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, true).expect("prune")).remove(0);
        assert!(plan.busy);
        assert!(plan.stale.is_empty() && plan.sessions.is_empty());
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "busy tree pruned"
        );
        drop(build);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn unreadable_record_refuses_its_slot_and_removes_nothing() {
        let ws = scratch("garbled");
        let tree = tree_fixture(&ws);
        fs::write(tree.join(".atl-unit-use.json"), "{garbled").expect("record");
        let err = prune_slot(&ws, 1, CUTOFF, NOW, true).expect_err("garbled record must refuse");
        assert!(
            format!("{err:#}").contains("unreadable use record"),
            "{err:#}"
        );
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "pruned under a refused record"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn slot_without_an_out_dir_is_not_built() {
        let ws = scratch("not-built");
        assert!(matches!(
            prune_slot(&ws, 2, CUTOFF, NOW, true).expect("prune"),
            Outcome::NotBuilt
        ));
    }

    #[test]
    fn symlinked_slot_is_refused_and_its_target_untouched() {
        let ws = scratch("slot-link");
        let elsewhere = ws.join("elsewhere");
        tree_fixture(&elsewhere);
        fs::create_dir_all(ws.join("out")).expect("out");
        symlink(elsewhere.join("out/slot-1"), ws.join("out/slot-3")).expect("link");
        let err = prune_slot(&ws, 3, CUTOFF, NOW, true).expect_err("symlink refused");
        assert!(format!("{err:#}").contains("refused"), "{err:#}");
        assert!(
            elsewhere
                .join("out/slot-1/cargo/release/.fingerprint")
                .join(STALE)
                .exists(),
            "the link target was pruned"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn symlinked_unit_directory_is_refused() {
        let ws = scratch("unit-link");
        let tree = tree_fixture(&ws);
        let outside = ws.join("outside-fp");
        sized(&outside.join(STALE).join("lib-x"), 3);
        let fp = tree.join(FINGERPRINT);
        fs::rename(&fp, ws.join("fp-moved")).expect("move");
        symlink(&outside, &fp).expect("link");
        let err = prune_slot(&ws, 1, CUTOFF, NOW, true).expect_err("symlink refused");
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(
            outside.join(STALE).join("lib-x").exists(),
            "link target pruned"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The trees under the roots: profiles and triple profiles only. `gl/other` and `cargo/cxxbridge` hold no root or no tree.
    #[test]
    fn trees_are_profiles_and_triple_profiles_under_the_roots_only() {
        let ws = scratch("trees");
        let slot = ws.join("out/slot-1");
        sized(&slot.join("cargo/release/.fingerprint/x/y"), 1);
        sized(
            &slot.join("cargo/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        sized(&slot.join("cargo-converter/release/.fingerprint/x/y"), 1);
        sized(&slot.join("target/debug/.fingerprint/x/y"), 1);
        sized(
            &slot.join("gl/cargo-undefined-gl/release/.fingerprint/x/y"),
            1,
        );
        sized(&slot.join("gl/other/debug/.fingerprint/x/y"), 1);
        sized(&slot.join("cargo/cxxbridge/somecrate/src/lib.rs"), 1);
        sized(&slot.join("cargo/tmp/file"), 1);
        let mut found: Vec<String> = trees_under(&slot_roots(&slot).expect("roots"))
            .expect("trees")
            .iter()
            .map(|f| relative(&ws, &f.dir))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "out/slot-1/cargo-converter/release",
                "out/slot-1/cargo/release",
                "out/slot-1/cargo/x86_64-unknown-linux-gnu/release",
                "out/slot-1/gl/cargo-undefined-gl/release",
                "out/slot-1/target/debug",
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The root names of a slot: the three target names xtask gives cargo, and the `cargo*` probes under `gl/`.
    #[test]
    fn slot_roots_are_the_target_names_and_the_gl_cargo_probes() {
        let ws = scratch("roots");
        let slot = ws.join("out/slot-2");
        fs::create_dir_all(slot.join("gl/cargo")).expect("gl");
        fs::create_dir_all(slot.join("gl/cargo-undefined-gl")).expect("gl");
        fs::create_dir_all(slot.join("gl/other")).expect("gl");
        let mut names: Vec<String> = slot_roots(&slot)
            .expect("roots")
            .iter()
            .map(|p| relative(&slot, p))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "cargo",
                "cargo-converter",
                "gl/cargo",
                "gl/cargo-undefined-gl",
                "target"
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The trees no slot owns: the main checkout's default target, and each EGL driver checkout's `cargo*` roots only.
    #[test]
    fn unslotted_roots_are_repo_target_and_the_egl_driver_cargo_probes() {
        let ws = scratch("unslotted");
        sized(&ws.join("repo/target/debug/.fingerprint/x/y"), 1);
        sized(
            &ws.join("repo/target/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        sized(&ws.join("repo/target/bp2ninja/bin/tool"), 1);
        sized(
            &ws.join("out/egl-driver/chk/cargo/release/.fingerprint/x/y"),
            1,
        );
        sized(
            &ws.join("out/egl-driver/chk/other/release/.fingerprint/x/y"),
            1,
        );
        sized(
            &ws.join("out/egl-driver/chk2/cargo-probe/debug/.fingerprint/x/y"),
            1,
        );
        let mut found: Vec<String> = trees_under(&unslotted_roots(&ws).expect("roots"))
            .expect("trees")
            .iter()
            .map(|f| relative(&ws, &f.dir))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "out/egl-driver/chk/cargo/release",
                "out/egl-driver/chk2/cargo-probe/debug",
                "repo/target/debug",
                "repo/target/x86_64-unknown-linux-gnu/release",
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Two crate dirs. `atl_x`: finalized `t` 1 (not newest), 2 (lock held), 9 (newest); `-working` 4 (lock free), 5 (lock held); orphan lock 7 (free), 8 (held); `s-x` matches no rustc shape. `atl_y`: a `-working` session younger than the grace (its `t` is far in the future).
    fn incremental_fixture(tree: &Path) -> PathBuf {
        let x = tree.join("incremental/atl_x-0123456789abcdef");
        for (name, len) in [
            ("s-1-a-svh1", 100usize),
            ("s-2-b-svh2", 200),
            ("s-9-z-svh9", 900),
            ("s-4-d-working", 40),
            ("s-5-e-working", 50),
            ("s-x", 7),
        ] {
            sized(&x.join(name).join("query-cache.bin"), len);
        }
        for lock in [
            "s-1-a.lock",
            "s-2-b.lock",
            "s-9-z.lock",
            "s-4-d.lock",
            "s-5-e.lock",
            "s-7-g.lock",
            "s-8-h.lock",
        ] {
            fs::write(x.join(lock), "").expect("lock");
        }
        sized(
            &tree.join("incremental/atl_y-0123456789abcdef/s-zzzzzzzzzz-y-working/query-cache.bin"),
            30,
        );
        x
    }

    /// The fixture's held locks: `s-2-b`, `s-5-e` and orphan `s-8-h`. The returned files keep the flocks while the test runs.
    fn hold_incremental_locks(x: &Path) -> Vec<File> {
        ["s-2-b.lock", "s-5-e.lock", "s-8-h.lock"]
            .iter()
            .map(|lock| {
                let file = File::open(x.join(lock)).expect("open lock");
                flock(&file, FlockOperation::NonBlockingLockExclusive).expect("hold");
                file
            })
            .collect()
    }

    /// Stale: `s-1-a` (finalized, not newest) and `-working` `s-4-d` (lock free, older than the grace), with their locks, and orphan lock `s-7-g` (free, old). Kept: the newest `s-9-z`, held `s-2-b` and `s-5-e`, the malformed `s-x`, held orphan `s-8-h`, and crate `atl_y`'s young `-working` session. Bytes: 100 + 40.
    #[test]
    fn incremental_keeps_the_newest_finalized_session_and_every_held_or_young_one() {
        let ws = scratch("incremental");
        let tree = ws.join("out/slot-1/cargo/debug");
        fs::create_dir_all(tree.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&tree);
        let _held = hold_incremental_locks(&x);
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        let mut dirs: Vec<String> = plan
            .sessions
            .iter()
            .filter_map(|s| s.dir.as_ref().map(|d| file_name(d)))
            .collect();
        dirs.sort();
        assert_eq!(dirs, ["s-1-a-svh1", "s-4-d-working"]);
        let mut locks: Vec<String> = plan
            .sessions
            .iter()
            .filter_map(|s| s.lock.as_ref().map(|l| file_name(l)))
            .collect();
        locks.sort();
        assert_eq!(locks, ["s-1-a.lock", "s-4-d.lock", "s-7-g.lock"]);
        assert_eq!(plan.sessions.len(), 3, "two sessions and one orphan lock");
        assert_eq!(plan.session_bytes(), 100 + 40);
        assert_eq!(
            plan.kept_sessions, 6,
            "s-2-b, s-5-e, s-9-z, s-x, orphan s-8-h, and atl_y's young working session"
        );
        assert!(x.join("s-1-a-svh1").exists(), "dry run removed");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn incremental_removal_takes_the_stale_sessions_and_their_locks_only() {
        let ws = scratch("incremental-exec");
        let tree = ws.join("out/slot-1/cargo/debug");
        fs::create_dir_all(tree.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&tree);
        let _held = hold_incremental_locks(&x);
        only_plan(prune_once_free(&ws, 1, CUTOFF, NOW, true));
        for gone in [
            "s-1-a-svh1",
            "s-1-a.lock",
            "s-4-d-working",
            "s-4-d.lock",
            "s-7-g.lock",
        ] {
            assert!(!x.join(gone).exists(), "{gone} survived");
        }
        for kept in [
            "s-2-b-svh2",
            "s-2-b.lock",
            "s-5-e-working",
            "s-5-e.lock",
            "s-8-h.lock",
            "s-9-z-svh9",
            "s-9-z.lock",
            "s-x",
        ] {
            assert!(x.join(kept).exists(), "{kept} was removed");
        }
        assert!(
            tree.join("incremental/atl_y-0123456789abcdef/s-zzzzzzzzzz-y-working")
                .exists(),
            "a young working session was removed"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// `session_time` and `lock_time` read only the shapes rustc writes: base-36 `t` in the second dash field.
    #[test]
    fn session_names_parse_only_in_rustcs_shapes() {
        assert_eq!(session_time("s-9-z-svh9"), Some(9));
        assert_eq!(
            session_time("s-zzzzzzzzzz-y-working"),
            Some(3_656_158_440_062_975)
        );
        assert_eq!(session_time("s-x"), None);
        assert_eq!(session_time("s-1-a"), None, "three fields: a lock stem");
        assert_eq!(lock_time("s-7-g.lock"), Some(7));
        assert_eq!(lock_time("s-7-g-svh.lock"), None);
        assert_eq!(lock_name("s-9-z-svh9"), "s-9-z.lock");
        assert_eq!(lock_name("s-4-d-working"), "s-4-d.lock");
    }

    #[test]
    fn slot_must_be_a_pool_dir() {
        let ws = Path::new("/ws");
        assert_eq!(pool_slot(ws, &ws.join("out/slot-2")), Some(2));
        assert_eq!(pool_slot(ws, &ws.join("out/slot-0")), None);
        // any number: the pool size is checked against the config elsewhere
        assert_eq!(pool_slot(ws, &ws.join("out/slot-9")), Some(9));
        assert_eq!(pool_slot(ws, &ws.join("out/slot-01")), None);
        assert_eq!(pool_slot(ws, &ws.join("out/slot-x")), None);
        assert_eq!(pool_slot(ws, &ws.join("out/native")), None);
        assert_eq!(pool_slot(ws, &ws.join("out/slot-1/cargo")), None);
        assert_eq!(pool_slot(ws, Path::new("/elsewhere/out/slot-1")), None);
    }

    #[test]
    fn staleness_is_strictly_older_than_the_cutoff() {
        assert!(
            !is_stale(NOW - CUTOFF, NOW, CUTOFF),
            "exactly the cutoff is kept"
        );
        assert!(is_stale(NOW - CUTOFF - 1, NOW, CUTOFF));
        assert!(!is_stale(NOW + DAY, NOW, CUTOFF), "a future use is kept");
        assert!(
            !is_stale(1, u64::MAX, u64::MAX),
            "an overflowing sum is kept"
        );
    }
}
