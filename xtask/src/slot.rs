// SPDX-License-Identifier: MIT
//! Build-output slots: the out dir a checkout builds into when `--out` / `--java-out` is not given.
//!
//! The pool is `build.slots` (`ws_config`) dirs `<workspace>/out/slot-<n>`. A checkout holds at most one slot, recorded as its number in `atl-out-slot` inside its git dir (`.git/worktrees/<name>/` for a linked worktree, `.git/` for the main checkout): the record goes with the worktree, and nothing tracked depends on it.
//! Resolution per flag: explicit flag > the checkout's slot > the old default. The old default prints a note naming `cargo xtask slot assign`, so it is never taken silently.
//! A record that cannot be read as a slot in the pool is an error, never a silent fallback.
//! A slot held by a live other checkout is refused unless `--force`; a record whose checkout directory is gone is stale and may be reassigned.
//! A process that resolves a slot holds its slot lock (`build_pool::hold_slot`) until it exits, or until `after_build` releases it to prune the slot's stale sessions; `slot prune` skips a slot whose lock it cannot take.
//! A slotted checkout's `target/` is a symlink to `out/slot-<n>/target`, so every cargo build there (xtask, `cargo test`, clippy) builds into its slot: one target dir per slot. A symlink because it is the only per-checkout switch cargo reads with no tracked file or env var; `.gitignore`'s `/target` matches it (`/target/` does not match a symlink).
//! `cargo clean` unlinks the link and keeps the slot's files: the checkout then builds into a real `target/` until `slot assign --execute` relinks it (`slot show` reports which).
//! A slot's target leaves with its holder: land removes it (`leave`); an assign wipes it when its holder is stale or its checkout moves to another slot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool::{self, SlotLock};
use crate::git::git;
use crate::slot_prune::{self, Outcome};
use crate::unit_use;
use crate::ws_config::{self, WsConfig};

/// The pool size (`build.slots`, `ws_config::DEFAULT_SLOTS` explains the default), refused while any record holds a slot above it: lowering the limit would strand that checkout's out dir and record.
pub fn pool(cfg: &WsConfig, holders: &[Holder]) -> Result<u32> {
    let n = cfg.slots.value;
    let above: Vec<String> = holders
        .iter()
        .filter(|h| h.slot > n)
        .map(|h| {
            let stale = if h.live { "" } else { " (stale)" };
            format!("slot {} by {}{stale}", h.slot, h.checkout.display())
        })
        .collect();
    ensure!(
        above.is_empty(),
        "{}: `build.slots` = {n} is below slots still held: {}; land or move those checkouts (a stale one: delete its `{RECORD}` record), or raise `build.slots`",
        cfg.path.display(),
        above.join(", ")
    );
    Ok(n)
}

/// File in a checkout's git dir holding its slot number.
const RECORD: &str = "atl-out-slot";

/// A checkout's cargo target dir: a link to its slot's `target/` while it holds one.
const TARGET: &str = "target";

/// Where one `--out` / `--java-out` goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Explicit,
    Slot(u32),
    Fallback,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub dir: PathBuf,
    pub source: Source,
}

/// A checkout holding a slot. `live` = its checkout directory exists; a holder that is not live is stale (worktree deleted without `git worktree remove`) and may be reassigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub slot: u32,
    pub checkout: PathBuf,
    pub record: PathBuf,
    pub live: bool,
}

/// What `assign` does: keep the checkout's slot, or delete the displaced records in `release`, then write its own.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Keep(u32),
    Assign { slot: u32, release: Vec<Holder> },
}

/// `slot` subcommands; `assign` is a dry run unless `--execute`.
#[derive(clap::Subcommand)]
pub enum Slot {
    /// This checkout's slot, the out dir it builds into, and its `target/` link.
    Show,
    /// Every slot: the checkout holding it, live or stale.
    List,
    /// Give this checkout a slot and link its `target/` to the slot's (dry run unless --execute).
    ///
    /// Without N: the lowest slot no live checkout holds. A slot held by a live other checkout is refused unless --force, which takes it over; that checkout then builds into the old default and its `target/` link is removed (a build it runs there fails); executables in the slot's `deps/` that embed its paths (test binaries' `CARGO_BIN_EXE_*`) are removed, the rest stays as cache.
    /// An own real `target/` is moved into the slot when the slot has no target yet, else removed.
    Assign {
        /// Slot number, 1..=build.slots.
        slot: Option<u32>,
        /// Write the record and the link (default: print the plan only).
        #[arg(long)]
        execute: bool,
        /// Take the slot over from a live checkout.
        #[arg(long)]
        force: bool,
    },
    /// Deletes the cargo units and incremental sessions of the target trees that no build used for `--older-than` days (dry run unless --execute).
    ///
    /// Name a slot, or `--all` for every slot plus the trees no slot owns (`repo/target`, `out/egl-driver/*/cargo*`). A slot a build holds, and a tree whose `.cargo-lock` a build holds, is skipped with a line saying so. Use is what each recorded build wrote (`<tree>/.atl-unit-use.json`, `unit_use`); a unit with no entry is judged by its fingerprint dir's mtime. Incremental: per crate, the newest finalized session stays, with the held and the young ones.
    Prune {
        /// Slot number, 1..=build.slots.
        #[arg(conflicts_with = "all")]
        slot: Option<u32>,
        /// Every slot of the pool.
        #[arg(long)]
        all: bool,
        /// Stale after this many days since the last build that used the unit.
        #[arg(long, value_name = "DAYS", default_value_t = slot_prune::PRUNE_AFTER_DAYS)]
        older_than: u64,
        /// Delete (default: print the plan only).
        #[arg(long)]
        execute: bool,
    },
}

/// The out dir of slot `n` under workspace `ws`.
pub fn slot_dir(ws: &Path, n: u32) -> PathBuf {
    ws.join("out").join(format!("slot-{n}"))
}

/// Explicit > slot > fallback. Pure: the caller supplies the flag's dir, the checkout's slot and the old default.
pub fn resolve(
    explicit: Option<PathBuf>,
    slot: Option<u32>,
    ws: &Path,
    fallback: PathBuf,
) -> Target {
    if let Some(dir) = explicit {
        return Target {
            dir,
            source: Source::Explicit,
        };
    }
    match slot {
        Some(n) => Target {
            dir: slot_dir(ws, n),
            source: Source::Slot(n),
        },
        None => Target {
            dir: fallback,
            source: Source::Fallback,
        },
    }
}

/// The out dir for `flag` (`--out` or `--java-out`) of the checkout at `root`. `fallback` is the old default, computed from the workspace; it is used only when the checkout has no slot, and then a note names `cargo xtask slot assign`.
pub fn out_dir(
    root: &Path,
    explicit: Option<&Path>,
    flag: &str,
    fallback: impl FnOnce(&Path) -> PathBuf,
) -> Result<PathBuf> {
    let workspace = Layout::of(root)?.workspace().to_owned();
    let explicit = explicit.map(std::path::absolute).transpose()?;
    // an explicit dir needs no record: skip the git call
    let slot = match explicit {
        Some(_) => None,
        None => read_record(&record_of(root)?)?,
    };
    if slot.is_some() {
        pool(&ws_config::load(&workspace)?, &holders_of(root)?)?;
    }
    let fallback = fallback(&workspace);
    let target = resolve(explicit, slot, &workspace, fallback);
    if target.source == Source::Fallback {
        eprintln!(
            "{flag}: this checkout has no slot, using the old default {} (`cargo xtask slot assign` gives it one)",
            target.dir.display()
        );
    }
    hold_if_slot(&workspace, &target.dir)?;
    Ok(target.dir)
}

/// Slot locks this process holds, one per slot number; released at exit.
static HELD: Mutex<Vec<(u32, SlotLock)>> = Mutex::new(Vec::new());

/// Holds the lock of `dir` when it is a pool slot, so `slot prune` skips the slot while this process runs. Other dirs take no lock.
fn hold_if_slot(ws: &Path, dir: &Path) -> Result<()> {
    let Some(n) = slot_prune::pool_slot(ws, dir) else {
        return Ok(());
    };
    let mut held = HELD
        .lock()
        .map_err(|_| anyhow::anyhow!("slot lock table poisoned"))?;
    if held.iter().all(|(m, _)| *m != n) {
        let lock = build_pool::hold_slot(ws, n, || eprintln!("slot {n}: being pruned, waiting"))?;
        held.push((n, lock));
    }
    Ok(())
}

/// The end of a checkout's build (`native`, `check`, `cross`, the harness commands): the stale incremental sessions of the trees it wrote go, one line per slot. `result` passes through unless the prune fails.
pub fn after_build(root: &Path, result: Result<ExitCode>) -> Result<ExitCode> {
    match (result, finish_build(root)) {
        (result, Ok(())) => result,
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Err(prune)) => {
            eprintln!("session prune: {prune:#}");
            Err(e)
        }
    }
}

/// Prunes the sessions of this process's slots and of the checkout's record slot; with no slot, of the checkout's own target dir.
fn finish_build(root: &Path) -> Result<()> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let now = unit_use::now()?;
    // a shared hold blocks the prune, so the slots this process resolved are released first
    let mut slots = release_holds()?;
    slots.extend(read_record(&record_of(root)?)?);
    if slots.is_empty() {
        return finish_target(&ws, &root.join(TARGET), now);
    }
    for n in slots {
        let line = match slot_prune::prune_sessions(&ws, n, now)? {
            Outcome::NotBuilt => format!("session prune: slot {n}: not built"),
            Outcome::Busy => {
                format!("session prune: slot {n}: busy, skipped: another process holds it")
            }
            Outcome::Done(plans) => slot_prune::sessions_line(&format!("slot {n}"), &plans),
        };
        println!("{line}");
    }
    Ok(())
}

/// Drops this process's slot locks (`hold_if_slot`) and returns their slot numbers.
fn release_holds() -> Result<BTreeSet<u32>> {
    let mut held = HELD
        .lock()
        .map_err(|_| anyhow::anyhow!("slot lock table poisoned"))?;
    Ok(held.drain(..).map(|(n, _lock)| n).collect())
}

/// No slot: the checkout's own target dir (the main checkout's `repo/target`). A link is refused: no record names the dir it points at.
fn finish_target(ws: &Path, target: &Path, now: u64) -> Result<()> {
    let label = target
        .strip_prefix(ws)
        .unwrap_or(target)
        .display()
        .to_string();
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "refused: {} is a link and this checkout has no slot record",
            target.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("session prune: {label}: no target dir");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
    }
    let plans = slot_prune::prune_target_sessions(ws, target, now)?;
    println!("{}", slot_prune::sessions_line(&label, &plans));
    Ok(())
}

/// The record file of the checkout at `checkout`, in its own git dir.
fn record_of(checkout: &Path) -> Result<PathBuf> {
    let git_dir = git(Some(checkout), &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir).join(RECORD))
}

/// The slot in a record file; None when there is no record.
fn read_record(path: &Path) -> Result<Option<u32>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(text.trim()).map(Some).with_context(|| {
        format!(
            "slot record {} (delete it, then `cargo xtask slot assign N --execute`)",
            path.display()
        )
    })
}

/// A slot number >= 1; whether it is in the pool is [`pool`]'s check, which names every holder above the limit.
fn parse(text: &str) -> Result<u32> {
    let n: u32 = text
        .parse()
        .with_context(|| format!("{text:?} is not a slot number"))?;
    ensure!(n >= 1, "slot {n} is not a slot number (slots start at 1)");
    Ok(n)
}

/// Every record of the repository: the main checkout's (`common` = its `.git`) and each linked worktree's under `common/worktrees/`.
pub fn holders(common: &Path) -> Result<Vec<Holder>> {
    let mut found = Vec::new();
    let main = common.join(RECORD);
    if let Some(slot) = read_record(&main)? {
        let checkout = common
            .parent()
            .with_context(|| format!("{} has no parent", common.display()))?;
        found.push(holder(slot, checkout.to_owned(), main));
    }
    let worktrees = common.join("worktrees");
    if worktrees.is_dir() {
        for entry in std::fs::read_dir(&worktrees)
            .with_context(|| format!("reading {}", worktrees.display()))?
        {
            let git_dir = entry?.path();
            let record = git_dir.join(RECORD);
            let Some(slot) = read_record(&record)? else {
                continue;
            };
            // `gitdir` names the worktree's `.git` file; its parent is the checkout
            let gitfile = git_dir.join("gitdir");
            let text = std::fs::read_to_string(&gitfile)
                .with_context(|| format!("reading {}", gitfile.display()))?;
            let checkout = Path::new(text.trim())
                .parent()
                .with_context(|| format!("{} names no checkout", gitfile.display()))?;
            found.push(holder(slot, checkout.to_owned(), record));
        }
    }
    found.sort_by_key(|h| h.slot);
    Ok(found)
}

/// Every slot record of the repository that `root` belongs to (see [`holders`]).
pub fn holders_of(root: &Path) -> Result<Vec<Holder>> {
    let common = PathBuf::from(git(
        Some(root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    holders(&common)
}

fn holder(slot: u32, checkout: PathBuf, record: PathBuf) -> Holder {
    let live = checkout.exists();
    Holder {
        slot,
        checkout,
        record,
        live,
    }
}

/// Plan for giving the checkout whose record is `record` slot `want` (None: the lowest slot no live other checkout holds) of a pool of `slots`.
/// A slot held by a live other checkout is refused unless `force`; stale holders are released without it.
pub fn plan_assign(
    record: &Path,
    current: Option<u32>,
    holders: &[Holder],
    want: Option<u32>,
    force: bool,
    slots: u32,
) -> Result<Plan> {
    let others = |n: u32| -> Vec<Holder> {
        holders
            .iter()
            .filter(|h| h.slot == n && h.record != record)
            .cloned()
            .collect()
    };
    let slot = match want {
        Some(n) => {
            ensure!(
                (1..=slots).contains(&n),
                "slot {n} is outside 1..={slots} (`build.slots`)"
            );
            n
        }
        None => {
            if let Some(n) = current {
                return Ok(Plan::Keep(n));
            }
            match (1..=slots).find(|&n| others(n).iter().all(|h| !h.live)) {
                Some(n) => n,
                None => bail!(
                    "every slot is held by a live checkout (`cargo xtask slot list`); name one with --force to take it over"
                ),
            }
        }
    };
    if current == Some(slot) {
        return Ok(Plan::Keep(slot));
    }
    let release = others(slot);
    if let Some(live) = release.iter().find(|h| h.live) {
        ensure!(
            force,
            "slot {slot} is held by {}; --force takes it over (that checkout then builds into the old default)",
            live.checkout.display()
        );
    }
    Ok(Plan::Assign { slot, release })
}

/// Writes an `Assign` plan: deletes the displaced records, then this checkout's record. `Keep` writes nothing.
pub fn apply(plan: &Plan, record: &Path) -> Result<()> {
    if let Plan::Assign { slot, release } = plan {
        for h in release {
            std::fs::remove_file(&h.record)
                .with_context(|| format!("releasing {}", h.record.display()))?;
        }
        std::fs::write(record, format!("{slot}\n"))
            .with_context(|| format!("writing {}", record.display()))?;
    }
    Ok(())
}

/// `<checkout>/target` as found on disk: nothing, a link (its target), a real dir, or another file.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Absent,
    Link(PathBuf),
    Dir,
    Other,
}

/// What `link` is now; does not follow a symlink.
pub fn entry(link: &Path) -> Result<Entry> {
    let meta = match std::fs::symlink_metadata(link) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Entry::Absent),
        Err(e) => return Err(e).with_context(|| format!("reading {}", link.display())),
    };
    if meta.file_type().is_symlink() {
        let to = std::fs::read_link(link).with_context(|| format!("reading {}", link.display()))?;
        Ok(Entry::Link(to))
    } else if meta.is_dir() {
        Ok(Entry::Dir)
    } else {
        Ok(Entry::Other)
    }
}

/// Slot `n`'s cargo target dir: what a slotted checkout's `target/` links to.
pub fn slot_target(ws: &Path, n: u32) -> PathBuf {
    slot_dir(ws, n).join(TARGET)
}

/// One change to a checkout's `target/` or a slot's target dir. `target_steps` plans them, `apply_steps` runs them in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Remove a displaced holder's link: that checkout builds into its old default.
    Unlink(PathBuf),
    /// Remove a slot's target dir: its holder has left.
    Wipe(PathBuf),
    /// Create a slot's target dir.
    Mkdir(PathBuf),
    /// Move a checkout's own real `target/` into an empty slot target, keeping its build.
    Adopt { from: PathBuf, to: PathBuf },
    /// Remove a checkout's own real `target/`: its slot's target exists already.
    Remove(PathBuf),
    /// Remove an executable a live displaced holder built: it embeds `checkout`'s absolute paths (`CARGO_BIN_EXE_*`, `CARGO_MANIFEST_DIR`), and cargo may reuse it as fresh for the new holder, which then runs the other checkout's binaries.
    Drop { file: PathBuf, checkout: PathBuf },
    /// Make `at` a link to `to`, creating `to`; replaces a link at `at`.
    Link { at: PathBuf, to: PathBuf },
}

/// The steps that give checkout `root` slot `slot`. `current` is its slot now, `release` the holders displaced from `slot`.
/// Displaced links go; a slot whose displaced holders are all stale is wiped; a live holder's files stay as cache, minus the executables embedding its paths ([`embedding`]); the slot the checkout leaves is wiped; then `root/target` is linked.
pub fn target_steps(
    root: &Path,
    ws: &Path,
    slot: u32,
    current: Option<u32>,
    release: &[Holder],
) -> Result<Vec<Step>> {
    let want = slot_target(ws, slot);
    let mut steps = Vec::new();
    for h in release {
        let link = h.checkout.join(TARGET);
        if entry(&link)? == Entry::Link(slot_target(ws, h.slot)) {
            steps.push(Step::Unlink(link));
        }
    }
    let stale = !release.is_empty() && release.iter().all(|h| !h.live);
    if stale && want.exists() {
        steps.push(Step::Wipe(want.clone()));
    } else {
        for h in release.iter().filter(|h| h.live && h.checkout != root) {
            for file in embedding(&want, &h.checkout)? {
                steps.push(Step::Drop {
                    file,
                    checkout: h.checkout.clone(),
                });
            }
        }
    }
    if let Some(old) = current.filter(|&n| n != slot) {
        let dir = slot_target(ws, old);
        if dir.exists() {
            steps.push(Step::Wipe(dir));
        }
    }
    steps.extend(link_steps(&root.join(TARGET), &want)?);
    Ok(steps)
}

/// Executables in cargo target `target`'s `deps/` dirs (`<profile>/deps`, `<triple>/<profile>/deps`) whose bytes hold `<checkout>/`. Executable = regular file with an x bit and no extension (test and bin outputs; `.so`/`.rlib` are not run with baked paths).
/// Matching bytes, not names: debug info and `env!` paths both carry the builder's checkout, and a binary built by the new holder never holds the other's path.
fn embedding(target: &Path, checkout: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let needle = format!("{}/", checkout.display()).into_bytes();
    let mut levels = vec![target.to_owned()];
    levels.extend(subdirs(target)?);
    let mut deps = Vec::new();
    for level in levels {
        for profile in subdirs(&level)? {
            let d = profile.join("deps");
            if d.is_dir() {
                deps.push(d);
            }
        }
    }
    let mut found = Vec::new();
    for d in deps {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let path = e?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.is_file()
                || meta.permissions().mode() & 0o111 == 0
                || path.extension().is_some()
            {
                continue;
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if bytes.windows(needle.len()).any(|w| w == needle.as_slice()) {
                found.push(path);
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// Directories directly under `dir` (none when it is missing).
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e?;
        if e.file_type()?.is_dir() {
            out.push(e.path());
        }
    }
    Ok(out)
}

/// Steps that make `link` (`<checkout>/target`) a link to `want`: none when it is one already (a missing `want` is created), else a link, after a real dir is moved into an empty `want` or removed.
fn link_steps(link: &Path, want: &Path) -> Result<Vec<Step>> {
    let relink = Step::Link {
        at: link.to_owned(),
        to: want.to_owned(),
    };
    Ok(match entry(link)? {
        Entry::Link(to) if to.as_path() == want => {
            if want.exists() {
                Vec::new()
            } else {
                vec![Step::Mkdir(want.to_owned())]
            }
        }
        Entry::Absent | Entry::Link(_) => vec![relink],
        Entry::Dir if want.exists() => vec![Step::Remove(link.to_owned()), relink],
        Entry::Dir => vec![
            Step::Adopt {
                from: link.to_owned(),
                to: want.to_owned(),
            },
            relink,
        ],
        Entry::Other => bail!("{} is a file: remove it, then rerun", link.display()),
    })
}

/// Runs `steps` in order. Callers plan them with `target_steps` first; this is the only place they change anything.
pub fn apply_steps(steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Unlink(p) => {
                std::fs::remove_file(p).with_context(|| format!("unlinking {}", p.display()))?;
            }
            Step::Wipe(p) | Step::Remove(p) => {
                std::fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))?;
            }
            Step::Mkdir(p) => {
                std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
            }
            Step::Drop { file, .. } => {
                std::fs::remove_file(file)
                    .with_context(|| format!("removing {}", file.display()))?;
            }
            Step::Adopt { from, to } => {
                let parent = to.parent().unwrap_or(to);
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
                std::fs::rename(from, to)
                    .with_context(|| format!("moving {} -> {}", from.display(), to.display()))?;
            }
            Step::Link { at, to } => {
                std::fs::create_dir_all(to)
                    .with_context(|| format!("creating {}", to.display()))?;
                if let Entry::Link(_) = entry(at)? {
                    std::fs::remove_file(at)
                        .with_context(|| format!("unlinking {}", at.display()))?;
                }
                std::os::unix::fs::symlink(to, at)
                    .with_context(|| format!("linking {} -> {}", at.display(), to.display()))?;
            }
        }
    }
    Ok(())
}

/// One dry-run line per step.
pub fn describe_step(step: &Step) -> String {
    match step {
        Step::Unlink(p) => format!("unlink {} (displaced)", p.display()),
        Step::Wipe(p) => format!("wipe {}", p.display()),
        Step::Mkdir(p) => format!("create {}", p.display()),
        Step::Adopt { from, to } => {
            format!(
                "move {} -> {} (keeps its build)",
                from.display(),
                to.display()
            )
        }
        Step::Remove(p) => format!("remove {} (its slot's target exists)", p.display()),
        Step::Drop { file, checkout } => format!(
            "remove {} (embeds {}'s paths; cargo could reuse it as fresh)",
            file.display(),
            checkout.display()
        ),
        Step::Link { at, to } => format!("link {} -> {}", at.display(), to.display()),
    }
}

/// The `target/` line of `slot show`.
fn describe_entry(e: &Entry, want: &Path) -> String {
    match e {
        Entry::Link(to) if to.as_path() == want => format!("linked to {}", want.display()),
        Entry::Link(to) => format!(
            "links to {}, not this slot's target: `slot assign --execute` relinks it",
            to.display()
        ),
        Entry::Dir => {
            "real dir, not linked: `slot assign --execute` moves or removes it".to_owned()
        }
        Entry::Absent => "missing: `slot assign --execute` links it".to_owned(),
        Entry::Other => "a file: remove it, then `slot assign --execute`".to_owned(),
    }
}

/// The slot in a checkout's record; None when it holds none.
pub fn record_slot(checkout: &Path) -> Result<Option<u32>> {
    read_record(&record_of(checkout)?)
}

/// Land: the checkout's `target/` leaves with it. Its link or real dir goes, and so does the target of its slot (`slot`, from its record): the holder is leaving.
/// Returns land's summary line.
pub fn leave(root: &Path, ws: &Path, slot: Option<u32>) -> Result<String> {
    let link = root.join(TARGET);
    let mut removed = Vec::new();
    match entry(&link)? {
        Entry::Link(_) => {
            std::fs::remove_file(&link).with_context(|| format!("unlinking {}", link.display()))?;
            removed.push(link.display().to_string());
        }
        Entry::Dir => {
            std::fs::remove_dir_all(&link)
                .with_context(|| format!("removing {}", link.display()))?;
            removed.push(link.display().to_string());
        }
        Entry::Absent | Entry::Other => {}
    }
    if let Some(n) = slot {
        let dir = slot_target(ws, n);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            removed.push(dir.display().to_string());
        }
    }
    Ok(if removed.is_empty() {
        "no target".to_owned()
    } else {
        format!("target removed: {}", removed.join(", "))
    })
}

pub fn run(root: &Path, cmd: &Slot) -> Result<ExitCode> {
    let layout = Layout::of(root)?;
    let ws = layout.workspace();
    let record = record_of(root)?;
    let common = PathBuf::from(git(
        Some(root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let cfg = ws_config::load(ws)?;
    let all = holders(&common)?;
    let slots = pool(&cfg, &all)?;
    match cmd {
        Slot::Prune {
            slot,
            all: every,
            older_than,
            execute,
        } => return slot_prune::run(ws, &all, slots, *slot, *every, *older_than, *execute),
        Slot::Show => match read_record(&record)? {
            Some(n) => {
                let dir = slot_dir(ws, n);
                let built = if dir.is_dir() {
                    "exists"
                } else {
                    "not built yet"
                };
                println!("slot {n}: {} ({built})", dir.display());
                let state = describe_entry(&entry(&root.join(TARGET))?, &slot_target(ws, n));
                println!("target: {state}");
            }
            None => println!(
                "no slot: --out and --java-out fall back to the old defaults; `cargo xtask slot assign` gives this checkout one"
            ),
        },
        Slot::List => {
            println!("{}", cfg.summary());
            for n in 1..=slots {
                let dir = slot_dir(ws, n).display().to_string();
                let held: Vec<&Holder> = all.iter().filter(|h| h.slot == n).collect();
                if held.is_empty() {
                    println!("slot {n}: free  {dir}");
                }
                for h in held {
                    let state = if h.live {
                        "held by"
                    } else {
                        "stale, checkout gone:"
                    };
                    let me = if h.record == record {
                        " (this checkout)"
                    } else {
                        ""
                    };
                    println!("slot {n}: {state} {}{me}  {dir}", h.checkout.display());
                }
            }
        }
        Slot::Assign {
            slot,
            execute,
            force,
        } => {
            let current = read_record(&record)?;
            let plan = plan_assign(&record, current, &all, *slot, *force, slots)?;
            let (to, release) = match &plan {
                Plan::Keep(n) => (*n, &[][..]),
                Plan::Assign { slot: to, release } => (*to, release.as_slice()),
            };
            let steps = target_steps(root, ws, to, current, release)?;
            match &plan {
                Plan::Keep(n) => println!("slot {n}: already held by this checkout"),
                Plan::Assign { slot: to, release } => {
                    for h in release {
                        let state = if h.live { "live, displaced" } else { "stale" };
                        println!(
                            "release slot {} from {} ({state})",
                            h.slot,
                            h.checkout.display()
                        );
                    }
                    println!("assign slot {to} -> {}", slot_dir(ws, *to).display());
                }
            }
            for step in &steps {
                println!("  {}", describe_step(step));
            }
            if matches!(plan, Plan::Keep(_)) && steps.is_empty() {
                println!("nothing to do");
            } else if *execute {
                apply_steps(&steps)?;
                apply(&plan, &record)?;
                println!("wrote {}", record.display());
            } else {
                println!("dry run: add --execute to write {}", record.display());
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pool size of the plan tests: any size above 2 works, 3 is the spec's default.
    const POOL: u32 = 3;

    fn holder_at(slot: u32, checkout: &str, live: bool) -> Holder {
        Holder {
            slot,
            checkout: PathBuf::from(checkout),
            record: PathBuf::from(format!("/git/{checkout}/atl-out-slot")),
            live,
        }
    }

    /// A fresh scratch dir for one test; any earlier run's leftovers are removed first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-slot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        dir
    }

    #[test]
    fn resolution_is_explicit_then_slot_then_fallback() {
        let ws = Path::new("/ws");
        let fallback = PathBuf::from("/ws/out/native/x86_64");
        let explicit = resolve(
            Some(PathBuf::from("/elsewhere")),
            Some(2),
            ws,
            fallback.clone(),
        );
        assert_eq!(explicit.dir, PathBuf::from("/elsewhere"));
        assert_eq!(explicit.source, Source::Explicit);

        let slot = resolve(None, Some(2), ws, fallback.clone());
        assert_eq!(slot.dir, PathBuf::from("/ws/out/slot-2"));
        assert_eq!(slot.source, Source::Slot(2));

        let none = resolve(None, None, ws, fallback.clone());
        assert_eq!(none.dir, fallback);
        assert_eq!(none.source, Source::Fallback);
    }

    #[test]
    fn slot_number_must_be_in_the_pool() {
        assert_eq!(parse("1").expect("slot 1"), 1);
        assert_eq!(
            parse("9").expect("above the pool parses; pool() refuses it"),
            9
        );
        assert!(parse("0").is_err());
        assert!(parse("two").is_err());
    }

    fn cfg_with(text: &str) -> WsConfig {
        ws_config::parse(text, Path::new("/ws/workspace.toml")).expect("config")
    }

    #[test]
    fn lowering_slots_under_a_held_slot_is_refused_naming_the_holders() {
        let held = [
            holder_at(1, "wt-a", true),
            holder_at(3, "wt-c", true),
            holder_at(2, "wt-gone", false),
        ];
        let refused = pool(&cfg_with("[build]\nslots = 1\n"), &held);
        let m = format!("{:#}", refused.expect_err("slots 2 and 3 are held"));
        assert!(
            m.contains("/ws/workspace.toml") && m.contains("`build.slots` = 1"),
            "{m}"
        );
        assert!(
            m.contains("slot 3 by wt-c") && m.contains("slot 2 by wt-gone (stale)"),
            "{m}"
        );
        assert!(!m.contains("wt-a"), "{m}");
        assert_eq!(
            pool(&cfg_with("[build]\nslots = 3\n"), &held).expect("fits"),
            3
        );
        // the spec's default pool is 3
        assert_eq!(pool(&cfg_with(""), &held).expect("default"), 3);
    }

    #[test]
    fn assign_follows_the_configured_pool() {
        let record = Path::new("/git/this/atl-out-slot");
        assert!(plan_assign(record, None, &[], Some(4), false, 3).is_err());
        assert_eq!(
            plan_assign(record, None, &[], Some(4), false, 4).expect("pool of 4"),
            Plan::Assign {
                slot: 4,
                release: vec![],
            }
        );
        let one_live = [holder_at(1, "wt-a", true)];
        assert!(plan_assign(record, None, &one_live, None, false, 1).is_err());
    }

    #[test]
    fn held_slot_is_refused_unless_forced() {
        let record = Path::new("/git/this/atl-out-slot");
        let held = [holder_at(1, "wt-a", true)];
        let refused = plan_assign(record, None, &held, Some(1), false, POOL);
        let message = format!("{:#}", refused.expect_err("live holder must refuse"));
        assert!(message.contains("is held by wt-a"), "{message}");
        assert_eq!(
            plan_assign(record, None, &held, Some(1), true, POOL).expect("forced"),
            Plan::Assign {
                slot: 1,
                release: vec![held[0].clone()],
            }
        );
    }

    #[test]
    fn stale_holder_is_reassigned_without_force() {
        let record = Path::new("/git/this/atl-out-slot");
        let held = [holder_at(2, "wt-gone", false)];
        assert_eq!(
            plan_assign(record, None, &held, Some(2), false, POOL).expect("stale"),
            Plan::Assign {
                slot: 2,
                release: vec![held[0].clone()],
            }
        );
    }

    #[test]
    fn automatic_pick_takes_the_first_slot_not_held_live() {
        let record = Path::new("/git/this/atl-out-slot");
        let held = [holder_at(1, "wt-a", true), holder_at(2, "wt-gone", false)];
        assert_eq!(
            plan_assign(record, None, &held, None, false, POOL).expect("pick"),
            Plan::Assign {
                slot: 2,
                release: vec![held[1].clone()],
            }
        );
        let all_live: Vec<Holder> = (1..=POOL)
            .map(|n| holder_at(n, &format!("wt-{n}"), true))
            .collect();
        assert!(plan_assign(record, None, &all_live, None, false, POOL).is_err());
    }

    #[test]
    fn a_checkout_that_holds_the_slot_keeps_it() {
        let record = Path::new("/git/this/atl-out-slot");
        let own = [Holder {
            slot: 2,
            checkout: PathBuf::from("this"),
            record: record.to_owned(),
            live: true,
        }];
        assert_eq!(
            plan_assign(record, Some(2), &own, None, false, POOL).expect("keep"),
            Plan::Keep(2)
        );
        assert_eq!(
            plan_assign(record, Some(2), &own, Some(2), false, POOL).expect("keep"),
            Plan::Keep(2)
        );
    }

    #[test]
    fn holders_reads_the_main_record_and_linked_worktrees() {
        let root = scratch("holders");
        let common = root.join("main/.git");
        let live_wt = root.join("wt-live");
        std::fs::create_dir_all(&live_wt).expect("live worktree");
        std::fs::create_dir_all(&common).expect("common dir");
        std::fs::write(common.join(RECORD), "1\n").expect("main record");
        let live_git = common.join("worktrees/live");
        std::fs::create_dir_all(&live_git).expect("live git dir");
        std::fs::write(
            live_git.join("gitdir"),
            format!("{}/.git\n", live_wt.display()),
        )
        .expect("live gitdir");
        std::fs::write(live_git.join(RECORD), "2\n").expect("live record");
        let gone_git = common.join("worktrees/gone");
        std::fs::create_dir_all(&gone_git).expect("gone git dir");
        std::fs::write(
            gone_git.join("gitdir"),
            format!("{}/.git\n", root.join("wt-gone").display()),
        )
        .expect("gone gitdir");
        std::fs::write(gone_git.join(RECORD), "1\n").expect("gone record");
        let found = holders(&common).expect("holders");
        std::fs::remove_dir_all(&root).expect("cleanup");
        let summary: Vec<(u32, bool)> = found.iter().map(|h| (h.slot, h.live)).collect();
        // main = slot 1 (live), linked live = slot 2, linked gone = slot 1 stale; sorted by slot
        assert_eq!(summary, vec![(1, true), (1, false), (2, true)]);
        assert_eq!(found[2].checkout, live_wt);
    }

    /// A workspace `ws` with `out/`, and its checkout `ws/wt/this`, in a fresh scratch dir. The caller removes `ws`'s parent.
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let ws = scratch(name).join("ws");
        let root = ws.join("wt/this");
        std::fs::create_dir_all(&root).expect("checkout");
        std::fs::create_dir_all(ws.join("out")).expect("out");
        (ws, root)
    }

    fn put(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, "x\n").expect("file");
    }

    fn holder_of(slot: u32, checkout: PathBuf, live: bool) -> Holder {
        let record = checkout.join(RECORD);
        Holder {
            slot,
            checkout,
            record,
            live,
        }
    }

    fn link(to: &Path, at: &Path) {
        std::os::unix::fs::symlink(to, at).expect("symlink");
    }

    #[test]
    fn a_fresh_checkout_links_its_slot_target() {
        let (ws, root) = fixture("link-fresh");
        let steps = target_steps(&root, &ws, 1, None, &[]).expect("steps");
        assert_eq!(
            steps,
            vec![Step::Link {
                at: root.join(TARGET),
                to: slot_target(&ws, 1),
            }]
        );
        apply_steps(&steps).expect("apply");
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(slot_target(&ws, 1))
        );
        assert!(slot_target(&ws, 1).is_dir());
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn an_own_real_target_is_moved_into_an_empty_slot_target() {
        let (ws, root) = fixture("link-adopt");
        put(&root.join(TARGET).join("debug/build.txt"));
        let steps = target_steps(&root, &ws, 1, None, &[]).expect("steps");
        assert_eq!(
            steps,
            vec![
                Step::Adopt {
                    from: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert!(slot_target(&ws, 1).join("debug/build.txt").is_file());
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(slot_target(&ws, 1))
        );
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn an_own_real_target_is_removed_when_the_slot_has_one() {
        let (ws, root) = fixture("link-remove");
        put(&slot_target(&ws, 1).join("kept.txt"));
        put(&root.join(TARGET).join("own.txt"));
        let steps = target_steps(&root, &ws, 1, None, &[]).expect("steps");
        assert_eq!(
            steps,
            vec![
                Step::Remove(root.join(TARGET)),
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert!(slot_target(&ws, 1).join("kept.txt").is_file());
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(slot_target(&ws, 1))
        );
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn a_link_to_another_slot_is_repointed() {
        let (ws, root) = fixture("link-repoint");
        std::fs::create_dir_all(slot_target(&ws, 2)).expect("slot 2");
        link(&slot_target(&ws, 2), &root.join(TARGET));
        let steps = target_steps(&root, &ws, 1, None, &[]).expect("steps");
        assert_eq!(
            steps,
            vec![Step::Link {
                at: root.join(TARGET),
                to: slot_target(&ws, 1),
            }]
        );
        apply_steps(&steps).expect("apply");
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(slot_target(&ws, 1))
        );
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn a_live_displaced_holder_loses_its_link_and_keeps_its_files() {
        let (ws, root) = fixture("link-live");
        let other = ws.join("wt/other");
        std::fs::create_dir_all(&other).expect("other checkout");
        put(&slot_target(&ws, 1).join("building.txt"));
        link(&slot_target(&ws, 1), &other.join(TARGET));
        let held = [holder_of(1, other.clone(), true)];
        let steps = target_steps(&root, &ws, 1, None, &held).expect("steps");
        assert_eq!(
            steps,
            vec![
                Step::Unlink(other.join(TARGET)),
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert_eq!(entry(&other.join(TARGET)).expect("entry"), Entry::Absent);
        assert!(slot_target(&ws, 1).join("building.txt").is_file());
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    /// An executable file at `path` holding `bytes`.
    fn exe(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, bytes).expect("file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn a_live_displaced_holders_path_embedding_executables_are_dropped() {
        let (ws, root) = fixture("link-embed");
        let other = ws.join("wt/other");
        std::fs::create_dir_all(&other).expect("other checkout");
        link(&slot_target(&ws, 1), &other.join(TARGET));
        let baked = format!("CARGO_BIN_EXE={}/target/debug/atl-launch", other.display());
        let deps = slot_target(&ws, 1).join("debug/deps");
        let cross = slot_target(&ws, 1).join("riscv64gc-unknown-linux-gnu/debug/deps");
        // stale: integration-test binaries naming the other checkout, on both deps levels
        exe(&deps.join("launch-0123456789abcdef"), baked.as_bytes());
        exe(&cross.join("launch-0123456789abcdef"), baked.as_bytes());
        // kept: a library with the path (not run), a binary of a sibling path sharing the prefix, a non-executable
        exe(&deps.join("libx-0123456789abcdef.rlib"), baked.as_bytes());
        let sibling = format!("{}-2/target", other.display());
        exe(&deps.join("sib-0123456789abcdef"), sibling.as_bytes());
        put(&deps.join("plain-0123456789abcdef"));
        std::fs::write(deps.join("plain-0123456789abcdef"), &baked).expect("plain");
        let held = [holder_of(1, other.clone(), true)];
        let steps = target_steps(&root, &ws, 1, None, &held).expect("steps");
        let drop = |file: PathBuf| Step::Drop {
            file,
            checkout: other.clone(),
        };
        assert_eq!(
            steps,
            vec![
                Step::Unlink(other.join(TARGET)),
                drop(deps.join("launch-0123456789abcdef")),
                drop(cross.join("launch-0123456789abcdef")),
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert!(!deps.join("launch-0123456789abcdef").exists());
        assert!(deps.join("libx-0123456789abcdef.rlib").is_file());
        assert!(deps.join("sib-0123456789abcdef").is_file());
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn a_stale_displaced_holder_leaves_its_target_to_the_new_one() {
        let (ws, root) = fixture("link-stale");
        put(&slot_target(&ws, 1).join("old.txt"));
        let gone = holder_of(1, ws.join("wt/gone"), false);
        let steps = target_steps(&root, &ws, 1, None, &[gone]).expect("steps");
        assert_eq!(
            steps,
            vec![
                Step::Wipe(slot_target(&ws, 1)),
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 1),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert!(!slot_target(&ws, 1).join("old.txt").exists());
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn moving_to_another_slot_wipes_the_one_left() {
        let (ws, root) = fixture("link-move");
        put(&slot_target(&ws, 1).join("left.txt"));
        link(&slot_target(&ws, 1), &root.join(TARGET));
        let steps = target_steps(&root, &ws, 2, Some(1), &[]).expect("steps");
        assert_eq!(
            steps,
            vec![
                Step::Wipe(slot_target(&ws, 1)),
                Step::Link {
                    at: root.join(TARGET),
                    to: slot_target(&ws, 2),
                },
            ]
        );
        apply_steps(&steps).expect("apply");
        assert!(!slot_target(&ws, 1).exists());
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(slot_target(&ws, 2))
        );
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn land_removes_the_link_and_the_slot_target_it_held() {
        let (ws, root) = fixture("leave-slot");
        put(&slot_target(&ws, 1).join("build.txt"));
        link(&slot_target(&ws, 1), &root.join(TARGET));
        let summary = leave(&root, &ws, Some(1)).expect("leave");
        assert!(summary.starts_with("target removed:"), "{summary}");
        assert_eq!(entry(&root.join(TARGET)).expect("entry"), Entry::Absent);
        assert!(!slot_target(&ws, 1).exists());
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn land_removes_a_real_target_when_the_checkout_has_no_slot() {
        let (ws, root) = fixture("leave-real");
        put(&root.join(TARGET).join("own.txt"));
        let summary = leave(&root, &ws, None).expect("leave");
        assert!(summary.starts_with("target removed:"), "{summary}");
        assert_eq!(entry(&root.join(TARGET)).expect("entry"), Entry::Absent);
        assert_eq!(leave(&root, &ws, None).expect("again"), "no target");
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }

    #[test]
    fn keeping_a_slot_relinks_a_missing_link_and_is_then_a_no_op() {
        let (ws, root) = fixture("link-keep");
        std::fs::create_dir_all(slot_target(&ws, 1)).expect("slot 1");
        let steps = target_steps(&root, &ws, 1, Some(1), &[]).expect("steps");
        assert_eq!(
            steps,
            vec![Step::Link {
                at: root.join(TARGET),
                to: slot_target(&ws, 1),
            }]
        );
        apply_steps(&steps).expect("apply");
        assert!(
            target_steps(&root, &ws, 1, Some(1), &[])
                .expect("steps")
                .is_empty()
        );
        std::fs::remove_dir_all(ws.parent().expect("scratch")).expect("cleanup");
    }
}
