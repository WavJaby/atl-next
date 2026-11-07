// SPDX-License-Identifier: MIT
//! `cargo xtask slot prune`: deletes the cargo units of a slot's target trees that were not rebuilt for `--older-than` days.
//! Trees under `<ws>/out/slot-<n>`: `cargo/<profile>`, `cargo/<triple>/<profile>`, `cargo-converter/<profile>`.
//! A unit is `.fingerprint/<pkg>-<hash>/`, `build/<pkg>-<hash>/` (build-script output) and the `deps/` files `*-<hash>` or `*-<hash>.<ext>`; `<hash>` is 16 lowercase hex digits.
//! Age is `invoked.timestamp`. Cargo 1.99.1 rewrites it when it rebuilds a unit and leaves it alone when it reuses one: it is the last rebuild, not the last use. A cutoff therefore also drops stable dependencies that are still in use, and their rebuild recompiles their dependents (see [`PRUNE_AFTER_DAYS`]).
//! Kept, never deleted: units without `invoked.timestamp` (build-script run units), names that are no unit, `deps/` files no unit owns (counted as orphans).
//! Busy: a build holds the slot's lock (`build_pool::hold_slot`); prune takes it exclusive without waiting and skips the slot.
//! Symlinks are refused, never followed: a link under the slot could point the prune outside it.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};

use crate::build_pool;
use crate::slot::{self, Holder, SLOTS};

/// Default `--older-than`, in days: a unit whose `invoked.timestamp` is older is stale. First guess, not measured. A shorter cutoff frees more but recompiles stable dependencies more often; a rebuilt dependency also recompiles its dependents (2026-11-08: pruning one unit recompiled it and its dependent crate). Re-fit from the rebuild counts after a few lands.
pub const PRUNE_AFTER_DAYS: u64 = 14;

const SECS_PER_DAY: u64 = 86_400;
const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;
/// Cargo writes a unit hash as `{:016x}`: 16 lowercase hex digits ending the unit's name.
const HASH_HEX_LEN: usize = 16;
/// Cargo target roots under a slot: `out/cargo` and `out/cargo-converter`.
const TREE_ROOTS: [&str; 2] = ["cargo", "cargo-converter"];
const FINGERPRINT: &str = ".fingerprint";
const BUILD: &str = "build";
const DEPS: &str = "deps";
/// Rewritten by cargo when it rebuilds a unit. Build-script run units have none.
const INVOKED: &str = "invoked.timestamp";

/// What one slot's prune found: no out dir, a build holds it, or one plan per target tree.
#[derive(Debug)]
pub enum Outcome {
    NotBuilt,
    Busy,
    Done(Vec<TreePlan>),
}

/// One target tree: its units, and what its stale units take.
#[derive(Debug, Default)]
pub struct TreePlan {
    /// Path under the slot, e.g. `cargo/release`.
    pub tree: String,
    /// Fingerprint dirs in the tree.
    pub units: usize,
    pub stale: Vec<Unit>,
    pub kept: usize,
    /// Units with no `invoked.timestamp`, or a name that is no unit. Kept.
    pub unattributed: usize,
    /// `deps/` files no unit owns. Kept.
    pub orphans: usize,
    pub orphan_bytes: u64,
}

impl TreePlan {
    fn stale_bytes(&self) -> u64 {
        self.stale.iter().map(|u| u.bytes).sum()
    }
}

/// A stale unit: the paths its removal takes (the fingerprint dir first), and their apparent bytes.
#[derive(Debug)]
pub struct Unit {
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
}

/// `cargo xtask slot prune`. Exactly one of `slot` / `all` names the slots. A dry run unless `execute`.
pub fn run(
    ws: &Path,
    holders: &[Holder],
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
            ensure!((1..=SLOTS).contains(&n), "slot {n} is outside 1..={SLOTS}");
            vec![n]
        }
        None => (1..=SLOTS).collect(),
    };
    let days = older_than
        .checked_mul(SECS_PER_DAY)
        .with_context(|| format!("--older-than {older_than} days is too large"))?;
    let cutoff = Duration::from_secs(days);
    let now = SystemTime::now();
    let (mut units, mut bytes, mut failed) = (0usize, 0u64, false);
    for n in slots {
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
            Ok(Outcome::Done(plans)) => {
                for plan in plans {
                    println!(
                        "slot {n} {}: units {}, stale {} ({}), kept {}, unattributed {}, orphan deps {} ({})",
                        plan.tree,
                        plan.units,
                        plan.stale.len(),
                        human(plan.stale_bytes()),
                        plan.kept,
                        plan.unattributed,
                        plan.orphans,
                        human(plan.orphan_bytes)
                    );
                    units += plan.stale.len();
                    bytes += plan.stale_bytes();
                }
            }
            Err(e) => {
                failed = true;
                println!("slot {n}: FAILED: {e:#}");
            }
        }
    }
    let verb = if execute {
        "removed"
    } else {
        "dry run, would remove (add --execute)"
    };
    println!("slot prune: {verb} {units} stale units, {}", human(bytes));
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Plans slot `n`'s prune and, with `execute`, removes its stale units. Holds the slot's lock for the whole call.
pub fn prune_slot(
    ws: &Path,
    n: u32,
    cutoff: Duration,
    now: SystemTime,
    execute: bool,
) -> Result<Outcome> {
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
    let mut plans = Vec::new();
    for tree in trees(&dir)? {
        plans.push(plan_tree(&dir, &tree, now, cutoff)?);
    }
    if execute {
        for plan in &plans {
            for unit in &plan.stale {
                for path in &unit.paths {
                    remove(&dir, path)?;
                }
            }
        }
    }
    Ok(Outcome::Done(plans))
}

/// The pool number `n` when `dir` is exactly `<ws>/out/slot-<n>` with `n` in the pool.
pub fn pool_slot(ws: &Path, dir: &Path) -> Option<u32> {
    (1..=SLOTS).find(|&n| slot::slot_dir(ws, n) == dir)
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

/// Every target tree under `slot`: a child of a root that holds `.fingerprint`, or a grandchild (`<triple>/<profile>`).
fn trees(slot: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for root in TREE_ROOTS {
        for child in children(&slot.join(root))? {
            if is_tree(&child)? {
                found.push(child);
                continue;
            }
            for grand in children(&child)? {
                if is_tree(&grand)? {
                    found.push(grand);
                }
            }
        }
    }
    Ok(found)
}

fn is_tree(dir: &Path) -> Result<bool> {
    Ok(real_dir(dir)? && real_dir(&dir.join(FINGERPRINT))?)
}

/// The plan for one tree. Units are the fingerprint dirs; `build/` and `deps/` entries are attributed to a unit by its hash.
fn plan_tree(slot: &Path, tree: &Path, now: SystemTime, cutoff: Duration) -> Result<TreePlan> {
    let label = tree
        .strip_prefix(slot)
        .unwrap_or(tree)
        .display()
        .to_string();
    let units = children(&tree.join(FINGERPRINT))?;
    let deps = children(&tree.join(DEPS))?;
    let dep_names: Vec<String> = deps.iter().map(|p| file_name(p)).collect();
    let mut plan = TreePlan {
        tree: label,
        units: units.len(),
        ..TreePlan::default()
    };
    let mut owners = BTreeSet::new();
    for unit in &units {
        let name = file_name(unit);
        let Some(hash) = unit_hash(&name) else {
            plan.unattributed += 1;
            continue;
        };
        owners.insert(hash.to_owned());
        let Some(mtime) = invoked_mtime(unit)? else {
            plan.unattributed += 1;
            continue;
        };
        if !is_stale(mtime, now, cutoff) {
            plan.kept += 1;
            continue;
        }
        let mut paths = vec![unit.clone()];
        let build = tree.join(BUILD).join(&name);
        if exists(&build)? {
            paths.push(build);
        }
        for (dep, dep_name) in deps.iter().zip(&dep_names) {
            if is_unit_output(dep_name, hash) {
                paths.push(dep.clone());
            }
        }
        let bytes = paths.iter().map(|p| size_of(p)).sum::<Result<u64>>()?;
        plan.stale.push(Unit { paths, bytes });
    }
    for (dep, dep_name) in deps.iter().zip(&dep_names) {
        if !owners.iter().any(|hash| is_unit_output(dep_name, hash)) {
            plan.orphans += 1;
            plan.orphan_bytes += size_of(dep)?;
        }
    }
    Ok(plan)
}

/// The hash of a unit named `<pkg>-<hash>`; None for any other name.
fn unit_hash(name: &str) -> Option<&str> {
    let (pkg, hash) = name.rsplit_once('-')?;
    let well_formed = !pkg.is_empty()
        && hash.len() == HASH_HEX_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    well_formed.then_some(hash)
}

/// Whether a `deps/` file belongs to the unit `hash`: `-<hash>` followed by the end or a `.` (`libfoo-<hash>.rlib`, `foo-<hash>`).
fn is_unit_output(file: &str, hash: &str) -> bool {
    let key = format!("-{hash}");
    file.match_indices(&key).any(|(at, _)| {
        let rest = &file[at + key.len()..];
        rest.is_empty() || rest.starts_with('.')
    })
}

/// The unit's `invoked.timestamp` mtime; None when it has none (not attributable).
fn invoked_mtime(unit: &Path) -> Result<Option<SystemTime>> {
    match fs::symlink_metadata(unit.join(INVOKED)) {
        Ok(meta) if meta.is_file() => Ok(Some(meta.modified()?)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", unit.display())),
    }
}

/// Stale when `mtime + cutoff` is before `now`. A timestamp in the future, or a sum that overflows, is kept.
fn is_stale(mtime: SystemTime, now: SystemTime, cutoff: Duration) -> bool {
    mtime.checked_add(cutoff).is_some_and(|limit| limit < now)
}

/// Apparent bytes of a path, every file under it. Symlinks count as links and are not followed.
fn size_of(path: &Path) -> Result<u64> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for child in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        total += size_of(&child?.path())?;
    }
    Ok(total)
}

/// Removes a unit's path. Refused outside the slot; a symlink is removed as a link.
fn remove(slot: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(slot),
        "refused: {} is outside {}",
        path.display(),
        slot.display()
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
    use std::os::unix::fs::symlink;

    const DAY: Duration = Duration::from_secs(SECS_PER_DAY);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-prune-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Writes `len` bytes at `path`, parents included; returns `len`.
    fn file(path: &Path, len: usize) -> u64 {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, vec![b'x'; len]).expect("file");
        len as u64
    }

    /// Sets `path`'s mtime to `age` before `now`.
    fn age(path: &Path, now: SystemTime, age: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(now - age)
            .expect("mtime");
    }

    // Unit names: 16 hex digits, as cargo writes them.
    const STALE: &str = "atl-stale-0123456789abcdef";
    const JUST: &str = "atl-just-bbbbbbbbbbbbbbbb";
    const EDGE: &str = "atl-edge-cccccccccccccccc";
    const LIVE: &str = "atl-live-dddddddddddddddd";
    const SCRIPT: &str = "atl-script-eeeeeeeeeeeeeeee";
    const NOHASH: &str = "not-a-unit";

    /// Sizes the fixture writes, so the expected bytes are stated, not computed by the code under test.
    const INVOKED_LEN: usize = 48;
    const STALE_BYTES: u64 = 48 + 10 + 300 + 1000 + 200 + 50;
    const JUST_BYTES: u64 = 48 + 300;
    /// `cargo/release` of slot 1: STALE (rebuilt 30 days ago), JUST (cutoff + 1 s), EDGE (exactly the cutoff, kept), LIVE (1 day), SCRIPT (no `invoked.timestamp`), NOHASH (not a unit name), plus two orphan `deps/` files.
    fn tree_fixture(ws: &Path, now: SystemTime) -> PathBuf {
        let tree = ws.join("out/slot-1/cargo/release");
        let fp = tree.join(FINGERPRINT);
        let deps = tree.join(DEPS);
        file(&fp.join(STALE).join(INVOKED), INVOKED_LEN);
        file(&fp.join(STALE).join("lib-atl_stale"), 10);
        file(&tree.join(BUILD).join(STALE).join("output"), 300);
        file(&deps.join("libatl_stale-0123456789abcdef.rlib"), 1000);
        file(&deps.join("libatl_stale-0123456789abcdef.rmeta"), 200);
        file(&deps.join("atl_stale-0123456789abcdef.d"), 50);
        file(&fp.join(JUST).join(INVOKED), INVOKED_LEN);
        file(&deps.join("libatl_just-bbbbbbbbbbbbbbbb.rlib"), 300);
        file(&fp.join(EDGE).join(INVOKED), INVOKED_LEN);
        file(&deps.join("libatl_edge-cccccccccccccccc.rlib"), 400);
        file(&fp.join(LIVE).join(INVOKED), INVOKED_LEN);
        file(&deps.join("libatl_live-dddddddddddddddd.rlib"), 500);
        file(&fp.join(SCRIPT).join("output"), 10);
        file(&tree.join(BUILD).join(SCRIPT).join("out"), 20);
        file(&deps.join("atl_script-eeeeeeeeeeeeeeee.d"), 70);
        file(&fp.join(NOHASH).join(INVOKED), INVOKED_LEN);
        file(&deps.join("libatl_gone-ffffffffffffffff.rlib"), 700);
        file(&deps.join("unrelated.rlib"), 400);
        age(&fp.join(STALE).join(INVOKED), now, 30 * DAY);
        age(
            &fp.join(JUST).join(INVOKED),
            now,
            DAY * 14 + Duration::from_secs(1),
        );
        age(&fp.join(EDGE).join(INVOKED), now, DAY * 14);
        age(&fp.join(LIVE).join(INVOKED), now, DAY);
        age(&fp.join(NOHASH).join(INVOKED), now, 30 * DAY);
        tree
    }

    /// Tries before a slot lock released a moment ago reads as free (the fork race in build_pool.rs tests): FREE_TRIES x FREE_POLL.
    const FREE_TRIES: u32 = 100;
    const FREE_POLL: Duration = Duration::from_millis(10);

    /// `prune_slot` again once the lock is free. A `Busy` answer while the previous hold drains is retried, never accepted as the answer.
    fn prune_once_free(
        ws: &Path,
        n: u32,
        cutoff: Duration,
        now: SystemTime,
        execute: bool,
    ) -> Outcome {
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

    #[test]
    fn stale_units_are_planned_by_their_timestamps() {
        let ws = scratch("plan");
        let now = SystemTime::now();
        tree_fixture(&ws, now);
        let plans = only_plan(prune_slot(&ws, 1, DAY * 14, now, false).expect("prune"));
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.tree, "cargo/release");
        assert_eq!(plan.units, 6);
        let stale: Vec<String> = plan.stale.iter().map(|u| file_name(&u.paths[0])).collect();
        assert_eq!(stale, [JUST, STALE]);
        assert_eq!(plan.kept, 2, "EDGE and LIVE");
        assert_eq!(plan.unattributed, 2, "SCRIPT and NOHASH");
        assert_eq!(plan.orphans, 2, "libatl_gone and unrelated");
        assert_eq!(plan.orphan_bytes, 700 + 400);
        assert_eq!(plan.stale_bytes(), STALE_BYTES + JUST_BYTES);
        let stale_paths = plan
            .stale
            .iter()
            .find(|u| file_name(&u.paths[0]) == STALE)
            .expect("STALE");
        assert_eq!(stale_paths.paths.len(), 5, "fingerprint, build, three deps");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn execute_removes_only_stale_units_and_their_outputs() {
        let ws = scratch("execute");
        let now = SystemTime::now();
        let tree = tree_fixture(&ws, now);
        let dry = only_plan(prune_slot(&ws, 1, DAY * 14, now, false).expect("dry"));
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "dry run removed"
        );
        assert_eq!(dry[0].stale.len(), 2);
        only_plan(prune_once_free(&ws, 1, DAY * 14, now, true));
        let fp = tree.join(FINGERPRINT);
        let deps = tree.join(DEPS);
        for gone in [
            fp.join(STALE),
            fp.join(JUST),
            tree.join(BUILD).join(STALE),
            deps.join("libatl_stale-0123456789abcdef.rlib"),
            deps.join("libatl_stale-0123456789abcdef.rmeta"),
            deps.join("atl_stale-0123456789abcdef.d"),
            deps.join("libatl_just-bbbbbbbbbbbbbbbb.rlib"),
        ] {
            assert!(!gone.exists(), "{} survived", gone.display());
        }
        for kept in [
            fp.join(EDGE),
            fp.join(LIVE),
            fp.join(SCRIPT).join("output"),
            tree.join(BUILD).join(SCRIPT),
            deps.join("atl_script-eeeeeeeeeeeeeeee.d"),
            fp.join(NOHASH),
            deps.join("libatl_gone-ffffffffffffffff.rlib"),
            deps.join("unrelated.rlib"),
            deps.join("libatl_live-dddddddddddddddd.rlib"),
        ] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn busy_slot_is_skipped_and_untouched() {
        let ws = scratch("busy");
        let now = SystemTime::now();
        let tree = tree_fixture(&ws, now);
        let build = build_pool::hold_slot(&ws, 1, || {}).expect("build holds slot 1");
        assert!(matches!(
            prune_slot(&ws, 1, DAY * 14, now, true).expect("prune"),
            Outcome::Busy
        ));
        assert!(
            tree.join(FINGERPRINT).join(STALE).exists(),
            "busy slot pruned"
        );
        drop(build);
        assert!(matches!(
            prune_once_free(&ws, 1, DAY * 14, now, false),
            Outcome::Done(_)
        ));
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn slot_without_an_out_dir_is_not_built() {
        let ws = scratch("not-built");
        let now = SystemTime::now();
        assert!(matches!(
            prune_slot(&ws, 2, DAY, now, true).expect("prune"),
            Outcome::NotBuilt
        ));
    }

    #[test]
    fn symlinked_slot_is_refused_and_its_target_untouched() {
        let ws = scratch("slot-link");
        let now = SystemTime::now();
        let elsewhere = ws.join("elsewhere");
        tree_fixture(&elsewhere, now);
        fs::create_dir_all(ws.join("out")).expect("out");
        symlink(elsewhere.join("out/slot-1"), ws.join("out/slot-3")).expect("link");
        let err = prune_slot(&ws, 3, DAY * 14, now, true).expect_err("symlink refused");
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
        let now = SystemTime::now();
        let tree = tree_fixture(&ws, now);
        let outside = ws.join("outside-fp");
        file(&outside.join(STALE).join(INVOKED), INVOKED_LEN);
        age(&outside.join(STALE).join(INVOKED), now, 30 * DAY);
        let fp = tree.join(FINGERPRINT);
        fs::rename(&fp, ws.join("fp-moved")).expect("move");
        symlink(&outside, &fp).expect("link");
        let err = prune_slot(&ws, 1, DAY * 14, now, true).expect_err("symlink refused");
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert!(
            outside.join(STALE).join(INVOKED).exists(),
            "link target pruned"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn trees_are_profiles_and_triple_profiles_only() {
        let ws = scratch("trees");
        let slot = ws.join("out/slot-1");
        file(&slot.join("cargo/release/.fingerprint/x/y"), 1);
        file(
            &slot.join("cargo/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        file(&slot.join("cargo-converter/release/.fingerprint/x/y"), 1);
        file(&slot.join("cargo/cxxbridge/somecrate/src/lib.rs"), 1);
        file(&slot.join("cargo/tmp/file"), 1);
        let mut found: Vec<String> = trees(&slot)
            .expect("trees")
            .iter()
            .map(|p| {
                p.strip_prefix(&slot)
                    .expect("under slot")
                    .display()
                    .to_string()
            })
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "cargo-converter/release",
                "cargo/release",
                "cargo/x86_64-unknown-linux-gnu/release"
            ]
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn slot_must_be_a_pool_dir() {
        let ws = Path::new("/ws");
        assert_eq!(pool_slot(ws, &ws.join("out/slot-2")), Some(2));
        assert_eq!(pool_slot(ws, &ws.join("out/slot-0")), None);
        assert_eq!(
            pool_slot(ws, &ws.join(format!("out/slot-{}", SLOTS + 1))),
            None
        );
        assert_eq!(pool_slot(ws, &ws.join("out/native")), None);
        assert_eq!(pool_slot(ws, &ws.join("out/slot-1/cargo")), None);
        assert_eq!(pool_slot(ws, Path::new("/elsewhere/out/slot-1")), None);
    }

    #[test]
    fn unit_names_end_in_sixteen_lowercase_hex_digits() {
        assert_eq!(
            unit_hash("atl-binderd-0123456789abcdef"),
            Some("0123456789abcdef")
        );
        assert_eq!(unit_hash("not-a-unit"), None);
        assert_eq!(unit_hash("x-0123456789ABCDEF"), None);
        assert_eq!(unit_hash("0123456789abcdef"), None);
        assert_eq!(unit_hash("-0123456789abcdef"), None);
    }

    #[test]
    fn deps_file_belongs_to_its_unit_hash_only() {
        let h = "0123456789abcdef";
        assert!(is_unit_output("libfoo-0123456789abcdef.rlib", h));
        assert!(is_unit_output("foo-0123456789abcdef", h));
        assert!(is_unit_output("foo-0123456789abcdef.d", h));
        assert!(!is_unit_output("foo-0123456789abcdef0", h));
        assert!(!is_unit_output("foo-fedcba9876543210.rlib", h));
    }

    #[test]
    fn age_is_strictly_older_than_the_cutoff() {
        let now = SystemTime::now();
        let cutoff = DAY * 14;
        assert!(
            !is_stale(now - cutoff, now, cutoff),
            "exactly the cutoff is kept"
        );
        assert!(is_stale(now - cutoff - Duration::from_secs(1), now, cutoff));
        assert!(
            !is_stale(now + DAY, now, cutoff),
            "a future timestamp is kept"
        );
    }
}
