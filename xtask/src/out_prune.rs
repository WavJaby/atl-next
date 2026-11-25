// SPDX-License-Identifier: MIT
//! `cargo xtask out prune --units`: deletes the cargo units of the target trees that no recorded build used for `--older-than` days, and the incremental sessions no build will load.
//! Trees (a profile dir holding `.fingerprint`, or `<triple>/<profile>`) under these roots: each build dir's `cargo`, `cargo-converter`, `cargo-converter-test`, `cargo-converter-lint`, `target` (what a checkout's `target/` links to), `gl/cargo*`, `vk-driver/cargo*` and `egl-driver/cargo*`; with the unowned trees also a real `repo/target`, which no build dir owns.
//! Use is the record each recorded build writes (`unit_use`): a unit's last reported build, fresh or rebuilt. A unit with no entry is judged by its fingerprint dir's mtime, which cargo moves when it rebuilds the unit: approximate, and the worst case is one rebuild.
//! A unit is `.fingerprint/<pkg>-<hash>/`, `build/<pkg>-<hash>/` (build-script output), the `deps/` and `examples/` files `*-<hash>` or `*-<hash>.<ext>`, and the uplifted hard links (`<tree>/<name>`) of those files; `<hash>` is 16 lowercase hex digits.
//! Incremental (`<tree>/incremental/<crate>/`): keeps the newest finalized session, the one rustc loads (`rustc_incremental/src/persist/fs.rs`, `find_source_directory_in_iter`, rustc 1.99.1; rustc reads an older one only when copying the newest fails). Removed: every other finalized session, `-working` sessions older than 10 s (rustc's own grace), and lock files whose session is gone. A held lock is never touched.
//! Dead incremental dirs go whole (units scope only, not on a tree without its needed record): rustc names a session by its start time and cargo touches `invoked.timestamp` as it starts rustc, so a dir is owned when a kept unit of its crate name was invoked within [`INCREMENTAL_MATCH_SECS`] of the dir's newest finalized session; dead = not owned and that session older than the cutoff. Whatever cannot be judged (no or unparsable session, an owner without `invoked.timestamp`, a held lock, a refused unit) is kept and counted: a wrong deletion costs one compile without reuse, a kept dir only space.
//! Kept, never deleted: entries that are no unit, `deps/` and `examples/` files no unit owns (counted as orphans). A tree whose record cannot be read refuses its build dir.
//! Busy: a build holds the dir lock (`build_pool::hold_dir`); prune takes it exclusive without waiting and skips the dir. Every tree also has cargo's own build-dir lock (`<tree>/.cargo-lock`), taken without waiting and held from plan to removal: a tree a build holds is skipped and reported.
//! Symlinks are never followed. One inside a unit (cxx links a crate's sources in its build dir) is content: unlinked with it. One that is a unit's dir skips that unit (named, counted); one on the way to a unit (a root, a tree, `.fingerprint`, `build`, `deps`) refuses the call. Removal resolves the parent with `openat2` from the workspace, so a link swapped in after the plan is refused too ([`remove`]).
//! Land ([`prune_land`]): before Publish, ws/land with window [`LAND_PRUNE_DAYS`], inside the dir's namespace, so the base it becomes holds what recent builds read; a tree with no use record keeps every unit there. Published bases are never pruned: they are the lowers of live overlays.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode as RawMode, OFlags, ResolveFlags};

use crate::build_pool;
use crate::flock::{Flock, Mode};
use crate::out;
use crate::unit_use::{self, BUILD, DEPS, EXAMPLES, FINGERPRINT};

/// Default `--older-than`, in days: a unit no cargo build used for this long is stale. First guess, not measured. A shorter cutoff frees more but recompiles stable dependencies more often; a rebuilt dependency also recompiles its dependents (2026-11-08: pruning one unit recompiled it and its dependent crate). Re-fit from the rebuild counts after a few lands.
pub const PRUNE_AFTER_DAYS: u64 = 14;
/// Land's window, in days: before Publish, ws/land drops the units no recorded build used for this long, so the base holds what main's recent builds read. Basis (base 9689dcda `target/debug`, 3289 units, base-cache-bloat study 2026-11-25): 2 days cuts 1073 units, 1 day 1525, 14 days 0; 2 keeps a unit a land skipped once (a branch that does not build it). A wrong cut costs one rebuild of the unit and its dependents in the next land.
pub const LAND_PRUNE_DAYS: u64 = 2;

const SECS_PER_DAY: u64 = 86_400;
const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;
/// Target roots directly under a build dir: xtask's `--target-dir` names, and `target`, what a checkout's `target/` links to (out.rs). Cargo units key on the checkout path, so `out::refresh` keeps these from the checkout's own dir.
pub const DIR_ROOTS: [&str; 7] = [
    "cargo",
    crate::packages::TARGET_DIR,
    crate::packages::TEST_TARGET_DIR,
    crate::packages::LINT_TARGET_DIR,
    crate::packages::FAULT_TARGET_DIR,
    "target",
    crate::out::BOOTSTRAP_DIR,
];
/// `<build dir>/gl/cargo*`: the GL probes' target roots.
const GL_DIR: &str = "gl";
/// Prefix of the root names under `gl/` and the driver probe dirs.
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
const MICROS_PER_SEC: u64 = 1_000_000;
/// Widest gap, in seconds, between a unit's `invoked.timestamp` and its incremental dir's newest session that still counts as the same compile. Base afc5228c `target/debug` (2023 dirs, 2-day window): dead at 5 / 30 / 60 / 300 / 3600 s = 802 / 791 / 777 / 770 / 764 dirs (12.09 / 11.77 / 11.63 / 11.52 / 11.51 GiB); every inode-linked pair (233) is within 5 s, so 60 costs 14 dirs (0.14 GiB) over 30 for margin.
pub const INCREMENTAL_MATCH_SECS: u64 = 60;

/// What one build dir's prune found: no dir, a build holds it, or one plan per target tree.
#[derive(Debug)]
pub enum Outcome {
    NotBuilt,
    Busy,
    Done(Vec<TreePlan>),
}

/// One target tree: its units and incremental sessions, and what the stale ones take. `busy`: a cargo build holds the tree; nothing is planned.
#[derive(Debug, Default)]
pub struct TreePlan {
    /// The root it sits under, relative to the workspace, e.g. `out/pool/ws/repo/cargo`.
    pub root: String,
    /// The tree relative to the workspace, e.g. `out/pool/ws/repo/cargo/release`.
    pub tree: String,
    pub dir: PathBuf,
    pub busy: bool,
    /// The tree has no use record and its scope needs one: no unit planned.
    pub no_record: bool,
    /// Fingerprint entries in the tree.
    pub units: usize,
    pub stale: Vec<Unit>,
    pub kept: usize,
    /// Units refused and kept whole: why, one line each. In neither `kept` nor `stale`.
    pub skipped: Vec<String>,
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
    /// Incremental crate dirs no kept unit owns, removed whole.
    pub dead_incremental: Vec<Incremental>,
    /// Incremental crate dirs that could not be judged: kept.
    pub incr_unknown: usize,
}

impl TreePlan {
    fn stale_bytes(&self) -> u64 {
        self.stale.iter().map(|u| u.bytes).sum()
    }

    fn incremental_bytes(&self) -> u64 {
        self.dead_incremental.iter().map(|d| d.bytes).sum()
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
    _held: Option<Flock>,
}

/// A dead incremental crate dir, its apparent bytes, and the flocks of every lock file in it, held from plan to removal.
#[derive(Debug)]
pub struct Incremental {
    pub dir: PathBuf,
    pub bytes: u64,
    _held: Vec<Flock>,
}

/// The kept units' claim on incremental dirs: per crate name, each owner's `invoked.timestamp` in seconds (None: missing). `refused`: a unit was skipped as a link, its crate names unread, so no dir is judged dead.
#[derive(Debug, Default)]
struct Owners {
    crates: HashMap<String, Vec<Option<u64>>>,
    refused: bool,
}

impl Owners {
    fn add(&mut self, fingerprint_dir: &Path) -> Result<()> {
        let invoked = invoked_secs(fingerprint_dir)?;
        for name in unit_use::crate_names_of(fingerprint_dir)? {
            self.crates.entry(name).or_default().push(invoked);
        }
        Ok(())
    }
}

/// Totals of one sweep: stale units, dead incremental dirs, stale sessions and their bytes, the refused units, and the build dirs or trees that failed.
#[derive(Debug, Default)]
pub struct Sweep {
    pub units: usize,
    pub incremental: usize,
    pub sessions: usize,
    pub bytes: u64,
    pub skipped: usize,
    pub failed: usize,
}

impl Sweep {
    /// The summary lines' ending: nothing when no unit was refused.
    fn skipped_note(&self) -> String {
        match self.skipped {
            0 => String::new(),
            n => format!("; skipped {n} refused units"),
        }
    }
}

/// A target tree found under a root.
struct Found {
    root: PathBuf,
    dir: PathBuf,
}

/// Prunes each build dir of `ids`, then with `unowned` the trees no build dir owns. Prints one line per target tree, one per root and a summary. Busy and failed scopes are reported; a failure is counted in `failed`, not returned.
pub fn sweep(
    ws: &Path,
    ids: &[String],
    unowned: bool,
    older_than: u64,
    execute: bool,
) -> Result<Sweep> {
    let cutoff = older_than
        .checked_mul(SECS_PER_DAY)
        .with_context(|| format!("--older-than {older_than} days is too large"))?;
    let now = unit_use::now()?;
    let mut total = Sweep::default();
    let mut roots: BTreeMap<String, Sweep> = BTreeMap::new();
    for id in ids {
        match prune_dir(ws, id, cutoff, now, execute) {
            Ok(Outcome::NotBuilt) => println!("ws/{id}: not built"),
            Ok(Outcome::Busy) => println!("ws/{id}: busy, skipped: a build holds its lock"),
            Ok(Outcome::Done(plans)) if plans.is_empty() => println!("ws/{id}: no cargo trees"),
            Ok(Outcome::Done(plans)) => report(&plans, &mut total, &mut roots),
            Err(e) => {
                total.failed += 1;
                println!("ws/{id}: FAILED: {e:#}");
            }
        }
    }
    if unowned {
        match prune_unowned(ws, cutoff, now, execute) {
            Ok(plans) if plans.is_empty() => println!("unowned: no cargo trees"),
            Ok(plans) => report(&plans, &mut total, &mut roots),
            Err(e) => {
                total.failed += 1;
                println!("unowned: FAILED: {e:#}");
            }
        }
    }
    for (root, sum) in &roots {
        println!(
            "root {root}: units {}, incremental dirs {}, sessions {}, {}",
            sum.units,
            sum.incremental,
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
        "unit prune: {verb} {} stale units, {} dead incremental dirs and {} stale sessions, {}{}",
        total.units,
        total.incremental,
        total.sessions,
        human(total.bytes),
        total.skipped_note()
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
        if plan.no_record {
            println!("{}: no use record: units not pruned", plan.tree);
        }
        println!(
            "{}: units {}, stale {} ({}), kept {} (unrecorded {}), unattributed {}, orphan deps {} ({}), incremental dead {} ({}), unknown {}, sessions stale {} ({}), kept {}",
            plan.tree,
            plan.units,
            plan.stale.len(),
            human(plan.stale_bytes()),
            plan.kept,
            plan.unrecorded,
            plan.unattributed,
            plan.orphans,
            human(plan.orphan_bytes),
            plan.dead_incremental.len(),
            human(plan.incremental_bytes()),
            plan.incr_unknown,
            plan.sessions.len(),
            human(plan.session_bytes()),
            plan.kept_sessions
        );
        for why in &plan.skipped {
            println!("{}: unit skipped: {why}", plan.tree);
        }
        let bytes = plan.stale_bytes() + plan.incremental_bytes() + plan.session_bytes();
        let sum = roots.entry(plan.root.clone()).or_default();
        for target in [&mut *total, sum] {
            target.units += plan.stale.len();
            target.incremental += plan.dead_incremental.len();
            target.sessions += plan.sessions.len();
            target.bytes += bytes;
            target.skipped += plan.skipped.len();
        }
    }
}

/// What a prune removes: units and sessions (`out prune --units`), or sessions only (the end of a build, `prune_sessions`).
#[derive(Clone, Copy, Debug)]
enum Scope {
    /// units last used `cutoff` seconds or more ago, and the stale sessions. `need_record`: a tree without a use record plans no unit (land: mtime is no evidence of use there).
    Everything { cutoff: u64, need_record: bool },
    /// stale sessions only: units keep their `--older-than` rule
    SessionsOnly,
}

/// Plans build dir `id`'s prune and, with `execute`, removes what it names. Holds the dir's lock for the whole call.
pub fn prune_dir(ws: &Path, id: &str, cutoff: u64, now: u64, execute: bool) -> Result<Outcome> {
    let scope = Scope::Everything {
        cutoff,
        need_record: false,
    };
    prune_dir_in(ws, id, scope, now, execute)
}

/// Land's prune of ws/land before Publish ([`LAND_PRUNE_DAYS`]): run inside the dir's namespace when it is a ram delta (outside, its mount point is empty and every tree would read as absent). A tree with no use record plans nothing; an unreadable one refuses the dir. Prints the tree lines; returns the summary line.
/// `inside`: this process runs in ws/land's delta namespace (`delta::stage`).
pub fn prune_land(ws: &Path, now: u64, execute: bool, inside: bool) -> Result<String> {
    let id = out::LAND_ID;
    out::unmounted_ram_at(&out::dir_of(ws, id), id, inside)?;
    let window = LAND_PRUNE_DAYS;
    let scope = Scope::Everything {
        cutoff: window * SECS_PER_DAY,
        need_record: true,
    };
    let plans = match prune_dir_in(ws, id, scope, now, execute)? {
        Outcome::NotBuilt => {
            return Ok(format!(
                "land unit prune: ws/{id}: not built, nothing pruned"
            ));
        }
        Outcome::Busy => {
            return Ok(format!(
                "land unit prune: ws/{id}: busy, nothing pruned: a process holds it"
            ));
        }
        Outcome::Done(plans) => plans,
    };
    let mut total = Sweep::default();
    report(&plans, &mut total, &mut BTreeMap::new());
    let kept: usize = plans.iter().map(|p| p.kept).sum();
    let no_record = plans.iter().filter(|p| p.no_record).count();
    let busy = plans.iter().filter(|p| p.busy).count();
    let verb = if execute {
        "removed"
    } else {
        "dry run, would remove"
    };
    Ok(format!(
        "land unit prune: ws/{id}: {verb} {} units, {} incremental dirs and {} sessions, {}; kept {kept} units; window {window} d; {} trees ({no_record} without a use record, {busy} busy: not pruned){}",
        total.units,
        total.incremental,
        total.sessions,
        human(total.bytes),
        plans.len(),
        total.skipped_note()
    ))
}

/// The end of a build in build dir `id` (`out::after_build`): removes its stale incremental sessions, never units. Busy while any process holds the dir, so the caller drops its own shared hold first.
pub fn prune_sessions(ws: &Path, id: &str, now: u64) -> Result<Outcome> {
    prune_dir_in(ws, id, Scope::SessionsOnly, now, true)
}

fn prune_dir_in(ws: &Path, id: &str, scope: Scope, now: u64, execute: bool) -> Result<Outcome> {
    let dir = out::dir_of(ws, id);
    let Some(real) = checked_dir(&dir)? else {
        return Ok(Outcome::NotBuilt);
    };
    let Some(_lock) = build_pool::try_lock_dir(ws, id)? else {
        return Ok(Outcome::Busy);
    };
    let found = trees_under(&dir_roots(&real)?)?;
    Ok(Outcome::Done(prune_trees(ws, &found, scope, now, execute)?))
}

/// The trees no build dir owns: a real `repo/target` (a main checkout not yet on its build dir). Each is busy-checked by its own `.cargo-lock`.
pub fn prune_unowned(ws: &Path, cutoff: u64, now: u64, execute: bool) -> Result<Vec<TreePlan>> {
    let found = trees_under(&unowned_roots(ws)?)?;
    let scope = Scope::Everything {
        cutoff,
        need_record: false,
    };
    prune_trees(ws, &found, scope, now, execute)
}

/// The one line the end of a build prints for `what` (a build dir): sessions removed and kept, their bytes, busy trees.
pub fn sessions_line(what: &str, plans: &[TreePlan]) -> String {
    let removed: usize = plans.iter().map(|p| p.sessions.len()).sum();
    let bytes: u64 = plans.iter().map(TreePlan::session_bytes).sum();
    let kept: usize = plans.iter().map(|p| p.kept_sessions).sum();
    let busy = plans.iter().filter(|p| p.busy).count();
    let busy = if busy == 0 {
        String::new()
    } else {
        format!(", {busy} tree(s) busy, skipped")
    };
    format!(
        "session prune: {what}: removed {removed} stale session entries ({}), kept {kept}{busy}",
        human(bytes)
    )
}

/// The dir to prune for build dir `dir`: itself when a real dir; None when absent. Anything else is refused: a symlink is never followed.
fn checked_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    ensure!(
        meta.is_dir(),
        "refused: {} is not a real dir (a symlink is never followed)",
        dir.display()
    );
    Ok(Some(dir.to_owned()))
}

/// The roots of build dir `dir`: [`DIR_ROOTS`], `gl/cargo*` and each driver probe family's `cargo*` (`out::FAMILIES`).
fn dir_roots(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut roots: Vec<PathBuf> = DIR_ROOTS.iter().map(|name| dir.join(name)).collect();
    for sub in std::iter::once(GL_DIR).chain(out::FAMILIES) {
        roots.extend(prefixed(&dir.join(sub), CARGO_ROOT)?);
    }
    Ok(roots)
}

/// The roots no build dir owns: the main checkout's `target` while it is a real dir (once linked it is its build dir's).
fn unowned_roots(ws: &Path) -> Result<Vec<PathBuf>> {
    let main = ws.join("repo").join("target");
    let mut roots = Vec::new();
    if fs::symlink_metadata(&main).is_ok_and(|m| m.is_dir()) {
        roots.push(main);
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
    scope: Scope,
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
            plans.push(plan_tree(&f.dir, root, tree, now, scope)?);
        }
    }
    if execute {
        for plan in &plans {
            if !plan.busy {
                remove_plan(ws, plan)?;
            }
        }
    }
    drop(locks);
    Ok(plans)
}

/// Removes what `plan` names: stale units (their record entries go after), then stale sessions.
fn remove_plan(ws: &Path, plan: &TreePlan) -> Result<()> {
    for unit in &plan.stale {
        for path in &unit.paths {
            remove(ws, &plan.dir, path)?;
        }
    }
    let hashes: Vec<String> = plan.stale.iter().map(|u| u.hash.clone()).collect();
    if !hashes.is_empty() {
        unit_use::forget(&plan.dir, &hashes)?;
    }
    // the dir's lock files go with it; their flocks are held until the plan drops
    for dead in &plan.dead_incremental {
        remove(ws, &plan.dir, &dead.dir)?;
    }
    for session in &plan.sessions {
        remove_session(ws, &plan.dir, session)?;
    }
    Ok(())
}

/// cargo's build-dir lock of `tree`, taken without waiting: a build holds it for its whole run. None when one does.
fn hold_tree(tree: &Path) -> Result<Option<Flock>> {
    let path = tree.join(CARGO_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    Flock::try_take(file, Mode::Exclusive).with_context(|| format!("flock {}", path.display()))
}

/// The plan for one tree (its `.cargo-lock` held by the caller): its stale sessions in both scopes, its units under `Everything` only.
fn plan_tree(tree: &Path, root: String, label: String, now: u64, scope: Scope) -> Result<TreePlan> {
    let mut plan = TreePlan {
        root,
        tree: label,
        dir: tree.to_owned(),
        ..TreePlan::default()
    };
    if let Scope::Everything {
        cutoff,
        need_record,
    } = scope
        && let Some(owners) = plan_units(tree, now, cutoff, need_record, &mut plan)?
    {
        plan_incremental(tree, now, cutoff, &owners, &mut plan)?;
    }
    let dead: Vec<PathBuf> = plan
        .dead_incremental
        .iter()
        .map(|d| d.dir.clone())
        .collect();
    let (sessions, kept) = stale_sessions(tree, now, &dead)?;
    plan.sessions = sessions;
    plan.kept_sessions = kept;
    Ok(plan)
}

/// Plans the units of `tree` into `plan`. Units are the fingerprint dirs; `build/`, `deps/`, `examples/` files are attributed by hash, uplifted links by inode. A unit's last use is its record entry; without one, its fingerprint dir's mtime. Stale when that is older than the cutoff.
/// Returns the units that stay (kept or skipped) as incremental owners; None when no unit was planned (no record where one is needed).
fn plan_units(
    tree: &Path,
    now: u64,
    cutoff: u64,
    need_record: bool,
    plan: &mut TreePlan,
) -> Result<Option<Owners>> {
    let record = unit_use::load(tree)?;
    if need_record && record.is_none() {
        plan.no_record = true;
        return Ok(None);
    }
    let mut owners = Owners::default();
    let mut units: Vec<(PathBuf, String)> = Vec::new();
    for dir in entries(&tree.join(FINGERPRINT))? {
        plan.units += 1;
        let Some(hash) = unit_use::unit_hash(&file_name(&dir)).map(str::to_owned) else {
            plan.unattributed += 1;
            continue;
        };
        match link_refusal(&dir)? {
            Some(why) => {
                // kept, but its crate names sit behind the link: never read
                owners.refused = true;
                plan.skipped.push(why);
            }
            None => units.push((dir, hash)),
        }
    }
    let owned: BTreeSet<String> = units.iter().map(|(_, hash)| hash.clone()).collect();
    let mut outputs: Vec<(PathBuf, Option<String>)> = Vec::new();
    for sub in [DEPS, EXAMPLES] {
        for file in entries(&tree.join(sub))? {
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
    for file in entries(tree)? {
        let meta =
            fs::symlink_metadata(&file).with_context(|| format!("reading {}", file.display()))?;
        if !meta.is_file() {
            continue;
        }
        if let Some(owner) = owner_of_inode.get(&(meta.dev(), meta.ino())) {
            uplifted.entry(owner.clone()).or_default().push(file);
        }
    }
    let build_dir = tree.join(BUILD);
    // refuses the tree when `build` itself is a link: every unit's build dir would be reached through it
    real_dir(&build_dir)?;
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
            owners.add(&dir)?;
            continue;
        }
        let mut paths = vec![dir.clone()];
        let build = build_dir.join(file_name(&dir));
        if exists(&build)? {
            if let Some(why) = link_refusal(&build)? {
                plan.skipped.push(why);
                owners.add(&dir)?;
                continue;
            }
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
    Ok(Some(owners))
}

/// The incremental crate dirs of `tree` no unit in `owners` owns, into `plan.dead_incremental`; the ones that cannot be judged into `plan.incr_unknown` (kept). Rule: module doc. Every lock file of a dead dir is taken and held by the plan.
fn plan_incremental(
    tree: &Path,
    now: u64,
    cutoff: u64,
    owners: &Owners,
    plan: &mut TreePlan,
) -> Result<()> {
    for crate_dir in children(&tree.join(INCREMENTAL))? {
        if !real_dir(&crate_dir)? {
            continue;
        }
        let name = file_name(&crate_dir);
        let Some((crate_name, _)) = name.rsplit_once('-') else {
            plan.incr_unknown += 1;
            continue;
        };
        let (dirs, locks) = session_entries(&crate_dir)?;
        let parsed = dirs.iter().all(|d| session_time(&file_name(d)).is_some());
        let newest = newest_finalized(&dirs).and_then(|d| session_time(&file_name(d)));
        let Some(t) = newest
            .filter(|_| parsed)
            .map(|micros| micros / MICROS_PER_SEC)
        else {
            plan.incr_unknown += 1;
            continue;
        };
        if !is_stale(t, now, cutoff) {
            continue;
        }
        let claims = owners
            .crates
            .get(crate_name)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if claims.iter().any(Option::is_none) {
            plan.incr_unknown += 1;
            continue;
        }
        if claims
            .iter()
            .flatten()
            .any(|&invoked| invoked.abs_diff(t) <= INCREMENTAL_MATCH_SECS)
        {
            continue;
        }
        if owners.refused {
            plan.incr_unknown += 1;
            continue;
        }
        let mut held = Vec::new();
        let mut busy = false;
        for lock in &locks {
            match take_lock(lock)? {
                Lock::Held => {
                    busy = true;
                    break;
                }
                Lock::Free(flock) => held.extend(flock),
            }
        }
        if busy {
            plan.incr_unknown += 1;
            continue;
        }
        plan.dead_incremental.push(Incremental {
            bytes: unique_bytes(std::slice::from_ref(&crate_dir))?,
            dir: crate_dir,
            _held: held,
        });
    }
    Ok(())
}

/// Unix seconds of a fingerprint dir's `invoked.timestamp`; None when it is missing.
fn invoked_secs(fingerprint_dir: &Path) -> Result<Option<u64>> {
    let path = fingerprint_dir.join(unit_use::INVOKED);
    if !exists(&path)? {
        return Ok(None);
    }
    mtime_secs(&path).map(Some)
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

/// The incremental sessions of `tree` no load reads, and how many entries are kept. Per crate dir: the newest finalized session stays, as rustc loads it; other finalized sessions go; `-working` sessions go once older than the grace; a lock held by a process keeps its session; lock files whose session is gone go once older than the grace. Session names that match no rustc shape are kept. The crate dirs in `dead` are skipped: their sessions go with the dir.
fn stale_sessions(tree: &Path, now: u64, dead: &[PathBuf]) -> Result<(Vec<Session>, usize)> {
    let now_micros = now.saturating_mul(MICROS_PER_SEC);
    let mut stale = Vec::new();
    let mut kept = 0;
    for crate_dir in children(&tree.join(INCREMENTAL))? {
        if !real_dir(&crate_dir)? || dead.contains(&crate_dir) {
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
    Free(Option<Flock>),
}

/// Takes `lock`'s exclusive flock without waiting. The caller keeps the returned lock while it acts on the lock's state, so no process takes it in between.
fn take_lock(lock: &Path) -> Result<Lock> {
    let file = match OpenOptions::new().read(true).open(lock) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Lock::Free(None)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", lock.display())),
    };
    Ok(
        match Flock::try_take(file, Mode::Exclusive)
            .with_context(|| format!("flock {}", lock.display()))?
        {
            Some(held) => Lock::Free(Some(held)),
            None => Lock::Held,
        },
    )
}

/// Whether a process holds `lock` (a dry run's busy probe): the flock is taken and dropped at once.
fn lock_held(lock: &Path) -> Result<bool> {
    Ok(matches!(take_lock(lock)?, Lock::Held))
}

/// Removes a stale session: a held lock leaves it alone; otherwise the dir, then its lock file.
fn remove_session(ws: &Path, tree: &Path, session: &Session) -> Result<()> {
    // no re-probe: the plan holds the lock since it was planned (Session `_held`)
    if let Some(dir) = &session.dir {
        remove(ws, tree, dir)?;
    }
    if let Some(lock) = &session.lock
        && exists(lock)?
    {
        remove(ws, tree, lock)?;
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
        for child in entries(path)? {
            add_bytes(&child, seen, total)?;
        }
    } else if seen.insert((meta.dev(), meta.ino())) {
        *total += meta.len();
    }
    Ok(())
}

/// Removes a path under `tree` (under workspace `ws`). Refused outside them or when any dir between `ws` and `path` is a symlink; `path` itself a symlink is removed as a link.
/// The parent is resolved once, by the kernel (`openat2`), and the removal starts from that fd: no link swapped in later redirects it. Below it `fs::remove_dir_all` opens each dir without following and unlinks links.
fn remove(ws: &Path, tree: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(tree),
        "refused: {} is outside {}",
        path.display(),
        tree.display()
    );
    let inside = path
        .strip_prefix(ws)
        .with_context(|| format!("refused: {} is outside {}", path.display(), ws.display()))?;
    let (Some(parent), Some(name)) = (inside.parent(), inside.file_name()) else {
        bail!("refused: {} names no entry", path.display());
    };
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let anchor = rustix::fs::open(ws, flags, RawMode::empty())
        .with_context(|| format!("opening {}", ws.display()))?;
    let held = rustix::fs::openat2(
        &anchor,
        parent,
        flags,
        RawMode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )
    .with_context(|| {
        format!(
            "refused: the dir of {} does not resolve without a symlink",
            path.display()
        )
    })?;
    let at = Path::new("/proc/self/fd")
        .join(held.as_raw_fd().to_string())
        .join(name);
    let meta = fs::symlink_metadata(&at).with_context(|| format!("reading {}", path.display()))?;
    let removed = if meta.is_dir() {
        fs::remove_dir_all(&at)
    } else {
        fs::remove_file(&at)
    };
    removed.with_context(|| format!("removing {}", path.display()))
}

/// Why a unit whose dir is `path` is skipped: the dir is a symlink. None: it is not.
fn link_refusal(path: &Path) -> Result<Option<String>> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(meta
        .is_symlink()
        .then(|| format!("refused: {} is a symlink", path.display())))
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

/// The entries of `dir`, sorted. Missing or not a directory: none. A symlink entry is an error: for dirs whose entries are walked into (roots, trees, sessions).
fn children(dir: &Path) -> Result<Vec<PathBuf>> {
    let out = entries(dir)?;
    for path in &out {
        if let Some(why) = link_refusal(path)? {
            bail!(why);
        }
    }
    Ok(out)
}

/// The entries of `dir`, sorted, symlinks included. Missing or not a directory: none. `dir` itself a symlink: an error.
fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    if !real_dir(dir)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        out.push(
            entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path(),
        );
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

pub(crate) fn human(bytes: u64) -> String {
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

    /// `cargo/release` of build dir `a` with one unit per name above, two orphan `deps/` files, and no record. Every fingerprint dir's mtime is 30 days old, except FRESH's, a day old.
    fn tree_fixture(ws: &Path) -> PathBuf {
        let tree = ws.join("out/pool/ws/a/cargo/release");
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

    /// Tries before a dir lock released a moment ago reads as free (the fork race in build_pool.rs tests): FREE_TRIES x FREE_POLL.
    const FREE_TRIES: u32 = 100;
    const FREE_POLL: Duration = Duration::from_millis(10);

    /// `prune_dir` again once the lock is free. A `Busy` answer while the previous hold drains is retried, never accepted as the answer.
    fn prune_once_free(ws: &Path, id: &str, cutoff: u64, now: u64, execute: bool) -> Outcome {
        for _ in 0..FREE_TRIES {
            match prune_dir(ws, id, cutoff, now, execute).expect("prune") {
                Outcome::Busy => std::thread::sleep(FREE_POLL),
                done => return done,
            }
        }
        panic!("ws/{id} stayed busy after {FREE_TRIES} tries");
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
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(plan.tree, "out/pool/ws/a/cargo/release");
        assert_eq!(plan.root, "out/pool/ws/a/cargo");
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
        let dry = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("dry run"));
        assert_eq!(dry[0].stale.len(), 5);
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "dry run removed"
        );
        only_plan(prune_once_free(&ws, "a", CUTOFF, NOW, true));
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
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("prune")).remove(0);
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
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("prune")).remove(0);
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
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(plan.kept, 1, "FRESH");
        assert_eq!(plan.unrecorded, 9);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A dry run writes nothing: no `.cargo-lock` appears in a tree that has none. Break target: a dry run that takes the lock with create fails here.
    #[test]
    fn dry_run_creates_no_cargo_lock() {
        let ws = scratch("dry-lock");
        let tree = tree_fixture(&ws);
        only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("dry run"));
        assert!(
            !tree.join(CARGO_LOCK).exists(),
            "dry run created a lock file"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn busy_dir_is_skipped_and_untouched() {
        let ws = scratch("busy");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let build = build_pool::hold_dir(&ws, "a", || {}).expect("build holds dir a");
        assert!(matches!(
            prune_dir(&ws, "a", CUTOFF, NOW, true).expect("prune"),
            Outcome::Busy
        ));
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "busy dir pruned"
        );
        drop(build);
        assert!(matches!(
            prune_once_free(&ws, "a", CUTOFF, NOW, false),
            Outcome::Done(_)
        ));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A cargo build that does not go through xtask holds only its tree's `.cargo-lock`: that tree is reported busy and left alone, while the build dir is pruned as usual.
    #[test]
    fn a_tree_whose_cargo_lock_is_held_is_busy_and_untouched() {
        let ws = scratch("tree-busy");
        let tree = tree_fixture(&ws);
        write_record(&tree, &recent_uses());
        let build = fs::File::create(tree.join(CARGO_LOCK)).expect("cargo lock");
        let build = Flock::try_take(build, Mode::Exclusive)
            .expect("flock")
            .expect("hold");
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, true).expect("prune")).remove(0);
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
    fn unreadable_record_refuses_its_build_dir_and_removes_nothing() {
        let ws = scratch("garbled");
        let tree = tree_fixture(&ws);
        fs::write(tree.join(".atl-unit-use.json"), "{garbled").expect("record");
        let err = prune_dir(&ws, "a", CUTOFF, NOW, true).expect_err("garbled record must refuse");
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
    fn a_missing_build_dir_is_not_built() {
        let ws = scratch("not-built");
        assert!(matches!(
            prune_dir(&ws, "b", CUTOFF, NOW, true).expect("prune"),
            Outcome::NotBuilt
        ));
    }

    #[test]
    fn symlinked_build_dir_is_refused_and_its_target_untouched() {
        let ws = scratch("dir-link");
        let elsewhere = ws.join("elsewhere");
        tree_fixture(&elsewhere);
        fs::create_dir_all(ws.join("out/pool/ws")).expect("pool");
        symlink(elsewhere.join("out/pool/ws/a"), ws.join("out/pool/ws/c")).expect("link");
        let err = prune_dir(&ws, "c", CUTOFF, NOW, true).expect_err("symlink refused");
        assert!(format!("{err:#}").contains("refused"), "{err:#}");
        assert!(
            elsewhere
                .join("out/pool/ws/a/cargo/release/.fingerprint")
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
        let err = prune_dir(&ws, "a", CUTOFF, NOW, true).expect_err("symlink refused");
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
        let dir = ws.join("out/pool/ws/a");
        sized(&dir.join("cargo/release/.fingerprint/x/y"), 1);
        sized(
            &dir.join("cargo/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        sized(&dir.join("cargo-converter/release/.fingerprint/x/y"), 1);
        sized(&dir.join("target/debug/.fingerprint/x/y"), 1);
        sized(
            &dir.join("gl/cargo-undefined-gl/release/.fingerprint/x/y"),
            1,
        );
        sized(&dir.join("gl/other/debug/.fingerprint/x/y"), 1);
        sized(&dir.join("cargo/cxxbridge/somecrate/src/lib.rs"), 1);
        sized(&dir.join("cargo/tmp/file"), 1);
        let mut found: Vec<String> = trees_under(&dir_roots(&dir).expect("roots"))
            .expect("trees")
            .iter()
            .map(|f| relative(&ws, &f.dir))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "out/pool/ws/a/cargo-converter/release",
                "out/pool/ws/a/cargo/release",
                "out/pool/ws/a/cargo/x86_64-unknown-linux-gnu/release",
                "out/pool/ws/a/gl/cargo-undefined-gl/release",
                "out/pool/ws/a/target/debug",
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The root names of a build dir: the target names xtask gives cargo (build, converter build, its faulted build, test, lint and the bootstrap gate), the checkout's `target`, and the `cargo*` probes under `gl/` and the driver probe dirs.
    #[test]
    fn dir_roots_are_the_target_names_and_the_cargo_probes() {
        let ws = scratch("roots");
        let dir = ws.join("out/pool/ws/b");
        fs::create_dir_all(dir.join("gl/cargo")).expect("gl");
        fs::create_dir_all(dir.join("gl/cargo-undefined-gl")).expect("gl");
        fs::create_dir_all(dir.join("gl/other")).expect("gl");
        fs::create_dir_all(dir.join("vk-driver/cargo")).expect("vk");
        fs::create_dir_all(dir.join("vk-driver/vk")).expect("vk");
        fs::create_dir_all(dir.join("egl-driver/cargo")).expect("egl");
        let mut names: Vec<String> = dir_roots(&dir)
            .expect("roots")
            .iter()
            .map(|p| relative(&dir, p))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "bootstrap",
                "cargo",
                "cargo-converter",
                "cargo-converter-fault",
                "cargo-converter-lint",
                "cargo-converter-test",
                "egl-driver/cargo",
                "gl/cargo",
                "gl/cargo-undefined-gl",
                "target",
                "vk-driver/cargo"
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The trees no build dir owns: the main checkout's real target only; a linked `repo/target` is its build dir's, not listed here.
    #[test]
    fn unowned_roots_are_a_real_repo_target() {
        let ws = scratch("unowned");
        sized(&ws.join("repo/target/debug/.fingerprint/x/y"), 1);
        sized(
            &ws.join("repo/target/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        sized(&ws.join("repo/target/bp2ninja/bin/tool"), 1);
        let mut found: Vec<String> = trees_under(&unowned_roots(&ws).expect("roots"))
            .expect("trees")
            .iter()
            .map(|f| relative(&ws, &f.dir))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "repo/target/debug",
                "repo/target/x86_64-unknown-linux-gnu/release",
            ]
        );
        fs::rename(ws.join("repo/target"), ws.join("linked")).expect("move");
        symlink(ws.join("linked"), ws.join("repo/target")).expect("link");
        assert!(
            unowned_roots(&ws).expect("roots").is_empty(),
            "a linked repo/target was listed"
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
    fn hold_incremental_locks(x: &Path) -> Vec<Flock> {
        ["s-2-b.lock", "s-5-e.lock", "s-8-h.lock"]
            .iter()
            .map(|lock| {
                let file = fs::File::open(x.join(lock)).expect("open lock");
                Flock::try_take(file, Mode::Exclusive)
                    .expect("flock")
                    .expect("hold")
            })
            .collect()
    }

    /// Stale: `s-1-a` (finalized, not newest) and `-working` `s-4-d` (lock free, older than the grace), with their locks, and orphan lock `s-7-g` (free, old). Kept: the newest `s-9-z`, held `s-2-b` and `s-5-e`, the malformed `s-x`, held orphan `s-8-h`, and crate `atl_y`'s young `-working` session. Bytes: 100 + 40.
    #[test]
    fn incremental_keeps_the_newest_finalized_session_and_every_held_or_young_one() {
        let ws = scratch("incremental");
        let tree = ws.join("out/pool/ws/a/cargo/debug");
        fs::create_dir_all(tree.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&tree);
        let _held = hold_incremental_locks(&x);
        let plan = only_plan(prune_dir(&ws, "a", CUTOFF, NOW, false).expect("prune")).remove(0);
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
        let tree = ws.join("out/pool/ws/a/cargo/debug");
        fs::create_dir_all(tree.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&tree);
        let _held = hold_incremental_locks(&x);
        only_plan(prune_once_free(&ws, "a", CUTOFF, NOW, true));
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

    /// The session dirs of crate dir `x`, sorted; lock files left out.
    fn session_dirs(x: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(x)
            .expect("crate dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(SESSION) && !name.ends_with(LOCK_EXT))
            .collect();
        names.sort();
        names
    }

    /// `prune_sessions` again once the lock is free; a `Busy` while the previous hold drains is retried, as in `prune_once_free`.
    fn sessions_once_free(ws: &Path, id: &str, now: u64) -> Vec<TreePlan> {
        for _ in 0..FREE_TRIES {
            match prune_sessions(ws, id, now).expect("prune") {
                Outcome::Busy => std::thread::sleep(FREE_POLL),
                done => return only_plan(done),
            }
        }
        panic!("ws/{id} stayed busy after {FREE_TRIES} tries");
    }

    #[test]
    fn session_prune_takes_stale_sessions_and_no_unit() {
        let ws = scratch("sessions-only");
        let release = tree_fixture(&ws);
        write_record(&release, &recent_uses());
        let debug = ws.join("out/pool/ws/a/cargo/debug");
        fs::create_dir_all(debug.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&debug);
        let _held = hold_incremental_locks(&x);
        let plans = only_plan(prune_sessions(&ws, "a", NOW).expect("prune"));
        assert!(
            plans.iter().all(|p| p.stale.is_empty()),
            "a unit was planned"
        );
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
            "s-5-e-working",
            "s-9-z-svh9",
            "s-x",
            "s-8-h.lock",
        ] {
            assert!(x.join(kept).exists(), "{kept} was removed");
        }
        for unit in [STALE, OLD, NEVER] {
            assert!(
                release.join(FINGERPRINT).join(unit).exists(),
                "unit {unit} was removed"
            );
        }
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The caller's own shared hold blocks the prune (why `finish_build` drops it first); once free, the prune runs.
    #[test]
    fn session_prune_skips_a_dir_a_build_holds() {
        let ws = scratch("sessions-busy");
        let debug = ws.join("out/pool/ws/a/cargo/debug");
        fs::create_dir_all(debug.join(FINGERPRINT)).expect("fingerprint");
        let x = incremental_fixture(&debug);
        let build = build_pool::hold_dir(&ws, "a", || {}).expect("build holds dir a");
        assert!(matches!(
            prune_sessions(&ws, "a", NOW).expect("prune"),
            Outcome::Busy
        ));
        assert!(x.join("s-1-a-svh1").exists(), "a busy dir was pruned");
        drop(build);
        sessions_once_free(&ws, "a", NOW);
        assert!(
            !x.join("s-1-a-svh1").exists(),
            "stale session survived once the dir was free"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn session_prune_leaves_the_other_dirs_alone() {
        let ws = scratch("sessions-other-dir");
        let one = ws.join("out/pool/ws/a/cargo/debug");
        let two = ws.join("out/pool/ws/b/cargo/debug");
        fs::create_dir_all(one.join(FINGERPRINT)).expect("fingerprint");
        fs::create_dir_all(two.join(FINGERPRINT)).expect("fingerprint");
        let x1 = incremental_fixture(&one);
        let x2 = incremental_fixture(&two);
        sessions_once_free(&ws, "a", NOW);
        assert!(
            !x1.join("s-1-a-svh1").exists(),
            "dir a's stale session survived"
        );
        assert!(x2.join("s-1-a-svh1").exists(), "dir b's session was pruned");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Two finalized sessions in one crate dir, as a crate's last two builds leave them: the call takes the older one and keeps the newest.
    #[test]
    fn after_a_build_only_the_newest_session_is_left() {
        let ws = scratch("sessions-steady");
        let debug = ws.join("out/pool/ws/a/cargo/debug");
        fs::create_dir_all(debug.join(FINGERPRINT)).expect("fingerprint");
        let x = debug.join("incremental/atl_x-0123456789abcdef");
        sized(&x.join("s-1-a-svh1/query-cache.bin"), 100);
        sized(&x.join("s-2-b-svh2/query-cache.bin"), 200);
        fs::write(x.join("s-1-a.lock"), "").expect("lock");
        fs::write(x.join("s-2-b.lock"), "").expect("lock");
        let plans = sessions_once_free(&ws, "a", NOW);
        assert_eq!(session_dirs(&x), ["s-2-b-svh2"]);
        assert_eq!(plans.iter().map(TreePlan::session_bytes).sum::<u64>(), 100);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn sessions_line_names_what_was_removed() {
        let plans = [TreePlan {
            sessions: vec![Session {
                dir: None,
                lock: None,
                bytes: 2048,
                _held: None,
            }],
            kept_sessions: 3,
            ..TreePlan::default()
        }];
        let line = sessions_line("ws/b", &plans);
        assert!(
            line.starts_with("session prune: ws/b: removed 1 stale session entries ("),
            "{line}"
        );
        assert!(line.ends_with("), kept 3"), "{line}");
    }

    /// Land's window, written out: 2 days.
    const LAND_WINDOW: u64 = 2 * DAY;

    /// `tree_fixture` as ws/land's `cargo/release`.
    fn land_fixture(ws: &Path) -> PathBuf {
        tree_fixture(ws);
        let pool = ws.join("out/pool/ws");
        fs::rename(pool.join("a"), pool.join("land")).expect("as ws/land");
        pool.join("land/cargo/release")
    }

    /// Land's record: LIVE used an hour ago, EDGE exactly the window ago, NEW a day ago (kept); JUST a day past the window, the rest weeks ago (stale). NEVER (no row, mtime 30 days) stale, FRESH (no row, mtime 1 day) kept.
    fn land_uses() -> Vec<(&'static str, u64)> {
        vec![
            (STALE, NOW - 30 * DAY),
            (JUST, NOW - LAND_WINDOW - DAY),
            (EDGE, NOW - LAND_WINDOW),
            (LIVE, NOW - 3600),
            (OLD, NOW - 20 * DAY),
            (NEW, NOW - DAY),
            (EXAMPLE, NOW - 30 * DAY),
        ]
    }

    fn fingerprints(tree: &Path) -> Vec<String> {
        let mut names: Vec<String> = children(&tree.join(FINGERPRINT))
            .expect("fingerprints")
            .iter()
            .map(|p| file_name(p))
            .collect();
        names.sort();
        names
    }

    /// What land keeps of `land_fixture` + `land_uses`, sorted.
    const LAND_KEPT: [&str; 5] = [EDGE, FRESH, LIVE, NEW, NOHASH];

    /// Break target: a window of 0 days removes LIVE (used an hour ago); the 14-day default keeps JUST.
    #[test]
    fn land_keeps_a_unit_used_today_and_drops_the_ones_past_its_window() {
        let ws = scratch("land-window");
        let tree = land_fixture(&ws);
        write_record(&tree, &land_uses());
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert_eq!(fingerprints(&tree), LAND_KEPT);
        assert!(
            line.starts_with(
                "land unit prune: ws/land: removed 5 units, 0 incremental dirs and 0 sessions, "
            ),
            "{line}"
        );
        assert!(
            line.ends_with(
                "kept 4 units; window 2 d; 1 trees (0 without a use record, 0 busy: not pruned)"
            ),
            "{line}"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// No record = no evidence of use: land prunes none of the tree's units and says so, though most are 30 days old by mtime. Break target: judging by mtime there (the manual prune's rule) removes them.
    #[test]
    fn land_without_a_use_record_prunes_nothing() {
        let ws = scratch("land-no-record");
        let tree = land_fixture(&ws);
        let before = fingerprints(&tree);
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert_eq!(fingerprints(&tree), before);
        assert!(line.contains("removed 0 units"), "{line}");
        assert!(line.contains("(1 without a use record"), "{line}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A ram ws/land seen outside its namespace is its empty mount point: refused, never reported as "no cargo trees". Break target: without the guard this returns Ok with 0 trees.
    #[test]
    fn land_on_ram_outside_its_namespace_is_refused() {
        let ws = scratch("land-ram");
        let dir = out::dir_of(&ws, out::LAND_ID);
        fs::create_dir_all(&dir).expect("mount point");
        fs::write(dir.join(crate::delta::RECORD), "").expect("delta record");
        let err = prune_land(&ws, NOW, true, false).expect_err("outside the namespace");
        assert!(format!("{err:#}").contains("ram delta"), "{err:#}");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The unit dir whose out dir cxx filled, as land-q114 met it.
    const CXX: &str = "atl-audio-bridge-1dab79eb66ef12f7";

    /// `land_fixture` + `land_uses` with one more stale unit, [`CXX`]: its build dir holds a link to a dir and one to a file, both outside the tree. Returns (tree, linked dir, linked file).
    fn cxx_fixture(ws: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let release = land_fixture(ws);
        let tree = release
            .parent()
            .and_then(Path::parent)
            .expect("ws/land")
            .join("cargo-converter/release");
        fs::create_dir_all(tree.parent().expect("root")).expect("root dir");
        fs::rename(&release, &tree).expect("as cargo-converter");
        let mut uses = land_uses();
        uses.push((CXX, NOW - 30 * DAY));
        write_record(&tree, &uses);
        sized(
            &tree.join(FINGERPRINT).join(CXX).join("run-build-script"),
            5,
        );
        let sources = ws.join("wt/land/crates/atl-audio-bridge");
        sized(&sources.join("src/lib.rs"), 7);
        let header = ws.join("wt/land/cxx.h");
        sized(&header, 9);
        let bridge = tree.join(BUILD).join(CXX).join("out/cxxbridge");
        sized(&bridge.join("sources/lib.rs.cc"), 20);
        fs::create_dir_all(bridge.join("crate")).expect("crate dir");
        fs::create_dir_all(bridge.join("include/rust")).expect("include dir");
        symlink(&sources, bridge.join("crate/atl-audio-bridge")).expect("dir link");
        symlink(&header, bridge.join("include/rust/cxx.h")).expect("file link");
        (tree, sources, header)
    }

    /// Links inside a stale unit are content: the land prune completes, the unit goes, the targets stay. Break targets: `add_bytes` over `children` refuses the tree (land-q114); a removal that follows links empties the targets.
    #[test]
    fn land_removes_a_unit_holding_symlinks_and_leaves_their_targets() {
        let ws = scratch("cxx-unit");
        let (tree, sources, header) = cxx_fixture(&ws);
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert!(line.contains("removed 6 units"), "{line}");
        assert!(line.ends_with("busy: not pruned)"), "{line}");
        assert_eq!(fingerprints(&tree), LAND_KEPT);
        assert!(!exists(&tree.join(BUILD).join(CXX)).expect("build dir"));
        assert!(sources.join("src/lib.rs").is_file(), "dir target emptied");
        assert!(header.is_file(), "file target removed");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A unit whose fingerprint or build dir is a link is skipped whole, named and counted; the other stale units go. Break target: without `link_refusal` the links are planned as units and unlinked.
    #[test]
    fn a_unit_dir_that_is_a_symlink_is_skipped_and_named() {
        let ws = scratch("unit-dir-link");
        let (tree, _, _) = cxx_fixture(&ws);
        let outside = ws.join("outside");
        sized(&outside.join("keep"), 3);
        let fp = tree.join(FINGERPRINT).join(OLD);
        let build = tree.join(BUILD).join(STALE);
        for dir in [&fp, &build] {
            fs::remove_dir_all(dir).expect("real dir");
            symlink(&outside, dir).expect("link");
        }
        let dry =
            only_plan(prune_dir(&ws, out::LAND_ID, LAND_WINDOW, NOW, false).expect("dry run"));
        assert_eq!(
            dry[0].skipped,
            [&fp, &build].map(|p| format!("refused: {} is a symlink", p.display()))
        );
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert!(line.contains("removed 4 units"), "{line}");
        assert!(line.ends_with("; skipped 2 refused units"), "{line}");
        assert!(tree.join(FINGERPRINT).join(STALE).is_dir(), "skipped unit");
        assert!(
            tree.join(DEPS)
                .join("libatl_old-eeeeeeeeeeeeeeee.rlib")
                .is_file(),
            "a skipped unit keeps its outputs"
        );
        for link in [&fp, &build] {
            assert!(link.is_symlink(), "{} was removed", link.display());
        }
        assert!(outside.join("keep").is_file(), "link target pruned");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// A link on the way to the units refuses the call and removes nothing: the tree, its root, or `build`. Break target: without the `real_dir` check of `build`, or the one in `is_tree`, no refusal.
    #[test]
    fn a_symlink_on_the_way_to_the_units_is_refused() {
        for part in ["", "..", BUILD] {
            let ws = scratch(&format!("path-link-{}", part.len()));
            let (tree, _, _) = cxx_fixture(&ws);
            let dir = match part {
                ".." => tree.parent().expect("root").to_owned(),
                part => ws
                    .join("out/pool/ws/land/cargo-converter/release")
                    .components()
                    .chain(Path::new(part).components())
                    .collect(),
            };
            let outside = ws.join("outside");
            fs::rename(&dir, &outside).expect("move");
            symlink(&outside, &dir).expect("link");
            let err = prune_land(&ws, NOW, true, false).expect_err("link refused");
            let text = format!("{err:#}");
            assert!(
                text.contains(&format!("refused: {} is a symlink", dir.display())),
                "{text}"
            );
            assert!(tree.join(FINGERPRINT).join(STALE).exists(), "pruned");
            assert!(tree.join(BUILD).join(CXX).exists(), "pruned");
            fs::remove_dir_all(&ws).expect("cleanup");
        }
    }

    /// The removal itself resolves no link, whatever the plan saw: a parent swapped for a link after the plan is refused. Break target: `remove` by path deletes the outside dir.
    #[test]
    fn a_parent_swapped_for_a_symlink_after_the_plan_is_refused() {
        let ws = scratch("swap");
        let (tree, _, _) = cxx_fixture(&ws);
        let outside = ws.join("outside");
        fs::rename(tree.join(BUILD), &outside).expect("move");
        symlink(&outside, tree.join(BUILD)).expect("link");
        let err = remove(&ws, &tree, &tree.join(BUILD).join(CXX)).expect_err("swap refused");
        assert!(format!("{err:#}").contains("refused"), "{err:#}");
        assert!(outside.join(CXX).is_dir(), "deleted through the link");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The base published after land's prune holds exactly the kept units. Break target: publishing before the prune carries the 5 stale ones.
    #[test]
    fn publish_after_land_prune_holds_the_kept_set() {
        let ws = scratch("land-publish");
        let tree = land_fixture(&ws);
        write_record(&tree, &land_uses());
        prune_land(&ws, NOW, true, false).expect("land prune");
        let base = ws.join("out/pool/base/c");
        fs::create_dir_all(base.parent().expect("parent")).expect("bases dir");
        out::copy_base(&out::dir_of(&ws, out::LAND_ID), &base).expect("publish copy");
        assert_eq!(fingerprints(&base.join("cargo/release")), LAND_KEPT);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Session start 3 days before NOW, past land's window: NOW - 3 * DAY = 1_799_740_800 s = 1_799_740_800_000_000 micros = base 36 `hpydice0w0`.
    const T_OLD: u64 = NOW - 3 * DAY;
    const OLD_SESSION: &str = "s-hpydice0w0-a-svh1";
    const OLD_LOCK: &str = "s-hpydice0w0-a.lock";
    /// Session start 1 day before NOW, inside the window: 1_799_913_600_000_000 micros = base 36 `hq0kw51q80`.
    const NEW_SESSION: &str = "s-hq0kw51q80-b-svh2";
    /// Match width, written out: 60 s.
    const MATCH: u64 = 60;

    /// Gives fingerprint dir `unit` of `tree` the target file of `crate_name` and, with `invoked`, an `invoked.timestamp` of that mtime.
    fn owner(tree: &Path, unit: &str, crate_name: &str, invoked: Option<u64>) {
        let dir = tree.join(FINGERPRINT).join(unit);
        let fp_mtime = mtime_secs(&dir).expect("fingerprint mtime");
        sized(&dir.join(format!("lib-{crate_name}.json")), 2);
        if let Some(at) = invoked {
            let stamp = dir.join(unit_use::INVOKED);
            fs::write(&stamp, "").expect("invoked");
            set_mtime(&stamp, at);
        }
        // the unit's own mtime is its age when it has no record row: keep it
        set_mtime(&dir, fp_mtime);
    }

    /// Crate dir `incremental/<crate_name>-<id>` of `tree` with one session and its lock file, 100 bytes. Returns the dir.
    fn incr(tree: &Path, crate_name: &str, session: &str) -> PathBuf {
        let dir = tree.join(INCREMENTAL).join(format!("{crate_name}-1abc"));
        sized(&dir.join(session).join("query-cache.bin"), 100);
        fs::write(dir.join(lock_name(session)), "").expect("lock");
        dir
    }

    /// `land_fixture` + `land_uses` (2-day window): STALE stale, LIVE kept (used an hour ago, compiled 30 days ago).
    fn incr_fixture(ws: &Path) -> PathBuf {
        let tree = land_fixture(ws);
        write_record(&tree, &land_uses());
        tree
    }

    fn land_dry(ws: &Path) -> TreePlan {
        only_plan(prune_dir(ws, out::LAND_ID, LAND_WINDOW, NOW, false).expect("dry run")).remove(0)
    }

    fn dead_names(plan: &TreePlan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .dead_incremental
            .iter()
            .map(|d| file_name(&d.dir))
            .collect();
        names.sort();
        names
    }

    /// Gate 1: the crate's only unit is stale and the session is past the window -> the dir goes whole, lock file included, and no session of it is planned twice. Break target: stale units as owners keep it.
    #[test]
    fn incremental_dir_of_a_crate_with_only_stale_units_is_removed() {
        let ws = scratch("incr-dead");
        let tree = incr_fixture(&ws);
        owner(&tree, STALE, "atl_stale", Some(T_OLD));
        let dir = incr(&tree, "atl_stale", OLD_SESSION);
        // an older finalized session (t = 1 micro) without a lock file: the session prune alone would plan it (a lock file would be held by this plan)
        sized(&dir.join("s-1-z-svh0").join("q"), 1);
        let plan = land_dry(&ws);
        assert_eq!(dead_names(&plan), ["atl_stale-1abc"]);
        assert!(
            plan.sessions.is_empty(),
            "its sessions planned twice: {:?}",
            plan.sessions
        );
        assert_eq!(plan.incremental_bytes(), 100 + 1);
        // the dry plan holds the dir's lock: an execute while it lives sees the dir as busy
        drop(plan);
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert!(
            line.contains("removed 5 units, 1 incremental dirs and 0 sessions"),
            "{line}"
        );
        assert!(!exists(&dir).expect("dir"), "dead dir survived");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate 2 + 3: a kept unit invoked within the match width of the session owns the dir, at exactly 60 s too; at 61 s it does not. LIVE was compiled 30 days ago, so age alone would drop it. Break targets: no match test (age only) -> the +0 dir goes; `<` for `<=` -> the +60 dir goes.
    #[test]
    fn incremental_dir_owned_by_a_kept_unit_is_kept_up_to_the_match_width() {
        for (offset, dead) in [(0, false), (MATCH, false), (MATCH + 1, true)] {
            let ws = scratch(&format!("incr-owned-{offset}"));
            let tree = incr_fixture(&ws);
            owner(&tree, LIVE, "atl_live", Some(T_OLD + offset));
            incr(&tree, "atl_live", OLD_SESSION);
            let plan = land_dry(&ws);
            assert_eq!(
                plan.dead_incremental.len(),
                usize::from(dead),
                "invoked at T + {offset} s"
            );
            fs::remove_dir_all(&ws).expect("cleanup");
        }
    }

    /// Gate 4: no owner, but the newest session is inside the window -> kept, not unknown. Break target: dropping the session-age test removes it.
    #[test]
    fn incremental_dir_compiled_inside_the_window_is_kept() {
        let ws = scratch("incr-young");
        let tree = incr_fixture(&ws);
        let dir = incr(&tree, "atl_nobody", NEW_SESSION);
        let plan = land_dry(&ws);
        assert!(plan.dead_incremental.is_empty());
        assert_eq!(plan.incr_unknown, 0);
        prune_land(&ws, NOW, true, false).expect("land prune");
        assert!(dir.join(NEW_SESSION).is_dir(), "young dir removed");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate 5: what cannot be judged is kept and counted, one case at a time (each would be dead otherwise: no owner, or only a stale one). Break targets: each guard in `plan_incremental`.
    #[test]
    fn incremental_dirs_that_cannot_be_judged_are_kept_and_counted() {
        type Setup = fn(&Path) -> Option<Flock>;
        let cases: [(&str, Setup); 5] = [
            ("no finalized session", |tree| {
                let dir = tree.join(INCREMENTAL).join("atl_a-1abc");
                sized(&dir.join("s-hpydice0w0-a-working/q"), 1);
                None
            }),
            ("unparsable session name", |tree| {
                let dir = incr(tree, "atl_b", OLD_SESSION);
                sized(&dir.join("s-x/q"), 1);
                None
            }),
            ("dir name without a dash", |tree| {
                sized(
                    &tree
                        .join(INCREMENTAL)
                        .join("nodash")
                        .join(OLD_SESSION)
                        .join("q"),
                    1,
                );
                None
            }),
            ("held lock", |tree| {
                let dir = incr(tree, "atl_c", OLD_SESSION);
                let file = fs::File::open(dir.join(OLD_LOCK)).expect("lock");
                Flock::try_take(file, Mode::Exclusive).expect("flock")
            }),
            ("owner without invoked.timestamp", |tree| {
                owner(tree, LIVE, "atl_d", None);
                incr(tree, "atl_d", OLD_SESSION);
                None
            }),
        ];
        for (i, (what, setup)) in cases.into_iter().enumerate() {
            let ws = scratch(&format!("incr-unknown-{i}"));
            let tree = incr_fixture(&ws);
            let _held = setup(&tree);
            let plan = land_dry(&ws);
            assert!(plan.dead_incremental.is_empty(), "{what}: planned dead");
            assert_eq!(plan.incr_unknown, 1, "{what}: not counted");
            fs::remove_dir_all(&ws).expect("cleanup");
        }
    }

    /// A unit refused as a link keeps every otherwise dead dir as unknown: its crate names are behind the link.
    #[test]
    fn a_refused_unit_keeps_dead_incremental_dirs_as_unknown() {
        let ws = scratch("incr-refused");
        let tree = incr_fixture(&ws);
        incr(&tree, "atl_nobody", OLD_SESSION);
        let fp = tree.join(FINGERPRINT).join(OLD);
        fs::remove_dir_all(&fp).expect("real dir");
        symlink(ws.join("outside"), &fp).expect("link");
        let plan = land_dry(&ws);
        assert!(plan.dead_incremental.is_empty());
        assert_eq!(plan.incr_unknown, 1);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate 6: a land tree without a use record plans no incremental dir, and the end-of-build session prune removes none. Break target: calling `plan_incremental` in either.
    #[test]
    fn no_record_and_sessions_only_plan_no_incremental_dir() {
        let ws = scratch("incr-no-record");
        let tree = land_fixture(&ws);
        let dir = incr(&tree, "atl_nobody", OLD_SESSION);
        let line = prune_land(&ws, NOW, true, false).expect("land prune");
        assert!(line.contains("0 incremental dirs"), "{line}");
        assert!(dir.join(OLD_SESSION).is_dir(), "removed without a record");
        let plans = only_plan(prune_sessions(&ws, out::LAND_ID, NOW).expect("session prune"));
        assert!(plans.iter().all(|p| p.dead_incremental.is_empty()));
        assert!(
            dir.join(OLD_SESSION).is_dir(),
            "removed by the session prune"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate 7: two units of one crate (OLD stale, NEW kept), both invoked near the session -> kept: the kept one owns it.
    #[test]
    fn a_crate_with_one_kept_unit_keeps_its_dir() {
        let ws = scratch("incr-two");
        let tree = incr_fixture(&ws);
        owner(&tree, OLD, "atl_old", Some(T_OLD));
        owner(&tree, NEW, "atl_old", Some(T_OLD + 1));
        incr(&tree, "atl_old", OLD_SESSION);
        let plan = land_dry(&ws);
        assert!(plan.dead_incremental.is_empty());
        assert_eq!(plan.incr_unknown, 0);
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Gate 8: a link under `incremental/` named like a dead dir refuses the call; its target is not followed or removed.
    #[test]
    fn a_symlinked_incremental_dir_is_refused() {
        let ws = scratch("incr-link");
        let tree = incr_fixture(&ws);
        let outside = ws.join("outside-incr");
        sized(&outside.join(OLD_SESSION).join("q"), 1);
        fs::create_dir_all(tree.join(INCREMENTAL)).expect("incremental");
        symlink(&outside, tree.join(INCREMENTAL).join("atl_stale-1abc")).expect("link");
        let err = prune_land(&ws, NOW, true, false).expect_err("link refused");
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(
            outside.join(OLD_SESSION).join("q").is_file(),
            "link target removed"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// `crate_names_of` reads `<kind>-<name>.json` only, `-` -> `_`; the hash file without extension and unknown kinds name nothing.
    #[test]
    fn crate_names_come_from_the_target_files() {
        let ws = scratch("crate-names");
        let dir = ws.join("fp");
        for f in [
            "lib-atl_x",
            "lib-atl_x.json",
            "test-integration-test-my-test.json",
            "build-script-build-script-build.json",
            "dep-lib-atl_x",
            "weird-thing.json",
            "lib-.json",
        ] {
            sized(&dir.join(f), 1);
        }
        assert_eq!(
            unit_use::crate_names_of(&dir).expect("names"),
            ["atl_x", "build_script_build", "my_test"]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }
}
