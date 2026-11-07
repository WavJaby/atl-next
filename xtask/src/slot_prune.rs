// SPDX-License-Identifier: MIT
//! `cargo xtask slot prune`: deletes the cargo units of a slot's target trees that no cargo build used for `--older-than` days.
//! Use is the record each build writes (`unit_use`): a unit's last reported build, fresh or rebuilt. A unit with no entry was not used since the record's `since`, so it is stale only once the record itself is that old.
//! Trees under `<ws>/out/slot-<n>`: `cargo/<profile>`, `cargo/<triple>/<profile>`, `cargo-converter/<profile>`.
//! A unit is `.fingerprint/<pkg>-<hash>/`, `build/<pkg>-<hash>/` (build-script output), the `deps/` and `examples/` files `*-<hash>` or `*-<hash>.<ext>`, and the uplifted hard links (`<tree>/<name>`) of those files; `<hash>` is 16 lowercase hex digits.
//! Kept, never deleted: entries that are no unit, `deps/` and `examples/` files no unit owns (counted as orphans). A tree whose record cannot be read refuses its slot.
//! Busy: a build holds the slot's lock (`build_pool::hold_slot`); prune takes it exclusive without waiting and skips the slot.
//! Symlinks are refused, never followed: a link under the slot could point the prune outside it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::build_pool;
use crate::slot::{self, Holder, SLOTS};
use crate::unit_use::{self, BUILD, DEPS, EXAMPLES, FINGERPRINT};

/// Default `--older-than`, in days: a unit no cargo build used for this long is stale. First guess, not measured. A shorter cutoff frees more but recompiles stable dependencies more often; a rebuilt dependency also recompiles its dependents (2026-11-08: pruning one unit recompiled it and its dependent crate). Re-fit from the rebuild counts after a few lands.
pub const PRUNE_AFTER_DAYS: u64 = 14;

const SECS_PER_DAY: u64 = 86_400;
const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;
/// Cargo target roots under a slot: `out/cargo` and `out/cargo-converter`.
const TREE_ROOTS: [&str; 2] = ["cargo", "cargo-converter"];

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
    pub dir: PathBuf,
    /// Fingerprint entries in the tree.
    pub units: usize,
    pub stale: Vec<Unit>,
    pub kept: usize,
    /// Units with no record entry; counted in `kept` or `stale` too.
    pub unrecorded: usize,
    /// Fingerprint entries that are no unit. Kept.
    pub unattributed: usize,
    /// `deps/` and `examples/` files no unit owns. Kept.
    pub orphans: usize,
    pub orphan_bytes: u64,
}

impl TreePlan {
    fn stale_bytes(&self) -> u64 {
        self.stale.iter().map(|u| u.bytes).sum()
    }
}

/// A stale unit: its hash, the paths its removal takes (the fingerprint dir first), and the bytes they hold, each inode once.
#[derive(Debug)]
pub struct Unit {
    pub hash: String,
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
}

/// Totals of one sweep: stale units and their bytes, and the slots that failed.
#[derive(Debug, Default)]
pub struct Sweep {
    pub units: usize,
    pub bytes: u64,
    pub failed: usize,
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
    let sweep = sweep(ws, holders, &slots, older_than, execute)?;
    Ok(if sweep.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Prunes each slot of `slots`, printing one line per target tree and a summary. Busy and failed slots are reported; a failure is counted in `failed`, not returned.
pub fn sweep(
    ws: &Path,
    holders: &[Holder],
    slots: &[u32],
    older_than: u64,
    execute: bool,
) -> Result<Sweep> {
    let cutoff = older_than
        .checked_mul(SECS_PER_DAY)
        .with_context(|| format!("--older-than {older_than} days is too large"))?;
    let now = unit_use::now()?;
    let mut total = Sweep::default();
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
            Ok(Outcome::Done(plans)) => {
                for plan in plans {
                    println!(
                        "slot {n} {}: units {}, stale {} ({}), kept {} (unrecorded {}), unattributed {}, orphan deps {} ({})",
                        plan.tree,
                        plan.units,
                        plan.stale.len(),
                        human(plan.stale_bytes()),
                        plan.kept,
                        plan.unrecorded,
                        plan.unattributed,
                        plan.orphans,
                        human(plan.orphan_bytes)
                    );
                    total.units += plan.stale.len();
                    total.bytes += plan.stale_bytes();
                }
            }
            Err(e) => {
                total.failed += 1;
                println!("slot {n}: FAILED: {e:#}");
            }
        }
    }
    let verb = if execute {
        "removed"
    } else {
        "dry run, would remove (add --execute)"
    };
    println!(
        "slot prune: {verb} {} stale units, {}",
        total.units,
        human(total.bytes)
    );
    Ok(total)
}

/// Plans slot `n`'s prune and, with `execute`, removes its stale units and their record entries. Holds the slot's lock for the whole call.
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
            let hashes: Vec<String> = plan.stale.iter().map(|u| u.hash.clone()).collect();
            if !hashes.is_empty() {
                unit_use::forget(&plan.dir, &hashes)?;
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

/// The plan for one tree. Units are the fingerprint dirs; `build/`, `deps/`, `examples/` files are attributed by hash, uplifted links by inode. Stale when the unit's last use (its record entry, else the record's `since`) is older than the cutoff.
fn plan_tree(slot: &Path, tree: &Path, now: u64, cutoff: u64) -> Result<TreePlan> {
    let label = tree
        .strip_prefix(slot)
        .unwrap_or(tree)
        .display()
        .to_string();
    let record = unit_use::load(tree)?;
    let mut plan = TreePlan {
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
        let last_use = match &record {
            Some(record) => match record.units.get(&hash) {
                Some(&at) => at,
                None => {
                    plan.unrecorded += 1;
                    record.since
                }
            },
            None => {
                plan.unrecorded += 1;
                plan.kept += 1;
                continue;
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
    Ok(plan)
}

/// Stale when the last use is more than `cutoff` seconds before `now`. A use in the future, or a sum that overflows, is kept.
fn is_stale(last_use: u64, now: u64, cutoff: u64) -> bool {
    last_use.saturating_add(cutoff) < now
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
    use serde_json::{Value, json};
    use std::os::unix::fs::symlink;

    /// Fixture clock, literal seconds since the epoch. Every use and record time below is relative to it.
    const NOW: u64 = 1_800_000_000;
    /// The default cutoff (14 days), written out rather than taken from the code under test.
    const DAY: u64 = 86_400;
    const CUTOFF: u64 = 14 * DAY;

    // Unit names; the hash ends each name. Each row says when the fixture's record puts its last use.
    const STALE: &str = "atl-stale-0123456789abcdef"; // 30 days ago: stale
    const JUST: &str = "atl-just-bbbbbbbbbbbbbbbb"; // cutoff + 1 s ago: stale
    const EDGE: &str = "atl-edge-cccccccccccccccc"; // exactly the cutoff ago: kept
    const LIVE: &str = "atl-live-dddddddddddddddd"; // 1 day ago: kept
    const OLD: &str = "atl-old-eeeeeeeeeeeeeeee"; // superseded generation, 20 days ago: stale
    const NEW: &str = "atl-old-ffffffffffffffff"; // its newer generation, 2 days ago: kept
    const NEVER: &str = "atl-never-1111111111111111"; // no entry: stale while the record is older than the cutoff
    const EXAMPLE: &str = "atl-ex-2222222222222222"; // examples/ file + uplifted link, 30 days ago: stale
    const NOHASH: &str = "not-a-unit"; // entry that is no unit: kept

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

    /// `cargo/release` of slot 1 with one unit per name above, two orphan `deps/` files, and no record.
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
        sized(&fp.join(EXAMPLE).join("lib-atl_ex"), 9);
        let example = tree.join(EXAMPLES).join("ex-2222222222222222");
        sized(&example, 900);
        fs::hard_link(&example, tree.join("ex")).expect("uplift");
        sized(&fp.join(NOHASH).join("lib-x"), 11);
        sized(&deps.join("libatl_gone-9999999999999999.rlib"), 700);
        sized(&deps.join("unrelated.rlib"), 400);
        tree
    }

    /// A use record for `tree`: `since`, and (unit name, last use) pairs keyed by each name's hash.
    fn write_record(tree: &Path, since: u64, uses: &[(&str, u64)]) {
        let units: BTreeMap<String, u64> = uses
            .iter()
            .map(|(name, at)| (name.rsplit_once('-').expect("hash").1.to_owned(), *at))
            .collect();
        fs::create_dir_all(tree).expect("tree");
        let text = json!({"since": since, "units": units}).to_string();
        fs::write(tree.join(".atl-unit-use.json"), text).expect("record");
    }

    /// The uses of the fixture that a day-old record holds; every other unit has no entry.
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
    const FREE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

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

    #[test]
    fn stale_units_are_planned_by_recorded_use() {
        let ws = scratch("plan");
        let tree = tree_fixture(&ws);
        write_record(&tree, NOW - 60 * DAY, &recent_uses());
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        assert_eq!(plan.tree, "cargo/release");
        let mut stale: Vec<String> = plan.stale.iter().map(|u| file_name(&u.paths[0])).collect();
        stale.sort();
        assert_eq!(stale, [EXAMPLE, JUST, NEVER, OLD, STALE]);
        assert_eq!(plan.units, 9, "every fingerprint entry");
        assert_eq!(plan.kept, 3, "EDGE, LIVE, NEW");
        assert_eq!(plan.unrecorded, 1, "NEVER");
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
        write_record(&tree, NOW - 60 * DAY, &recent_uses());
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
        write_record(&tree, NOW - 60 * DAY, &[]);
        let reused = json!({"reason": "compiler-artifact", "package_id": "path+file:///ws/atl-stale#0.1.0",
            "filenames": [tree.join("deps/libatl_stale-0123456789abcdef.rlib").display().to_string()],
            "executable": null, "fresh": true});
        unit_use::record(&[reused], NOW).expect("record the reuse");
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        let stale: Vec<String> = plan.stale.iter().map(|u| file_name(&u.paths[0])).collect();
        assert!(
            !stale.contains(&STALE.to_owned()),
            "reused unit planned for removal: {stale:?}"
        );
        assert!(
            stale.contains(&JUST.to_owned()),
            "control: JUST has no entry in a 60-day record"
        );
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn a_unit_without_an_entry_is_kept_while_the_record_is_young() {
        let ws = scratch("young");
        let tree = tree_fixture(&ws);
        write_record(&tree, NOW - 3 * DAY, &[]);
        let plan = only_plan(prune_slot(&ws, 1, CUTOFF, NOW, false).expect("prune")).remove(0);
        assert!(plan.stale.is_empty(), "stale under a 3-day record");
        assert_eq!(plan.kept, 8, "every hashed unit");
        assert_eq!(plan.unrecorded, 8);
        assert_eq!(plan.unattributed, 1, "NOHASH");
        fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn busy_slot_is_skipped_and_untouched() {
        let ws = scratch("busy");
        let tree = tree_fixture(&ws);
        write_record(&tree, NOW - 60 * DAY, &recent_uses());
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

    #[test]
    fn trees_are_profiles_and_triple_profiles_only() {
        let ws = scratch("trees");
        let slot = ws.join("out/slot-1");
        sized(&slot.join("cargo/release/.fingerprint/x/y"), 1);
        sized(
            &slot.join("cargo/x86_64-unknown-linux-gnu/release/.fingerprint/x/y"),
            1,
        );
        sized(&slot.join("cargo-converter/release/.fingerprint/x/y"), 1);
        sized(&slot.join("cargo/cxxbridge/somecrate/src/lib.rs"), 1);
        sized(&slot.join("cargo/tmp/file"), 1);
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
