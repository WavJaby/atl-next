// SPDX-License-Identifier: MIT
//! Build-output slots: the out dir a checkout builds into when `--out` / `--java-out` is not given.
//!
//! The pool is `SLOTS` dirs `<workspace>/out/slot-<n>`. A checkout holds at most one slot, recorded as its number in `atl-out-slot` inside its git dir (`.git/worktrees/<name>/` for a linked worktree, `.git/` for the main checkout): the record goes with the worktree, and nothing tracked depends on it.
//! Resolution per flag: explicit flag > the checkout's slot > the old default. The old default prints a note naming `cargo xtask slot assign`, so it is never taken silently.
//! A record that cannot be read as a slot in the pool is an error, never a silent fallback.
//! A slot held by a live other checkout is refused unless `--force`; a record whose checkout directory is gone is stale and may be reassigned.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::git::git;

/// Slots in the pool = checkouts that hold an out dir at once, i.e. agents working in parallel. Disk is the limit: each slot is a whole out dir (~9.3 GB), so one per worktree is too much; 4 fits the dev machine's free disk (~52 GB on the vhdx's host drive, 2026-11-06).
/// Concurrent builds are a separate limit (CPU/RAM), [`crate::build_pool::TOKENS`]: a slot holder mostly edits and tests, and queues only for ninja.
pub const SLOTS: u32 = 4;

/// File in a checkout's git dir holding its slot number.
const RECORD: &str = "atl-out-slot";

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
    /// This checkout's slot and the out dir it builds into.
    Show,
    /// Every slot: the checkout holding it, live or stale.
    List,
    /// Give this checkout a slot (dry run unless --execute).
    ///
    /// Without N: the lowest slot no live checkout holds. A slot held by a live other checkout is refused unless --force, which takes it over; that checkout then builds into the old default.
    Assign {
        /// Slot number, 1..=SLOTS.
        slot: Option<u32>,
        /// Write the record (default: print the plan only).
        #[arg(long)]
        execute: bool,
        /// Take the slot over from a live checkout.
        #[arg(long)]
        force: bool,
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
    let fallback = fallback(&workspace);
    let target = resolve(explicit, slot, &workspace, fallback);
    if target.source == Source::Fallback {
        eprintln!(
            "{flag}: this checkout has no slot, using the old default {} (`cargo xtask slot assign` gives it one)",
            target.dir.display()
        );
    }
    Ok(target.dir)
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

fn parse(text: &str) -> Result<u32> {
    let n: u32 = text
        .parse()
        .with_context(|| format!("{text:?} is not a slot number"))?;
    ensure!((1..=SLOTS).contains(&n), "slot {n} is outside 1..={SLOTS}");
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

fn holder(slot: u32, checkout: PathBuf, record: PathBuf) -> Holder {
    let live = checkout.exists();
    Holder {
        slot,
        checkout,
        record,
        live,
    }
}

/// Plan for giving the checkout whose record is `record` slot `want` (None: the lowest slot no live other checkout holds).
/// A slot held by a live other checkout is refused unless `force`; stale holders are released without it.
pub fn plan_assign(
    record: &Path,
    current: Option<u32>,
    holders: &[Holder],
    want: Option<u32>,
    force: bool,
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
            ensure!((1..=SLOTS).contains(&n), "slot {n} is outside 1..={SLOTS}");
            n
        }
        None => {
            if let Some(n) = current {
                return Ok(Plan::Keep(n));
            }
            match (1..=SLOTS).find(|&n| others(n).iter().all(|h| !h.live)) {
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

pub fn run(root: &Path, cmd: &Slot) -> Result<ExitCode> {
    let layout = Layout::of(root)?;
    let ws = layout.workspace();
    let record = record_of(root)?;
    let common = PathBuf::from(git(
        Some(root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    match cmd {
        Slot::Show => match read_record(&record)? {
            Some(n) => {
                let dir = slot_dir(ws, n);
                let built = if dir.is_dir() {
                    "exists"
                } else {
                    "not built yet"
                };
                println!("slot {n}: {} ({built})", dir.display());
            }
            None => println!(
                "no slot: --out and --java-out fall back to the old defaults; `cargo xtask slot assign` gives this checkout one"
            ),
        },
        Slot::List => {
            let all = holders(&common)?;
            for n in 1..=SLOTS {
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
            let all = holders(&common)?;
            let current = read_record(&record)?;
            let plan = plan_assign(&record, current, &all, *slot, *force)?;
            match &plan {
                Plan::Keep(n) => println!("slot {n}: already held by this checkout, nothing to do"),
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
                    if *execute {
                        apply(&plan, &record)?;
                        println!("wrote {}", record.display());
                    } else {
                        println!("dry run: add --execute to write {}", record.display());
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(parse("0").is_err());
        assert!(parse(&(SLOTS + 1).to_string()).is_err());
        assert!(parse("two").is_err());
    }

    #[test]
    fn held_slot_is_refused_unless_forced() {
        let record = Path::new("/git/this/atl-out-slot");
        let held = [holder_at(1, "wt-a", true)];
        let refused = plan_assign(record, None, &held, Some(1), false);
        let message = format!("{:#}", refused.expect_err("live holder must refuse"));
        assert!(message.contains("is held by wt-a"), "{message}");
        assert_eq!(
            plan_assign(record, None, &held, Some(1), true).expect("forced"),
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
            plan_assign(record, None, &held, Some(2), false).expect("stale"),
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
            plan_assign(record, None, &held, None, false).expect("pick"),
            Plan::Assign {
                slot: 2,
                release: vec![held[1].clone()],
            }
        );
        let all_live: Vec<Holder> = (1..=SLOTS)
            .map(|n| holder_at(n, &format!("wt-{n}"), true))
            .collect();
        assert!(plan_assign(record, None, &all_live, None, false).is_err());
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
            plan_assign(record, Some(2), &own, None, false).expect("keep"),
            Plan::Keep(2)
        );
        assert_eq!(
            plan_assign(record, Some(2), &own, Some(2), false).expect("keep"),
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
}
