// SPDX-License-Identifier: MIT
//! Build dirs: the out dir a checkout builds into when `--out` / `--java-out` is not given.
//!
//! Pool `<workspace>/out/pool` (one copy-on-write filesystem where the host has one): `ws/<id>` per checkout -- `repo` for the main checkout, the git worktree name otherwise (`.git/worktrees/<name>`), so no record file exists -- and `base/<commit>`, main's published builds: copies of `ws/land` taken by land after a green verify (`land::LAND_WT`, [`publish`]).
//! The first command that needs a checkout's dir makes it ([`ensure`]): a copy of the newest base whose commit is an ancestor of HEAD (else the newest base, else an empty dir), into a temp name renamed into place, so a half copy is never a build dir. Its `.base` names the base.
//! The copy is one interface with two capability tiers (probed per copy, never by host name): `reflink` (`cp --reflink=always`, extents shared, ~2-3 s for main's 100k-entry build on btrfs) and the baseline `copy` (`cp -a`, any filesystem). The create line names the tier; `pool/tier-<name>.count` counts each.
//! Every build sees its dir at [`CANONICAL`] (the build namespace binds it there), so a base copy stays valid at its new real path.
//! A process that resolves the dir holds its lock shared until it exits (`build_pool::hold_dir`); create, delete and prune take it exclusive, so a dir is never deleted under a build.
//! The checkout's `target/` is a symlink to its dir's `target/` (the only per-checkout switch cargo reads with no tracked file or env var), settled when the outermost process ends ([`settle_target`]): the xtask binary running from the previous `target/` must outlive the swap.
//! Before that, from `git worktree add` on, it links to `<pool>/boot/<id>` (the post-checkout hook [`install_hook`] writes on every xtask run), so cargo's bootstrap build of xtask lands on the pool; settle moves or removes it. A real `target/` happens only for a worktree made before the hook; settle removes it and counts it (`pool/target-real.count`, else `target-boot.count`).
//! A dir leaves with its checkout (land `Cleanup`, [`leave`]), together with the checkout's run dir (out_run.rs, same id and lock); `out prune` deletes the build and run dirs of checkouts removed outside xtask and every base but the newest [`KEEP_BASES`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Mutex;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool::{self, DirLock};
use crate::git::git;
use crate::out_prune::{self, Outcome};
use crate::red_logs;
use crate::unit_use;

/// The pool root, workspace-relative.
pub const POOL: &str = "out/pool";
/// The main checkout's id; a worktree of that name is refused.
pub const MAIN_ID: &str = "repo";
/// The land worktree's id (`land::LAND_WT`): the one build dir published as a base.
pub const LAND_ID: &str = "land";
/// Bases `out prune` keeps: the newest, plus the one a child may still be copying from.
pub const KEEP_BASES: usize = 2;
/// A checkout's cargo target dir: a link to its build dir's `target/`.
const TARGET: &str = "target";
/// In a build dir: the commit of the base it was copied from.
pub(crate) const BASE_FILE: &str = ".base";
/// A build dir's bootstrap-gate target (bootstrap_gate.rs). Its cargo units key on the checkout path: refresh keeps it (out_prune::DIR_ROOTS), publish leaves it out.
pub const BOOTSTRAP_DIR: &str = "bootstrap";
/// Driver probe dirs `<build dir>/<family>` (harness vk_driver.rs, egl_driver.rs): cargo outputs of the probe and driver, regenerable, so they live and go with the checkout's build dir (ram tier for a child, shared from the base by reflink) instead of the root disk.
pub const FAMILIES: [&str; 2] = ["vk-driver", "egl-driver"];

/// The checkout at `root`'s dir of driver probe `family` ([`FAMILIES`]), inside its build dir.
pub fn family_dir(root: &Path, family: &str) -> Result<PathBuf> {
    Ok(dir_of(Layout::of(root)?.workspace(), &checkout_id(root)?).join(family))
}

/// `out` subcommands.
#[derive(clap::Subcommand)]
pub enum Out {
    /// This checkout's build dir, its base, its `target/` link, its run dir, and the pool: free space, bases, every build dir.
    Show {
        /// Print only this checkout's run dir (out_run.rs: app data roots and bulky regenerable run data, never cited by a report), making it when missing.
        #[arg(long)]
        run: bool,
    },
    /// Deletes the build dirs and run dirs whose checkout is gone, every base but the newest two, and each live checkout's kept gate logs past the newest runs per bucket (red_logs.rs) (dry run unless --execute).
    ///
    /// A dir a build or another process holds is skipped with a line saying so. With --tasks: instead, the task dirs `out/<task>/` whose worktree, unmerged branch and processes are gone (out_tasks.rs: classes keep / live / orphan / unknown; only orphans are removed). With --units: instead, the cargo units and incremental sessions no build used for --older-than days in every build dir plus a real `repo/target`, which no build dir owns; use is what each recorded build wrote (`<tree>/.atl-unit-use.json`), a unit with no entry is judged by its fingerprint dir's mtime.
    Prune {
        /// Prune cargo units and incremental sessions instead of dirs.
        #[arg(long)]
        units: bool,
        /// Prune orphan task dirs at the top of `out/` instead of build dirs.
        #[arg(long, conflicts_with = "units")]
        tasks: bool,
        /// With --units: stale after this many days since the last build that used the unit.
        #[arg(long, value_name = "DAYS", default_value_t = out_prune::PRUNE_AFTER_DAYS)]
        older_than: u64,
        /// Delete (default: print the plan only).
        #[arg(long)]
        execute: bool,
    },
    /// Remakes this checkout's build dir on delta tier `ram` or `pool` (delta.rs), dropping the current dir (dry run unless --execute).
    ///
    /// Build commands choose the tier when they make a dir: `ram` for a child when `delta.ram_store` is RAM-backed and its build class fits, else `pool`; a dir keeps its tier after that. `pool` is the baseline. Refused while a command of the checkout runs.
    Tier {
        /// `ram` or `pool`.
        #[arg(value_parser = ["ram", "pool"])]
        tier: String,
        /// Drop and remake (default: print the plan only).
        #[arg(long)]
        execute: bool,
    },
}

/// The copy tiers of [`copy_tree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Reflink,
    Copy,
}

impl Tier {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tier::Reflink => "reflink",
            Tier::Copy => "copy",
        }
    }
}

pub fn pool(ws: &Path) -> PathBuf {
    ws.join(POOL)
}

/// The build dir of checkout `id`.
pub fn dir_of(ws: &Path, id: &str) -> PathBuf {
    pool(ws).join("ws").join(id)
}

/// The workspace of a build dir path ([`dir_of`]'s inverse).
fn workspace_of(dir: &Path) -> Option<&Path> {
    dir.ancestors().nth(4)
}

/// The workspace whose build-dir pool holds `dst`, when it does: only a copy into the pool writes the pool's metadata.
fn pool_ws(dst: &Path) -> Option<&Path> {
    let ws = workspace_of(dst)?;
    dst.starts_with(pool(ws)).then_some(ws)
}

pub fn base_of(ws: &Path, commit: &str) -> PathBuf {
    pool(ws).join("base").join(commit)
}

/// The id of the checkout at `root`: [`MAIN_ID`] for the main checkout, else its worktree name.
pub fn checkout_id(root: &Path) -> Result<String> {
    let git_dir = git(Some(root), &["rev-parse", "--absolute-git-dir"])?;
    let common = git(
        Some(root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    id_of(Path::new(&git_dir), Path::new(&common))
}

fn id_of(git_dir: &Path, common: &Path) -> Result<String> {
    if git_dir == common {
        return Ok(MAIN_ID.to_owned());
    }
    let name = git_dir
        .strip_prefix(common.join("worktrees"))
        .ok()
        .and_then(|rest| rest.to_str())
        .filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('.'))
        .with_context(|| {
            format!(
                "{} is neither {} nor a worktree dir under it",
                git_dir.display(),
                common.display()
            )
        })?;
    ensure!(
        name != MAIN_ID,
        "worktree name {MAIN_ID:?} is the main checkout's build dir id: rename the worktree (`git worktree move`)"
    );
    Ok(name.to_owned())
}

/// The ids of every checkout of the repository whose git common dir is `common`: the main checkout and each linked worktree whose checkout dir exists.
fn live_ids(common: &Path) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::from([MAIN_ID.to_owned()]);
    let worktrees = common.join("worktrees");
    if worktrees.is_dir() {
        for entry in std::fs::read_dir(&worktrees)
            .with_context(|| format!("reading {}", worktrees.display()))?
        {
            let git_dir = entry?.path();
            let gitfile = git_dir.join("gitdir");
            let Ok(text) = std::fs::read_to_string(&gitfile) else {
                continue;
            };
            // `gitdir` names the worktree's `.git` file; its parent is the checkout
            if Path::new(text.trim()).parent().is_some_and(Path::exists)
                && let Some(name) = git_dir.file_name().and_then(|n| n.to_str())
            {
                ids.insert(name.to_owned());
            }
        }
    }
    Ok(ids)
}

/// The out dir for `--out` / `--java-out` of the checkout at `root`: the explicit dir, else the checkout's build dir (made if missing), held shared until exit. Inside the build namespace that is [`CANONICAL`].
pub fn out_dir(root: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(std::path::absolute(dir)?);
    }
    let ws = Layout::of(root)?.workspace().to_owned();
    let (_, dir) = ensure_held(root)?;
    if crate::tree::bound(CANONICAL).as_deref() == Some(dir.as_path()) {
        return Ok(ws.join(CANONICAL));
    }
    Ok(dir)
}

/// The checkout's id and build dir, made when missing: copied from a base, else empty; a copy gives the checkout the base's mtimes where its content is the base's (`checkout_stamps`). Prints one line when it makes one.
pub fn ensure(root: &Path) -> Result<(String, PathBuf)> {
    make(root, true)
}

/// [`ensure`]; `restore` = [`restore_stamps`] after a base copy. False when outputs the base did not build will be carried into the dir (`checkout_stamps` module doc).
fn make(root: &Path, restore: bool) -> Result<(String, PathBuf)> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = checkout_id(root)?;
    let dir = dir_of(&ws, &id);
    if present(&dir)? {
        unmounted_ram(&dir, &id)?;
        return Ok((id, dir));
    }
    let _lock = build_pool::lock_dir(&ws, &id, || {
        eprintln!("out: ws/{id} is being made or deleted by another process, waiting")
    })?;
    if present(&dir)? {
        unmounted_ram(&dir, &id)?;
        return Ok((id, dir));
    }
    let parent = dir.parent().context("build dir has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let pick = pick_base(root, &ws)?;
    let line = match pick.as_ref() {
        Some(c) => {
            let tier = copy_tree(&base_of(&ws, c), &dir, Some(c), &[])?;
            count_tier(&ws, tier);
            let stamps = if restore {
                restore_stamps(root, &base_of(&ws, c))?
            } else {
                "mtimes kept: outputs carried in".to_owned()
            };
            // land's own dir keeps its units: their paths are its own
            let forgot = if id == LAND_ID {
                0
            } else {
                forget_runs_reading(&dir, &ws.join(crate::land::LAND_WT))?
            };
            format!(
                "out: ws/{id} from base/{c} ({}); {stamps}; {forgot} test/bin unit(s) reading the land checkout forgotten",
                tier.name()
            )
        }
        None => {
            std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
            format!(
                "out: ws/{id} new and empty: no base under {} (a build from zero)",
                pool(&ws).join("base").display()
            )
        }
    };
    println!("{line}");
    Ok((id, dir))
}

/// The base a new dir of the checkout at `root` starts from: the newest base that is an ancestor of HEAD, else the newest; None without bases.
pub(crate) fn pick_base(root: &Path, ws: &Path) -> Result<Option<String>> {
    let bases = bases(ws)?;
    let pick = bases
        .iter()
        .find(|c| git(Some(root), &["merge-base", "--is-ancestor", c, "HEAD"]).is_ok())
        .or(bases.first());
    Ok(pick.cloned())
}

/// Refuses a ram dir (delta.rs) seen outside its namespace: there it is the empty mount point, and a build would write the pool under the overlay's place.
fn unmounted_ram(dir: &Path, id: &str) -> Result<()> {
    ensure!(
        crate::delta::stage().is_some() || !dir.join(crate::delta::RECORD).exists(),
        "ws/{id} is a ram delta (delta.rs): reachable only inside its namespace, which build commands enter; this command runs outside it"
    );
    Ok(())
}

/// Deletes, in the cargo roots of build dir `dir` (out_prune::DIR_ROOTS), the fingerprint of every test or bin unit whose rustc dep-info records an `env!` value under `builder` (the checkout the base was built in), so cargo rebuilds it. Returns how many.
/// Why: a unit's key leaves out the checkout path, so a base's units are fresh in any checkout; an executable reading `env!("CARGO_MANIFEST_DIR" | "OUT_DIR" | "CARGO_TARGET_TMPDIR")` at run time then reads the land checkout and `ws/land` (2026-11-17: atl-bionic tests/shape.rs failed 3 tests while ws/land was remade).
/// Executables only: they are leaves (rebuilding one rebuilds nothing else); libs take those values at compile time (`include!`) from outputs equal to the copy's. A lib reading one at run time would need adding here.
/// Retired when cargo runs at one canonical checkout and target path, as native's out dir already does ([`CANONICAL`]): base units are then valid as they are.
pub(crate) fn forget_runs_reading(dir: &Path, builder: &Path) -> Result<usize> {
    let needle = format!("={}/", builder.display());
    let mut n = 0;
    for root in out_prune::DIR_ROOTS {
        for fp in fingerprint_dirs(&dir.join(root))? {
            if reads_builder(&fp, &needle)? {
                std::fs::remove_dir_all(&fp)
                    .with_context(|| format!("removing {}", fp.display()))?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Whether cargo unit dir `fp` (`<profile>/.fingerprint/<pkg>-<hash>`) is a test or bin whose dep-info (`<profile>/deps/<target>-<hash>.d`) has an `# env-dep:` line containing `needle`.
fn reads_builder(fp: &Path, needle: &str) -> Result<bool> {
    let Some(hash) = fp
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit_once('-'))
        .map(|(_, h)| format!("-{h}"))
    else {
        return Ok(false);
    };
    let executable = std::fs::read_dir(fp)?.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|s| s.starts_with("test-") || s.starts_with("bin-"))
    });
    let Some(deps) = fp.parent().and_then(Path::parent).map(|p| p.join("deps")) else {
        return Ok(false);
    };
    if !executable || !deps.is_dir() {
        return Ok(false);
    }
    for e in std::fs::read_dir(&deps)?.flatten() {
        let p = e.path();
        let ours = p.extension().is_some_and(|x| x == "d")
            && p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(&hash));
        if ours
            && std::fs::read_to_string(&p).is_ok_and(|t| {
                t.lines()
                    .any(|l| l.starts_with("# env-dep:") && l.contains(needle))
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The unit dirs of every `.fingerprint` under cargo target dir `target`: `<profile>/.fingerprint/*` and `<triple>/<profile>/.fingerprint/*`.
fn fingerprint_dirs(target: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut walk = vec![(target.to_owned(), 0)];
    while let Some((d, depth)) = walk.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            if !e.file_type()?.is_dir() {
                continue;
            }
            let p = e.path();
            if e.file_name() == ".fingerprint" {
                found.extend(std::fs::read_dir(&p)?.flatten().map(|u| u.path()));
            } else if depth < 2 {
                walk.push((p, depth + 1));
            }
        }
    }
    Ok(found)
}

/// The checkout at `root` gets `base`'s recorded mtimes where its content is the recorded one, now elsewhere. Returns the line part. A base published before stamps existed restores nothing: every path gets now.
pub(crate) fn restore_stamps(root: &Path, base: &Path) -> Result<String> {
    let file = base.join(crate::checkout_stamps::FILE);
    let Ok(text) = std::fs::read_to_string(&file) else {
        crate::checkout_stamps::restore(root, "")?;
        return Ok(format!(
            "no {}: mtimes set to now",
            crate::checkout_stamps::FILE
        ));
    };
    let (n, m) = crate::checkout_stamps::restore(root, &text)?;
    Ok(format!("restored {n} of {m} mtimes"))
}

/// [`ensure`], then a shared hold until [`after_build`]: refresh, leave and prune skip or wait for the dir meanwhile. For a build that writes its own tree there.
pub fn ensure_held(root: &Path) -> Result<(String, PathBuf)> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let (id, dir) = ensure(root)?;
    hold(&ws, &id)?;
    Ok((id, dir))
}

/// Whether `path` exists as a dir or a link; a link is not followed.
pub(crate) fn present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Base commits, newest first (by the base dir's mtime: publish order). Temp names (`.`-prefixed) are skipped.
fn bases(ws: &Path) -> Result<Vec<String>> {
    let dir = pool(ws).join("base");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type()?.is_dir() {
            continue;
        }
        found.push((e.metadata()?.modified()?, name));
    }
    found.sort();
    Ok(found.into_iter().rev().map(|(_, n)| n).collect())
}

/// Which tier copies `src` into `dst_parent`: a regular file of `src` reflinked into `dst_parent` succeeds -> `Reflink`, else `Copy`. Probes the real pair, not the host: a source off the pool's filesystem is a `Copy`.
/// Reads `src` only when it holds a top-level file: a write there would bump a base's mtime, the order [`bases`] ranks by, so the base just copied would rank newest. An empty `src` gets a probe file of its own (nothing to rank yet).
pub fn probe(src: &Path, dst_parent: &Path) -> Result<Tier> {
    let pid = std::process::id();
    let to = dst_parent.join(format!(".atl-reflink-probe-{pid}"));
    let existing = std::fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))?
        .filter_map(Result::ok)
        .find(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path());
    let (from, made) = match existing {
        Some(f) => (f, false),
        None => {
            let f = src.join(format!(".atl-reflink-probe-{pid}"));
            std::fs::write(&f, b"x").with_context(|| format!("writing {}", f.display()))?;
            (f, true)
        }
    };
    let ok = Command::new("cp")
        .arg("--reflink=always")
        .arg(&from)
        .arg(&to)
        .stderr(std::process::Stdio::null())
        .status()
        .context("running cp")?
        .success();
    if made {
        let _ = std::fs::remove_file(&from);
    }
    let _ = std::fs::remove_file(&to);
    Ok(if ok { Tier::Reflink } else { Tier::Copy })
}

/// Copies dir `src` to the absent `dst` with the probed tier, through a temp name renamed into place; `base` (when given) goes to `dst/.base`; top-level entries of `src` named in `skip` are left out. Returns the tier.
pub fn copy_tree(src: &Path, dst: &Path, base: Option<&str>, skip: &[&str]) -> Result<Tier> {
    // outside its namespace a ram dir is its empty mount point: a copy of it would be a base holding only the record
    ensure!(
        !src.join(crate::delta::RECORD).exists(),
        "{} is a ram dir's mount point (delta.rs): its content is lower + upper, never this dir",
        src.display()
    );
    if let Some(ws) = pool_ws(dst) {
        crate::disk_floor::pool_gate(ws, &crate::ws_config::load(ws)?)?;
    }
    let parent = dst.parent().context("copy target has no parent")?;
    let name = dst
        .file_name()
        .context("copy target has no name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    if present(&tmp)? {
        std::fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    let tier = probe(src, parent)?;
    let mut entries = Vec::new();
    for e in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let e = e?;
        if !e.file_name().to_str().is_some_and(|n| skip.contains(&n)) {
            entries.push(e.path());
        }
    }
    std::fs::create_dir(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    if !entries.is_empty() {
        let mut cp = Command::new("cp");
        cp.arg("-a");
        if tier == Tier::Reflink {
            cp.arg("--reflink=always");
        }
        let status = cp
            .arg("-t")
            .arg(&tmp)
            .arg("--")
            .args(&entries)
            .status()
            .context("running cp")?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            bail!(
                "cp -a ({}) {} -> {} failed: {status}",
                tier.name(),
                src.display(),
                tmp.display()
            );
        }
    }
    // the top dir keeps src's mode and mtime, as `cp -a -T` gave it: `bases` ranks by mtime
    let meta = std::fs::metadata(src).with_context(|| format!("reading {}", src.display()))?;
    std::fs::set_permissions(&tmp, meta.permissions())
        .with_context(|| format!("setting the mode of {}", tmp.display()))?;
    let mtime = meta
        .modified()
        .with_context(|| format!("reading the mtime of {}", src.display()))?;
    std::fs::File::open(&tmp)
        .and_then(|d| d.set_modified(mtime))
        .with_context(|| format!("setting the mtime of {}", tmp.display()))?;
    let mark = tmp.join(BASE_FILE);
    match base {
        Some(c) => std::fs::write(&mark, format!("{c}\n"))
            .with_context(|| format!("writing {}", mark.display()))?,
        None => {
            let _ = std::fs::remove_file(&mark);
        }
    }
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;
    Ok(tier)
}

/// Adds one to `pool/tier-<name>.count`. Read-modify-write without a lock: two copies ending at once may count one; the count is a tier-usage signal, not an invariant.
fn count_tier(ws: &Path, tier: Tier) {
    count(ws, &format!("tier-{}", tier.name()));
}

/// Adds one to counter `<pool>/<name>.count`.
pub(crate) fn count(ws: &Path, name: &str) {
    let file = pool(ws).join(format!("{name}.count"));
    let n: u64 = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    if let Err(e) = std::fs::write(&file, format!("{}\n", n + 1)) {
        eprintln!("out: count {}: {e}", file.display());
    }
}

/// The one path every build sees its out dir at (workspace-relative): the build namespace binds the checkout's build dir over it ([`build_bind`]), so build.ninja, CMake caches, dep files and DWARF name it in every build dir and a copy stays valid at another real path.
/// Outside every namespace it is an empty dir, mode 0555 (`tree::enter` makes it): a tool following a logged path there fails loudly instead of reading or writing another checkout's build.
pub const CANONICAL: &str = atl_layout::BUILD;

/// The build namespace's bind of the checkout's build dir (made if missing) over [`CANONICAL`].
pub fn build_bind(root: &Path) -> Result<crate::tree::Overlay> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let (_, dir) = ensure(root)?;
    Ok(crate::tree::Overlay {
        project: CANONICAL.into(),
        copy: dir,
        target: ws.join(CANONICAL),
    })
}

/// The bound build dir and its workspace; None outside a namespace.
fn bound() -> Option<(PathBuf, PathBuf)> {
    let dir = crate::tree::bound(CANONICAL)?;
    let ws = workspace_of(&dir)?.to_owned();
    Some((ws, dir))
}

/// `path` below the bound build dir (as bound, or resolved: the `target` link and `/proc/self/exe` name the resolved path) shown at the canonical path instead, for paths a build output records. Other paths, and any path outside a namespace, unchanged.
pub fn canonical(path: &Path) -> PathBuf {
    let Some((ws, dir)) = bound() else {
        return path.to_owned();
    };
    let resolved = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    match path
        .strip_prefix(&dir)
        .or_else(|_| path.strip_prefix(&resolved))
    {
        Ok(rest) => ws.join(CANONICAL).join(rest),
        Err(_) => path.to_owned(),
    }
}

/// `dir` with the canonical prefix translated back to the bound build dir, for names that must differ per checkout outside the namespace (remote run dirs); other paths, and any path outside a namespace, unchanged.
pub fn real(dir: &Path) -> PathBuf {
    let Some((ws, bound)) = bound() else {
        return dir.to_owned();
    };
    match dir.strip_prefix(ws.join(CANONICAL)) {
        Ok(rest) if rest.as_os_str().is_empty() => bound,
        Ok(rest) => bound.join(rest),
        Err(_) => dir.to_owned(),
    }
}

/// The first real build dir path (`<ws>/out/pool/...`) that `text` names, inside a namespace; None outside, or when `text` names only [`CANONICAL`]. A cache naming one was configured at that real path and would write there.
pub fn foreign_build_dir(text: &str) -> Option<String> {
    let (ws, _) = bound()?;
    let out = format!("{}/out/", ws.display());
    text.match_indices(&out).find_map(|(at, _)| {
        let rest = &text[at + out.len()..];
        let seg = rest.split(['/', ';', '\n', ' ', '"']).next()?;
        (seg == "pool").then(|| format!("{out}{seg}"))
    })
}

/// Dir locks this process holds shared, one per (workspace, id); released at exit or by [`after_build`].
static HELD: Mutex<Vec<(PathBuf, String, DirLock)>> = Mutex::new(Vec::new());

fn hold(ws: &Path, id: &str) -> Result<()> {
    let mut held = HELD
        .lock()
        .map_err(|_| anyhow::anyhow!("dir lock table poisoned"))?;
    if held.iter().all(|(w, m, _)| w != ws || m != id) {
        let lock = build_pool::hold_dir(ws, id, || {
            eprintln!("out: ws/{id} is being pruned, waiting")
        })?;
        held.push((ws.to_owned(), id.to_owned(), lock));
    }
    Ok(())
}

/// Drops this process's dir locks under workspace `ws` and returns their ids; other workspaces' holds stay.
fn release_holds(ws: &Path) -> Result<BTreeSet<String>> {
    let mut held = HELD
        .lock()
        .map_err(|_| anyhow::anyhow!("dir lock table poisoned"))?;
    let mut ids = BTreeSet::new();
    held.retain(|(w, m, _)| {
        let mine = w == ws;
        if mine {
            ids.insert(m.clone());
        }
        !mine
    });
    Ok(ids)
}

/// The end of a checkout's build (`native`, `check`, `cross`, the harness commands): the stale incremental sessions of the trees it wrote go, one line per build dir. `result` passes through unless the prune fails.
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

/// Prunes the sessions of the dirs this process held and of the checkout's own.
fn finish_build(root: &Path) -> Result<()> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let now = unit_use::now()?;
    // a shared hold blocks the prune, so the dirs this process resolved are released first
    let mut ids = release_holds(&ws)?;
    ids.insert(checkout_id(root)?);
    for id in ids {
        let line = match out_prune::prune_sessions(&ws, &id, now)? {
            Outcome::NotBuilt => format!("session prune: ws/{id}: not built"),
            Outcome::Busy => {
                format!("session prune: ws/{id}: busy, skipped: another process holds it")
            }
            Outcome::Done(plans) => out_prune::sessions_line(&format!("ws/{id}"), &plans),
        };
        println!("{line}");
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

/// One change to a checkout's `target/`. `link_steps` plans them, `apply_steps` runs them in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Create the build dir's target dir.
    Mkdir(PathBuf),
    /// Move a checkout's own real `target/` into an absent build-dir target, keeping its build.
    Adopt { from: PathBuf, to: PathBuf },
    /// Remove a checkout's own real `target/` on another filesystem than its build dir: rename cannot cross (EXDEV), and cargo's rebuild costs less than a 10-30 GB copy.
    Discard(PathBuf),
    /// Remove a checkout's own real `target/`: its build dir has a target already (copied from the base).
    Remove(PathBuf),
    /// Make `at` a link to `to`, creating `to`; replaces a link at `at`.
    Link { at: PathBuf, to: PathBuf },
}

/// The target `<checkout>/target` links to: the resolved build dir's `target/`.
fn target_of(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_owned())
        .join(TARGET)
}

/// The boot target of build-dir target `want` (`<pool>/ws/<id>/target` -> `<pool>/boot/<id>`): where the [`HOOK`] links a new worktree's `target/` until its build dir exists, so the first `cargo xtask` builds xtask on the pool, never in the checkout.
fn boot_of(want: &Path) -> Option<PathBuf> {
    let id = want.parent()?.file_name()?;
    Some(want.ancestors().nth(3)?.join(BOOT).join(id))
}

/// Steps that make `link` (`<checkout>/target`) a link to `want`: none when it is one already (a missing `want` is created), else a link, after a real dir or the boot target ([`boot_of`]) is moved into an absent `want` or removed.
pub fn link_steps(link: &Path, want: &Path) -> Result<Vec<Step>> {
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
        // the boot dir is on the pool like `want`: rename works
        Entry::Link(to) if Some(&to) == boot_of(want).as_ref() && to.is_dir() => {
            if want.exists() {
                vec![Step::Remove(to), relink]
            } else {
                vec![
                    Step::Adopt {
                        from: to,
                        to: want.to_owned(),
                    },
                    relink,
                ]
            }
        }
        Entry::Absent | Entry::Link(_) => vec![relink],
        Entry::Dir if want.exists() => vec![Step::Remove(link.to_owned()), relink],
        Entry::Dir if !same_filesystem(link, want)? => vec![Step::Discard(link.to_owned()), relink],
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

/// Whether `dir` and `want` (missing: its nearest existing ancestor, where it will be created) are on one filesystem, i.e. a rename between them works.
fn same_filesystem(dir: &Path, want: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let dev = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.dev())
            .with_context(|| format!("reading {}", p.display()))
    };
    let existing = want
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"));
    Ok(dev(dir)? == dev(existing)?)
}

/// Runs `steps` in order; the only place they change anything.
pub fn apply_steps(steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Remove(p) | Step::Discard(p) => {
                std::fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))?;
            }
            Step::Mkdir(p) => {
                std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
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

pub fn describe_step(step: &Step) -> String {
    match step {
        Step::Mkdir(p) => format!("create {}", p.display()),
        Step::Adopt { from, to } => {
            format!(
                "move {} -> {} (keeps its build)",
                from.display(),
                to.display()
            )
        }
        Step::Remove(p) => format!("remove {} (the build dir has a target)", p.display()),
        Step::Discard(p) => format!(
            "remove {} (on another filesystem than the build dir: cannot be moved; cargo rebuilds it)",
            p.display()
        ),
        Step::Link { at, to } => format!("link {} -> {}", at.display(), to.display()),
    }
}

/// The build dir's target while `<checkout>/target` is not yet the link [`settle_target`] makes (a new worktree's first command: `cargo xtask` compiled xtask into a real `target/`); None once linked.
/// The caller hands it to the command's cargo runs as `CARGO_TARGET_DIR`: without it the first command's whole build lands in the real dir on the checkout's disk and is discarded at settle.
pub fn pending_target(root: &Path) -> Result<Option<PathBuf>> {
    let (_, dir) = ensure(root)?;
    let want = target_of(&dir);
    Ok(if link_steps(&root.join(TARGET), &want)?.is_empty() {
        None
    } else {
        Some(want)
    })
}

/// The target dir this process's cargo runs write: `CARGO_TARGET_DIR` ([`pending_target`] sets it), else `<checkout>/target`. For tools placed beside cargo's units (bp2ninja).
pub fn cargo_target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join(TARGET), PathBuf::from)
}

/// Links the checkout's `target/` to its build dir's, when the dir exists. Run by the outermost xtask process as it ends (cli): the running binary may live in the real `target/` this removes, and the namespace re-exec still needs it.
/// Only while no other xtask process holds the dir (its exclusive lock, not waited for): a concurrent build may run its xtask from that `target/` (2026-11-15: `out prune` beside a `native` removed the running binary's dir). Busy: left for a later command.
/// A ram dir (delta.rs) keeps the link at its boot target: cargo's bootstrap of xtask runs outside the namespace, where the dir is an empty mount point; its builds get the dir's target as `CARGO_TARGET_DIR` ([`pending_target`]).
pub fn settle_target(root: &Path) -> Result<()> {
    // the outermost process settles: inside a namespace the ram record is covered by the overlay
    if std::env::var_os(crate::tree::ENV).is_some() || crate::delta::stage().is_some() {
        return Ok(());
    }
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = checkout_id(root)?;
    let dir = dir_of(&ws, &id);
    if !present(&dir)? {
        return Ok(());
    }
    let want = if dir.join(crate::delta::RECORD).exists() {
        boot_of(&target_of(&dir)).context("build dir path has no boot target")?
    } else {
        target_of(&dir)
    };
    if link_steps(&root.join(TARGET), &want)?.is_empty() {
        return Ok(());
    }
    release_holds(&ws)?;
    let Some(_lock) = build_pool::try_lock_dir(&ws, &id)? else {
        return Ok(());
    };
    let steps = link_steps(&root.join(TARGET), &want)?;
    for step in &steps {
        println!("out: target: {}", describe_step(step));
    }
    // which route the first build took: `boot` = on the pool via the hook; `real` = a real dir in the checkout (worktree made before the hook was installed, or its `target` removed by hand)
    let link = root.join(TARGET);
    let real = steps.iter().any(|s| match s {
        Step::Remove(p) | Step::Discard(p) => *p == link,
        Step::Adopt { from, .. } => *from == link,
        _ => false,
    });
    let boot = steps.iter().any(|s| match s {
        Step::Remove(p) => *p != link,
        Step::Adopt { from, .. } => *from != link,
        _ => false,
    });
    if real {
        count(&ws, "target-real");
        println!(
            "out: target: the first build wrote a real {} (not via the post-checkout hook)",
            link.display()
        );
    } else if boot {
        count(&ws, "target-boot");
    }
    apply_steps(&steps)
}

/// A boot target's dir under the pool ([`boot_of`]).
const BOOT: &str = "boot";
/// The first line after the shebang of the post-checkout hook [`install_hook`] writes: marks a hook as ours (rewritten when its text changes).
const HOOK_MARK: &str =
    "# atl-next: generated by cargo xtask (out::install_hook); links target/ into the build pool";

/// The post-checkout hook text for pool `pool` (absolute): on `git worktree add` (previous HEAD all zeros), a checkout without `target` gets `target -> <pool>/boot/<worktree name>`.
/// Why a hook: cargo builds xtask before xtask runs, and the only per-checkout switch cargo reads with no tracked file or env var is `target` itself (`build.target-dir` takes no templates; a relative one resolves per checkout depth). Without it every new worktree's bootstrap writes ~0.6 GB on the checkout's disk.
/// Shell because git runs hooks as executables before any of our binaries exist; generated like the host/bin wrappers, never tracked.
pub fn hook_text(pool: &Path) -> String {
    let boot = pool.join(BOOT);
    format!(
        "#!/bin/sh\n{HOOK_MARK}\n[ \"$1\" = 0000000000000000000000000000000000000000 ] || exit 0\n[ -e target ] || [ -L target ] && exit 0\nid=$(basename \"$(git rev-parse --absolute-git-dir)\")\nmkdir -p '{b}'/\"$id\" && ln -s '{b}'/\"$id\" target\n",
        b = boot.display()
    )
}

/// Writes the post-checkout hook ([`hook_text`]) of the repository at `root` when missing or ours and outdated; returns the line when it writes. A foreign hook is refused: chaining an unknown script is not ours to decide.
pub fn install_hook(root: &Path) -> Result<Option<String>> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let pool = pool(&ws);
    let pool = std::fs::canonicalize(&pool).unwrap_or(pool);
    let path = PathBuf::from(git(
        Some(root),
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/post-checkout",
        ],
    )?);
    write_hook(&path, &hook_text(&pool))
}

fn write_hook(path: &Path, text: &str) -> Result<Option<String>> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::read_to_string(path) {
        Ok(old) if old == text => return Ok(None),
        Ok(old) if !old.lines().any(|l| l == HOOK_MARK) => bail!(
            "{} is not the hook xtask writes (it links a new worktree's target/ into the build pool, so the first build stays off the checkout's disk): move it aside (`mv {0} {0}.local`) and rerun (xtask then writes its own; call {0}.local from it by hand if still needed)",
            path.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(Some(format!("out: hook: wrote {}", path.display())))
}

/// The `target/` line of `out show`.
fn describe_entry(e: &Entry, want: &Path) -> String {
    match e {
        Entry::Link(to) if to.as_path() == want => format!("linked to {}", want.display()),
        Entry::Link(to) => format!(
            "links to {}, not this build dir's target: relinked when this command ends",
            to.display()
        ),
        Entry::Dir => "real dir, not linked: moved or removed when this command ends".to_owned(),
        Entry::Absent => "missing: linked when this command ends".to_owned(),
        Entry::Other => "a file: remove it".to_owned(),
    }
}

/// Deletes build dir `dir` with its contents.
fn remove_build_dir(dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

/// Land: the checkout's `target/` and build dir leave with it. Refused while a process holds the dir. Returns land's summary line.
pub fn leave(root: &Path, ws: &Path) -> Result<String> {
    let id = checkout_id(root)?;
    let mut removed = Vec::new();
    let link = root.join(TARGET);
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
    let dir = dir_of(ws, &id);
    let store = crate::ws_config::load(ws)?.delta.ram_store.value;
    let run = crate::out_run::locate(ws, &store, &id)?
        .map_or_else(|| crate::out_run::run_dir_of(ws, &id), |(d, _)| d);
    if present(&dir)? || present(&run)? {
        let Some(_lock) = build_pool::try_lock_dir(ws, &id)? else {
            bail!(
                "{} is held by a running process: not removed",
                dir.display()
            );
        };
        if present(&run)? {
            crate::out_run::remove(&run)?;
            removed.push(run.display().to_string());
        }
        if let Some(rec) = crate::delta::record(&dir)? {
            crate::delta::drop_ram(ws, &id, &rec)?;
            removed.push(format!(
                "{} (ram delta {})",
                dir.display(),
                rec.dir(&id).display()
            ));
        } else if present(&dir)? {
            remove_build_dir(&dir)?;
            removed.push(dir.display().to_string());
        }
    }
    Ok(if removed.is_empty() {
        "no build dir".to_owned()
    } else {
        format!("removed: {}", removed.join(", "))
    })
}

/// The land worktree at `land`: its id must be [`LAND_ID`] (the only checkout whose build dir is built from exactly a landed commit at the canonical path).
fn land_id(land: &Path) -> Result<()> {
    let id = checkout_id(land)?;
    ensure!(
        id == LAND_ID,
        "only the land worktree's build dir becomes a base (this is {id})"
    );
    Ok(())
}

/// What `publish` keeps of `ws/land`: all but the gate's target ([`BOOTSTRAP_DIR`]). Cargo reuses a target's outputs as fresh when the checkout's sources are older than them, and the unit key does not include the checkout path: a child would pass its gate on the land's artifacts. [`reset_land`] carries the land's own back.
fn copy_base(src: &Path, dst: &Path) -> Result<Tier> {
    copy_tree(src, dst, None, &[BOOTSTRAP_DIR])
}

/// Land's Publish: `ws/land` becomes `base/<HEAD of land>` under its exclusive lock (no build runs in it), through a temp name renamed into place; then the dir is laid again ([`relay_land`]). Returns the summary line.
/// A pool dir is copied ([`copy_base`]). A ram dir (delta.rs) is published without mounting it: its lower reflinked, its upper applied (delta_publish.rs), so the data written to the pool is the upper's, the base's new content. Both leave out the gate state ([`BOOTSTRAP_DIR`]).
pub fn publish(land: &Path) -> Result<String> {
    land_id(land)?;
    let ws = Layout::of(land)?.workspace().to_owned();
    let id = LAND_ID;
    let head = git(Some(land), &["rev-parse", "HEAD"])?;
    let dst = base_of(&ws, &head);
    let src = dir_of(&ws, id);
    if present(&dst)? {
        return Ok(format!("out: base/{head} exists, not published again"));
    }
    ensure!(
        present(&src)?,
        "{} does not exist: nothing to publish",
        src.display()
    );
    let line = {
        let Some(_lock) = build_pool::try_lock_dir(&ws, id)? else {
            bail!(
                "{} is held by a running process: not published",
                src.display()
            );
        };
        let parent = dst.parent().context("base has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // the mtimes ws/land was built against, into the base: a child's new dir restores them (`checkout_stamps`)
        let stamps = crate::checkout_stamps::record(land)?;
        match crate::delta::record(&src)? {
            Some(rec) => publish_ram(&ws, &rec, &dst, &stamps)?,
            None => {
                let file = src.join(crate::checkout_stamps::FILE);
                std::fs::write(&file, stamps)
                    .with_context(|| format!("writing {}", file.display()))?;
                let tier = copy_base(&src, &dst)?;
                count_tier(&ws, tier);
                format!("out: base/{head} from ws/{id} ({})", tier.name())
            }
        }
    };
    // the base is out: a failed relay costs only the next land a fresh dir
    Ok(match relay_land(land, &ws, true) {
        Ok(laid) => format!("{line}; {laid}"),
        Err(e) => format!("{line}; ws/{id} not laid again: {e:#}"),
    })
}

/// [`publish`] of ram dir `rec` of land as `dst`. Refused while the delta is mounted (an upper is stable only unmounted), or when its lower or upper is gone or the upper was laid over another base: a base is never made from a part.
fn publish_ram(ws: &Path, rec: &crate::delta::Record, dst: &Path, stamps: &str) -> Result<String> {
    let id = LAND_ID;
    let Some(_mounted) = crate::delta::try_mount_lock(ws, id)? else {
        bail!("ws/{id}'s ram delta is mounted by a running command: not published");
    };
    let lower = base_of(ws, &rec.base);
    let upper = rec.dir(id).join("upper");
    ensure!(
        lower.is_dir(),
        "base/{} under ws/{id}'s delta is gone: not published",
        rec.base
    );
    let on = std::fs::read_to_string(upper.join(BASE_FILE)).with_context(|| {
        format!(
            "ws/{id}'s upper {} is gone or unmarked (store emptied, e.g. a WSL restart): not published",
            upper.display()
        )
    })?;
    ensure!(
        on.trim() == rec.base,
        "ws/{id}'s upper lies over base/{}, its record names base/{}: not published",
        on.trim(),
        rec.base
    );
    let started = std::time::Instant::now();
    let file = crate::checkout_stamps::FILE;
    let (tier, done) =
        crate::delta_publish::publish(&lower, &upper, dst, &[BOOTSTRAP_DIR], &|tmp| {
            std::fs::write(tmp.join(file), stamps)
                .with_context(|| format!("writing {}", tmp.join(file).display()))
        })?;
    count_tier(ws, tier);
    count(ws, "publish-ram");
    Ok(format!(
        "out: {} from base/{} ({}) + ws/{id}'s ram upper in {:.1} s: {}",
        dst.strip_prefix(pool(ws)).unwrap_or(dst).display(),
        rec.base,
        tier.name(),
        started.elapsed().as_secs_f64(),
        done.line()
    ))
}

/// Land's reset after a failed verify: `ws/land` laid again from the newest base that is an ancestor of the land worktree's HEAD ([`relay_land`]): no failed branch's outputs stay in the dir the next base comes from. Returns the line.
pub fn reset_land(land: &Path) -> Result<String> {
    land_id(land)?;
    let ws = Layout::of(land)?.workspace().to_owned();
    relay_land(land, &ws, false)
}

/// Lays land's dir again over the newest base that is an ancestor of the land worktree's HEAD (after Publish: the base just published; after a failed verify: the base before the branch), carrying its gate state ([`BOOTSTRAP_DIR`], which bases leave out: its cargo units key on the land checkout).
/// Tier: ram when the store probes RAM-backed and one admission try of class native (land's next build) fits, never waiting; else pool. `keep_pool`: a pool dir that would stay pool is kept as it is (after Publish it equals the base: nothing to redo).
/// A ram dir's upper then holds only the next land's delta: publishing moved the last one into the base. The worktree's `target` link follows the tier at once (ram: the boot dir), since land's next cargo bootstrap runs before any xtask settles it.
pub fn relay_land(land: &Path, ws: &Path, keep_pool: bool) -> Result<String> {
    let id = LAND_ID;
    let dir = dir_of(ws, id);
    let cfg = crate::ws_config::load(ws)?;
    let rec = crate::delta::record(&dir)?;
    // the configured store: the new dir's tier is decided now, wherever the old upper lives
    let store = cfg.delta.ram_store.value.clone();
    let fits = || crate::delta::fits_now(ws, id, &cfg.delta, crate::delta::Class::Native, &store);
    if keep_pool
        && rec.is_none()
        && present(&dir)?
        && let Err(why) = fits()?
    {
        return Ok(format!("ws/{id} stays on tier pool ({why})"));
    }
    let pid = std::process::id();
    // aside on the filesystem it lives on, so moving it is a rename; no id lists these names
    let (state, aside) = match &rec {
        Some(r) => (
            r.dir(id).join("upper").join(BOOTSTRAP_DIR),
            r.store.join(format!(".{id}-{BOOTSTRAP_DIR}-{pid}")),
        ),
        None => (
            dir.join(BOOTSTRAP_DIR),
            pool(ws).join(format!(".{id}-{BOOTSTRAP_DIR}-{pid}")),
        ),
    };
    let mut kept = false;
    if present(&dir)? {
        let Some(_lock) = build_pool::try_lock_dir(ws, id)? else {
            bail!(
                "{} is held by a running process: not laid again",
                dir.display()
            );
        };
        if present(&state)? {
            std::fs::rename(&state, &aside)
                .with_context(|| format!("moving {} aside", state.display()))?;
            kept = true;
        }
        match &rec {
            Some(r) => crate::delta::drop_ram(ws, id, r)?,
            None => {
                // gone at once: a crash mid-delete must not leave a partial dir that Enter takes for land's (a `.`-name is an orphan to `out prune`)
                let old = dir.with_file_name(format!(".{id}.old-{pid}"));
                std::fs::rename(&dir, &old)
                    .with_context(|| format!("renaming {} -> {}", dir.display(), old.display()))?;
                remove_build_dir(&old)?;
            }
        }
    }
    // the carried gate state was built from the land's earlier content: no mtime restore under it
    let laid = (|| -> Result<String> {
        Ok(match fits()? {
            Ok(why) => match crate::delta::make_ram(land, ws, id, &store, !kept) {
                Ok(_) => format!("ram ({why})"),
                Err(e) => {
                    make(land, !kept)?;
                    format!("pool (ram dir not made: {e:#})")
                }
            },
            Err(why) => {
                make(land, !kept)?;
                format!("pool ({why})")
            }
        })
    })();
    let laid = match laid {
        Ok(l) => l,
        Err(e) => {
            if kept {
                let _ = std::fs::remove_dir_all(&aside);
            }
            return Err(e);
        }
    };
    let ram = crate::delta::record(&dir)?;
    if kept {
        let to = match &ram {
            Some(r) => r.dir(id).join("upper").join(BOOTSTRAP_DIR),
            None => dir.join(BOOTSTRAP_DIR),
        };
        place(&aside, &to)?;
    }
    if ram.is_some() {
        link_target(land, &dir)?;
    } else {
        // a link to the boot dir or to this dir's target stays valid; settle_target moves it at the next command's end
        let target = target_of(&dir);
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating {}", target.display()))?;
    }
    let base = match &ram {
        Some(r) => r.base.clone(),
        None => std::fs::read_to_string(dir.join(BASE_FILE))
            .map_or("no base".to_owned(), |c| c.trim().to_owned()),
    };
    Ok(format!(
        "ws/{id} laid over base/{base} on tier {laid}{}",
        if kept { "; gate state carried" } else { "" }
    ))
}

/// Moves dir `from` to the absent `to`: a rename on one filesystem, else a copy (`cp -a`) and removal.
fn place(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let ok = Command::new("cp")
        .arg("-a")
        .arg("--")
        .arg(from)
        .arg(to)
        .status()
        .context("running cp")?
        .success();
    ensure!(ok, "cp -a {} -> {} failed", from.display(), to.display());
    std::fs::remove_dir_all(from).with_context(|| format!("removing {}", from.display()))
}

/// Points the checkout's `target` at what its build dir `dir` needs now: a ram dir's boot target (cargo's bootstrap runs outside the namespace), a pool dir's own `target/` ([`settle_target`]'s rule, applied at once).
fn link_target(root: &Path, dir: &Path) -> Result<()> {
    let want = if dir.join(crate::delta::RECORD).exists() {
        boot_of(&target_of(dir)).context("build dir path has no boot target")?
    } else {
        target_of(dir)
    };
    apply_steps(&link_steps(&root.join(TARGET), &want)?)
}

/// `native`'s refresh after a rebase: when a base newer than the one `ws/<id>` came from (`.base`) is an ancestor of HEAD, the dir is replaced by a copy of it. Returns the line to print; None = nothing to do.
/// Why: the dir's own outputs were built on the old base and are mostly stale after the rebase, while the new base holds main's build of everything the rebase brought; the checkout's compiles hit sccache at the canonical path, so redoing its own delta is cheap.
/// Kept: the dir's cargo target roots (`out_prune::DIR_ROOTS`), moved into the new copy: `target/` holds this running xtask binary (re-executed by path into the namespace), and every root holds the checkout's cargo units, which key on the checkout path a base's units (built in the land worktree) never match: taking the base's cost a 72-crate rebuild (X4 gate).
/// No mtime restore (`checkout_stamps`): the kept roots hold units built from the checkout's own content after the base's recorded mtimes, so an old mtime on a file rebased back to the base's content would make them fresh. Instead every path differing from the new base gets now (`touch_changed_since`): the base's ninja outputs are newer than an old own edit.
/// Only when main brought a large change (owner 2026-11-16): at least `build.refresh_min_upstream_files` files under [`UPSTREAM_INPUTS`] changed from the dir's base to the newer one (derivation on ws_config.rs DEFAULT_REFRESH_MIN_UPSTREAM_FILES); below it keeping the old dir rebuilds less.
/// Kept (line `out: keep <reason>`): a dir without `.base` (built from zero), one already on the newest base or whose base is not an ancestor of it, a small change, a dir another process holds. None: the land worktree (land owns `ws/land`) or no dir yet (the first build makes it).
pub fn refresh(root: &Path) -> Result<Option<String>> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = checkout_id(root)?;
    let dir = dir_of(&ws, &id);
    if id == LAND_ID {
        return Ok(None);
    }
    match entry(&dir)? {
        Entry::Dir => {}
        Entry::Absent => return Ok(None),
        _ => return Ok(Some(format!("out: keep ws/{id}: not a dir of its own"))),
    }
    // a ram dir (delta.rs) names its base in its record; its .base is in the upper, covered outside
    let ram = crate::delta::record(&dir)?;
    let cur = match &ram {
        Some(r) => r.base.clone(),
        None => {
            let Ok(cur) = std::fs::read_to_string(dir.join(BASE_FILE)) else {
                return Ok(Some(format!(
                    "out: keep ws/{id}: built from zero (no {BASE_FILE})"
                )));
            };
            cur.trim().to_owned()
        }
    };
    let bases = bases(&ws)?;
    let Some(pick) = bases
        .iter()
        .find(|c| git(Some(root), &["merge-base", "--is-ancestor", c, "HEAD"]).is_ok())
    else {
        return Ok(Some(format!(
            "out: keep ws/{id}: no base is an ancestor of HEAD"
        )));
    };
    if *pick == cur {
        return Ok(Some(format!("out: keep ws/{id}: on the newest base/{cur}")));
    }
    if git(Some(root), &["merge-base", "--is-ancestor", &cur, pick]).is_err() {
        return Ok(Some(format!(
            "out: keep ws/{id}: its base/{cur} is not an ancestor of base/{pick}"
        )));
    }
    let changed = upstream_changed(root, &cur, pick)?;
    let min = crate::ws_config::load(&ws)?.refresh_min_upstream_files;
    if !wants_refresh(changed, min.value) {
        return Ok(Some(format!(
            "out: keep ws/{id} on base/{cur}: {changed} upstream input files changed to base/{pick}, under build.refresh_min_upstream_files {min}"
        )));
    }
    if let Some(r) = &ram {
        let why = format!("{changed} upstream input files changed");
        return crate::delta::refresh(root, &ws, &id, r, pick, &why).map(Some);
    }
    let Some(_lock) = build_pool::try_lock_dir(&ws, &id)? else {
        return Ok(Some(format!(
            "out: keep ws/{id}: not refreshed to base/{pick}: another process holds it"
        )));
    };
    let parent = dir.parent().context("build dir has no parent")?;
    let old = parent.join(format!(".{id}.old-{}", std::process::id()));
    std::fs::rename(&dir, &old)
        .with_context(|| format!("renaming {} -> {}", dir.display(), old.display()))?;
    let tier = match copy_tree(&base_of(&ws, pick), &dir, Some(pick), &[]) {
        Ok(t) => t,
        Err(e) => {
            // the old dir back: a failed copy must not cost the checkout its build
            std::fs::rename(&old, &dir)
                .with_context(|| format!("restoring {} after: {e:#}", dir.display()))?;
            return Err(e);
        }
    };
    count_tier(&ws, tier);
    let mut kept = Vec::new();
    for root_name in out_prune::DIR_ROOTS {
        let (own, fresh) = (old.join(root_name), dir.join(root_name));
        if !own.is_dir() {
            continue;
        }
        if present(&fresh)? {
            std::fs::remove_dir_all(&fresh)
                .with_context(|| format!("removing {}", fresh.display()))?;
        }
        std::fs::rename(&own, &fresh)
            .with_context(|| format!("moving {} -> {}", own.display(), fresh.display()))?;
        kept.push(root_name);
    }
    remove_build_dir(&old)?;
    // the new base's outputs postdate an older own edit of the checkout's: its delta to base/pick must read newer
    let touched = crate::checkout_stamps::touch_changed_since(root, pick)?;
    Ok(Some(format!(
        "out: refresh ws/{id} from base/{cur} to base/{pick} ({changed} upstream input files changed; {}); kept: {}; {touched} paths differing from base/{pick} set to now",
        tier.name(),
        kept.join(", ")
    )))
}

/// Repo paths whose change makes native redo ninja/AOSP work a newer base already did: the upstream patches and the pins. Refresh gains only there (see [`refresh`]).
pub const UPSTREAM_INPUTS: [&str; 2] = ["patches", crate::sources::FILE];

/// Files under [`UPSTREAM_INPUTS`] that differ between commits `from` and `to`.
fn upstream_changed(root: &Path, from: &str, to: &str) -> Result<usize> {
    let mut args = vec!["diff", "--name-only", from, to, "--"];
    args.extend(UPSTREAM_INPUTS);
    Ok(git(Some(root), &args)?
        .lines()
        .filter(|l| !l.is_empty())
        .count())
}

/// The refresh decision: `changed` upstream input files reach the threshold `min`.
fn wants_refresh(changed: usize, min: u32) -> bool {
    changed >= min as usize
}

/// The checkout's build dir made ([`ensure`]) with its `target/`, before a parent runs cargo there: the checkout's `target` link may name this dir's target/ already (a dir laid again or deleted), an empty copy (no base) lacks it, and cargo, which runs before any xtask code can settle the link, fails on a dangling one.
/// Land's Enter calls it: a missing land dir is laid with the tier rule of [`relay_land`]; a ram dir needs nothing (its `target` link names the boot dir, which exists).
pub fn prepare(root: &Path) -> Result<PathBuf> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = checkout_id(root)?;
    let dir = dir_of(&ws, &id);
    if crate::delta::record(&dir)?.is_some() {
        link_target(root, &dir)?;
        return Ok(dir);
    }
    if id == LAND_ID && !present(&dir)? {
        println!("out: {}", relay_land(root, &ws, false)?);
        return Ok(dir);
    }
    let (_, dir) = make(root, true)?;
    let target = target_of(&dir);
    std::fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    Ok(dir)
}

/// Free bytes for unprivileged writes on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Result<u64> {
    let st = rustix::fs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    Ok(st.f_bavail * st.f_frsize)
}

/// The build dirs under the pool: (id, path), sorted; temp names (`.`-prefixed, a crashed copy) included as written.
fn build_dirs(ws: &Path) -> Result<Vec<(String, PathBuf)>> {
    let dir = pool(ws).join("ws");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for e in rd {
        let e = e?;
        found.push((e.file_name().to_string_lossy().into_owned(), e.path()));
    }
    found.sort();
    Ok(found)
}

/// `(id, path)` of build dirs.
type Dirs = Vec<(String, PathBuf)>;

/// What `out prune` deletes: build dirs whose id no live checkout has (temp leftovers included) and bases beyond the newest [`KEEP_BASES`] that no live ram dir lays its delta over (delta.rs: a lower must not vanish under an upper). Also returns the pinned bases.
fn orphans(ws: &Path, live: &BTreeSet<String>) -> Result<(Dirs, Vec<PathBuf>, BTreeSet<String>)> {
    let (dirs, kept): (Dirs, Dirs) = build_dirs(ws)?
        .into_iter()
        .partition(|(id, _)| !live.contains(id));
    let mut pinned = BTreeSet::new();
    for (_, path) in &kept {
        if let Some(rec) = crate::delta::record(path)? {
            pinned.insert(rec.base);
        }
    }
    let mut old: Vec<PathBuf> = bases(ws)?
        .into_iter()
        .skip(KEEP_BASES)
        .filter(|c| !pinned.contains(c))
        .map(|c| base_of(ws, &c))
        .collect();
    // temp names under base/ (`.`-prefixed: a publish's copy before its rename) exist only while a publish holds ws/land's lock: with the lock free each is a crashed publish's leftover
    if let Some(_free) = build_pool::try_lock_dir(ws, LAND_ID)?
        && let Ok(rd) = std::fs::read_dir(pool(ws).join("base"))
    {
        old.extend(
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path()),
        );
    }
    Ok((dirs, old, pinned))
}

/// Boot targets ([`boot_of`]) whose id no live checkout has. Nothing else frees them: once a worktree's build dir exists its `target` is moved there, so a boot dir left behind is a checkout gone before its first build or one whose link was replaced.
fn boot_orphans(ws: &Path, live: &BTreeSet<String>) -> Result<Dirs> {
    let boot = pool(ws).join(BOOT);
    let rd = match std::fs::read_dir(&boot) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", boot.display())),
    };
    let mut out = Dirs::new();
    for entry in rd {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !live.contains(&id) {
            out.push((id, entry.path()));
        }
    }
    out.sort();
    Ok(out)
}

/// Removes orphan boot dir `path` under build dir `id`'s exclusive lock, as the other orphan classes: `false` when a process of that id still holds it.
fn remove_boot(ws: &Path, id: &str, path: &Path) -> Result<bool> {
    let Some(_lock) = build_pool::try_lock_dir(ws, id)? else {
        return Ok(false);
    };
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))?;
    Ok(true)
}

/// Sorts prune candidates most paths first and returns each one's path count. A copy's metadata scales with its paths, so this is what a prune frees first.
fn rank_by_paths(dirs: &mut Dirs, old: &mut [PathBuf]) -> Result<BTreeMap<PathBuf, u64>> {
    let mut counts = BTreeMap::new();
    for path in dirs.iter().map(|(_, p)| p).chain(old.iter()) {
        counts.insert(path.clone(), path_count(path)?);
    }
    dirs.sort_by_key(|(_, p)| std::cmp::Reverse(counts[p]));
    old.sort_by_key(|p| std::cmp::Reverse(counts[p]));
    Ok(counts)
}

/// The ` (N paths)` note a ranked candidate prints with; empty when unranked.
fn paths_note(counts: &BTreeMap<PathBuf, u64>, path: &Path) -> String {
    counts
        .get(path)
        .map_or_else(String::new, |n| format!(" ({n} paths)"))
}

/// Entries under `dir`, recursively, symlinks not followed.
fn path_count(dir: &Path) -> Result<u64> {
    let mut count = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let entry = entry?;
            count += 1;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(count)
}

fn age(path: &Path) -> String {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or("?".to_owned(), |d| {
            format!("{:.1} h", d.as_secs_f64() / 3600.0)
        })
}

/// The ids of the live checkouts of `root`'s repository: a build dir or remote run dir named otherwise is an orphan.
pub fn live_ids_of(root: &Path) -> Result<BTreeSet<String>> {
    let common = PathBuf::from(git(
        Some(root),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    live_ids(&common)
}

/// The ids of the build dirs whose checkout is live (`root`'s repository).
pub fn live_dirs(root: &Path) -> Result<Vec<String>> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let live = live_ids_of(root)?;
    Ok(build_dirs(&ws)?
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| live.contains(id))
        .collect())
}

pub fn run(root: &Path, cmd: &Out) -> Result<ExitCode> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let live = live_ids_of(root)?;
    match cmd {
        Out::Show { run: true } => {
            let (_, run, _held) = crate::out_run::ensure(root)?;
            println!("{}", run.display());
        }
        Out::Show { run: false } => {
            let id = checkout_id(root)?;
            let dir = dir_of(&ws, &id);
            if let Some(rec) = crate::delta::record(&dir)? {
                println!(
                    "out: ws/{id}: {} (tier ram: overlay of base/{} + {}; outside build commands the dir is its empty mount point)",
                    dir.display(),
                    rec.base,
                    rec.dir(&id).join("upper").display()
                );
                let boot = boot_of(&target_of(&dir)).unwrap_or_default();
                println!(
                    "target: {}",
                    describe_entry(&entry(&root.join(TARGET))?, &boot)
                );
            } else if present(&dir)? {
                let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                let base = std::fs::read_to_string(dir.join(BASE_FILE))
                    .map_or("no base".to_owned(), |c| format!("from base/{}", c.trim()));
                println!("out: ws/{id}: {} (tier pool, {base})", real.display());
                let state = describe_entry(&entry(&root.join(TARGET))?, &target_of(&dir));
                println!("target: {state}");
            } else {
                println!("out: ws/{id}: not made yet: the next build command makes it");
            }
            let cfg = crate::ws_config::load(&ws)?;
            match crate::out_run::locate(&ws, &cfg.delta.ram_store.value, &id)? {
                Some((run, rec)) => println!("run={} ({})", run.display(), rec.tier),
                None => println!(
                    "run=: not made yet (`cargo xtask out show --run` or an app run makes it, on the ram store when class run fits, else {})",
                    crate::out_run::run_dir_of(&ws, &id).display()
                ),
            }
            let store = &cfg.delta.ram_store;
            match crate::delta::probe(&store.value, &cfg.delta)? {
                Ok(s) => {
                    println!("delta store {}: {}", store.value.display(), s.line());
                    let h = crate::delta::hold_now(&ws, &cfg.delta, &s)?;
                    println!(
                        "land hold: {:.2} GiB RAM / {:.2} GiB raw (class native peak at the store ratio; 0 = land's reservation stands in or native never fits)",
                        h.ram as f64 / (1u64 << 30) as f64,
                        h.raw as f64 / (1u64 << 30) as f64
                    );
                }
                Err(why) => println!("delta store {}: unusable: {why}", store.value.display()),
            }
            for c in crate::delta::Class::ALL {
                print!(
                    "{}{} {:.1} GiB",
                    if c == crate::delta::Class::Small {
                        "delta peaks: "
                    } else {
                        ", "
                    },
                    c.name(),
                    c.peak(&cfg.delta) as f64 / 1_073_741_824.0
                );
            }
            println!();
            let p = pool(&ws);
            match free_bytes(&p) {
                Ok(b) => println!(
                    "pool: {} free {:.1} GiB",
                    p.display(),
                    b as f64 / 1_073_741_824.0
                ),
                Err(e) => println!("pool: {}: {e:#}", p.display()),
            }
            match crate::ws_config::load(&ws)
                .and_then(|cfg| crate::disk_floor::pool_rows(&ws, &cfg))
            {
                Ok(rows) => {
                    for (line, short) in rows {
                        println!("{line}{}", if short { " [SHORT]" } else { "" });
                    }
                }
                Err(e) => println!("pool disk rows: {e:#}"),
            }
            for c in bases(&ws)? {
                println!("base/{c}: {}", age(&base_of(&ws, &c)));
            }
            for (id, path) in build_dirs(&ws)? {
                let state = if live.contains(&id) { "live" } else { "orphan" };
                let tier = if path.join(crate::delta::RECORD).exists() {
                    "ram"
                } else {
                    "pool"
                };
                println!("ws/{id}: {state}, {tier}, {}", age(&path));
            }
            for name in ["ram", "pool", "wait", "refused", "lost"] {
                let n = std::fs::read_to_string(p.join(format!("delta-{name}.count")))
                    .map_or("0".to_owned(), |t| t.trim().to_owned());
                print!(
                    "{}delta-{name} {n}",
                    if name == "ram" { "counts: " } else { ", " }
                );
            }
            let published = std::fs::read_to_string(p.join("publish-ram.count"))
                .map_or("0".to_owned(), |t| t.trim().to_owned());
            println!(", publish-ram {published}");
            let runs = [
                "run-dir-ram",
                "run-dir-pool",
                "run-ram",
                "run-pool",
                "run-full",
            ]
            .map(|n| {
                let v = std::fs::read_to_string(p.join(format!("{n}.count")))
                    .map_or("0".to_owned(), |t| t.trim().to_owned());
                format!("{n} {v}")
            });
            println!("run counts: {}", runs.join(", "));
        }
        Out::Tier { tier, execute } => return set_tier(root, &ws, tier == "ram", *execute),
        Out::Prune {
            tasks: true,
            execute,
            ..
        } => return crate::out_tasks::run(root, &ws, *execute),
        Out::Prune {
            units: true,
            older_than,
            execute,
            ..
        } => {
            let sweep = out_prune::sweep(&ws, &live_dirs(root)?, true, *older_than, *execute)?;
            return Ok(if sweep.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Out::Prune {
            units: false,
            execute,
            ..
        } => {
            if prune_dirs(&ws, &live, *execute)? > 0 {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `out tier`: drops the checkout's build dir and makes it again on tier `ram` (delta.rs make_ram) or `pool` ([`ensure`]); dry run unless `execute`.
fn set_tier(root: &Path, ws: &Path, ram: bool, execute: bool) -> Result<ExitCode> {
    let id = checkout_id(root)?;
    let dir = dir_of(ws, &id);
    let rec = crate::delta::record(&dir)?;
    let now = match (&rec, present(&dir)?) {
        (Some(_), _) => "ram",
        (None, true) => "pool",
        (None, false) => "not made",
    };
    let want = if ram { "ram" } else { "pool" };
    let cfg = crate::ws_config::load(ws)?;
    if ram && let Err(why) = crate::delta::probe(&cfg.delta.ram_store.value, &cfg.delta)? {
        bail!("no ram tier for ws/{id}: {why}");
    }
    // the run dir follows the explicit choice too (off = exact baseline); its data is regenerable
    crate::out_run::set_tier(root, ws, &cfg.delta, ram, execute)?;
    if now == want {
        println!("out tier: ws/{id} is on tier {want} already");
        return Ok(ExitCode::SUCCESS);
    }
    if !execute {
        println!(
            "out tier: would drop ws/{id} ({now}) and make it on tier {want} (dry run; add --execute)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    {
        let Some(_lock) = build_pool::try_lock_dir(ws, &id)? else {
            bail!("ws/{id} is held by a running command: not changed");
        };
        match &rec {
            Some(r) => crate::delta::drop_ram(ws, &id, r)?,
            None if present(&dir)? => remove_build_dir(&dir)?,
            None => {}
        }
    }
    if ram {
        crate::delta::make_ram(root, ws, &id, &cfg.delta.ram_store.value, true)?;
    } else {
        make(root, true)?;
    }
    println!("out tier: ws/{id} now on tier {want}");
    Ok(ExitCode::SUCCESS)
}

/// `out prune` without a flag: build dirs whose checkout is not in `live`, bases beyond the keep rule, and each live checkout's kept gate logs past [`red_logs::KEEP_RUNS`] (red_logs::prune); lists them, `execute` removes them. Returns how many removals failed (a busy dir is skipped, not failed).
/// Also `land`'s cleanup child, after the landed worktree is gone. The orphans and bases are under the owner's standing approval; the kept gate logs past the keep (red_logs.rs) are not on that list, yet this land cleanup (execute) removes them too.
pub fn prune_dirs(ws: &Path, live: &BTreeSet<String>, execute: bool) -> Result<usize> {
    let rows =
        match crate::ws_config::load(ws).and_then(|cfg| crate::disk_floor::pool_rows(ws, &cfg)) {
            Ok(rows) => rows,
            Err(e) => {
                println!("pool metadata: {e:#}");
                Vec::new()
            }
        };
    for (line, _) in &rows {
        println!("{line}");
    }
    // held to the end: a ram dir made meanwhile cannot record a base this prune is deleting
    let _pins = crate::delta::bases_lock(ws)?;
    let (mut dirs, mut old, pinned) = orphans(ws, live)?;
    for c in &pinned {
        println!("base/{c}: kept: a ram delta lies over it");
    }
    let counts = if rows.iter().any(|(_, short)| *short) {
        println!(
            "out prune: a pool row is short: candidates ranked most paths first (a copy's metadata scales with its paths)"
        );
        rank_by_paths(&mut dirs, &mut old)?
    } else {
        BTreeMap::new()
    };
    let mut failed = 0;
    for (id, path) in &dirs {
        if !execute {
            println!(
                "orphan ws/{id}: {} (checkout gone){}",
                path.display(),
                paths_note(&counts, path)
            );
            continue;
        }
        let gone = |path: &Path| match crate::delta::record(path)? {
            Some(rec) => crate::delta::drop_ram(ws, id, &rec),
            None => remove_build_dir(path),
        };
        match build_pool::try_lock_dir(ws, id)? {
            None => println!("orphan ws/{id}: busy, skipped: a process holds it"),
            Some(_lock) => match gone(path) {
                Ok(()) => println!("orphan ws/{id}: removed"),
                Err(e) => {
                    failed += 1;
                    println!("orphan ws/{id}: FAILED: {e:#}");
                }
            },
        }
    }
    let store = crate::ws_config::load(ws)?.delta.ram_store.value;
    let runs = crate::out_run::orphans(ws, &store, live)?;
    for (id, path) in &runs {
        if !execute {
            println!("orphan run/{id}: {} (checkout gone)", path.display());
            continue;
        }
        match build_pool::try_lock_dir(ws, id)? {
            None => println!("orphan run/{id}: busy, skipped: a process holds it"),
            Some(_lock) => match crate::out_run::remove(path) {
                Ok(()) => println!("orphan run/{id}: removed"),
                Err(e) => {
                    failed += 1;
                    println!("orphan run/{id}: FAILED: {e:#}");
                }
            },
        }
    }
    let boots = boot_orphans(ws, live)?;
    for (id, path) in &boots {
        if !execute {
            println!("orphan boot/{id}: {} (checkout gone)", path.display());
            continue;
        }
        match remove_boot(ws, id, path) {
            Ok(true) => println!("orphan boot/{id}: removed"),
            Ok(false) => println!("orphan boot/{id}: busy, skipped: a process holds it"),
            Err(e) => {
                failed += 1;
                println!("orphan boot/{id}: FAILED: {e:#}");
            }
        }
    }
    // ram store dirs with neither a live checkout nor a build dir (a crash between dropping the mount point and the store dir, a checkout removed by hand)
    let mut keep = live.clone();
    keep.extend(build_dirs(ws)?.into_iter().map(|(id, _)| id));
    let strays = crate::delta::orphan_store_dirs(&store, ws, &keep);
    for (id, path) in &strays {
        if !execute {
            println!("orphan ram delta {id}: {} (checkout gone)", path.display());
            continue;
        }
        match crate::delta::remove_orphan_store(ws, id, path) {
            Ok(true) => println!("orphan ram delta {id}: removed"),
            Ok(false) => println!("orphan ram delta {id}: busy, skipped: mounted"),
            Err(e) => {
                failed += 1;
                println!("orphan ram delta {id}: FAILED: {e:#}");
            }
        }
    }
    for path in &old {
        if !execute {
            println!("old base {}{}", path.display(), paths_note(&counts, path));
            continue;
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => println!("old base {}: removed", path.display()),
            Err(e) => {
                failed += 1;
                println!("old base {}: FAILED: {e}", path.display());
            }
        }
    }
    let (kept_runs, kept_failed) = red_logs::prune(ws, live, execute)?;
    failed += kept_failed;
    let verb = if execute {
        "removed"
    } else {
        "dry run, would remove (add --execute)"
    };
    println!(
        "out prune: {verb} {} orphan build dir(s), {} orphan run dir(s), {} orphan boot dir(s), {} orphan ram delta(s), {} old base(s) and {kept_runs} kept gate-log run(s) past the keep",
        dirs.len(),
        runs.len(),
        boots.len(),
        strays.len(),
        old.len()
    );
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch workspace whose delta store is absent (pool tier): a test never reaches the host's store, which every workspace shares by id (2026-11-21 and 2026-11-22: scratch runs wrote and deleted store dirs of live ids).
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-out-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        let toml = format!(
            "[delta]\nram_store = \"{}\"\n",
            d.join("no-ram-store").display()
        );
        std::fs::write(d.join("workspace.toml"), toml).expect("workspace.toml");
        d
    }

    /// Only a test or bin unit whose dep-info names the builder checkout loses its fingerprint: a lib naming it, a test naming another path, and the cross-target layout's units stay or go by the same rule.
    #[test]
    fn copied_executables_reading_the_builder_are_forgotten() {
        let dir = scratch("forget-runs");
        let builder = Path::new("/w/wt/land");
        let unit = |profile: &str, pkg: &str, hash: &str, kind: &str, env: &str| {
            let fp = dir
                .join(profile)
                .join(".fingerprint")
                .join(format!("{pkg}-{hash}"));
            std::fs::create_dir_all(&fp).unwrap();
            std::fs::write(fp.join(format!("{kind}-{pkg}")), "").unwrap();
            let deps = dir.join(profile).join("deps");
            std::fs::create_dir_all(&deps).unwrap();
            std::fs::write(
                deps.join(format!("{}-{hash}.d", pkg.replace('-', "_"))),
                format!("x: a.rs\n\n# env-dep:{env}\n"),
            )
            .unwrap();
            fp
        };
        let land_dir = "OUT_DIR=/w/wt/land/target/debug/build/b-1/out";
        let test = unit(
            "target/debug",
            "atl-bionic",
            "aa",
            "test-integration-test",
            land_dir,
        );
        let bin = unit(
            "target/x86_64-unknown-linux-gnu/debug",
            "tool",
            "bb",
            "bin",
            "CARGO_MANIFEST_DIR=/w/wt/land/crates/tool",
        );
        let lib = unit("target/debug", "atl-layout", "cc", "lib", land_dir);
        let own = unit(
            "target/debug",
            "atl-bionic",
            "dd",
            "test-integration-test",
            "CARGO_MANIFEST_DIR=/w/wt/child/crates/atl-bionic",
        );
        // `/w/wt/landing` shares the prefix but is another checkout
        let near = unit(
            "target/debug",
            "x",
            "ee",
            "test-lib",
            "CARGO_MANIFEST_DIR=/w/wt/landing/crates/x",
        );
        assert_eq!(forget_runs_reading(&dir, builder).unwrap(), 2);
        assert!(!test.exists() && !bin.exists());
        assert!(lib.exists() && own.exists() && near.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn put(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, "x\n").expect("file");
    }

    /// Independent counts: a = 3 files; b = 1 file + dir s with 2 files = 4 entries; c = empty = 0. Ranked most paths first.
    #[test]
    fn prune_ranks_candidates_by_paths() {
        let root = scratch("rank");
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for i in 0..3 {
            put(&a.join(format!("f{i}")));
        }
        put(&b.join("f0"));
        put(&b.join("s").join("f0"));
        put(&b.join("s").join("f1"));
        std::fs::create_dir_all(&c).expect("empty dir");
        let mut dirs = vec![("x".to_owned(), a.clone()), ("y".to_owned(), c.clone())];
        let mut old = vec![b.clone()];
        let counts = rank_by_paths(&mut dirs, &mut old).expect("ranked");
        assert_eq!(old.as_slice(), std::slice::from_ref(&b));
        assert_eq!(
            dirs.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
            [a.clone(), c.clone()]
        );
        assert_eq!(counts[&a], 3);
        assert_eq!(counts[&b], 4);
        assert_eq!(counts[&c], 0);
        assert_eq!(paths_note(&counts, &b), " (4 paths)");
        assert_eq!(paths_note(&BTreeMap::new(), &b), "");
    }

    /// Only a copy into `<ws>/out/pool` writes the pool's metadata; a copy elsewhere is not gated.
    #[test]
    fn pool_ws_is_the_workspace_of_a_pool_copy_only() {
        assert_eq!(
            pool_ws(Path::new("/w/out/pool/ws/repo")),
            Some(Path::new("/w"))
        );
        assert_eq!(
            pool_ws(Path::new("/w/out/pool/base/c1")),
            Some(Path::new("/w"))
        );
        assert_eq!(pool_ws(Path::new("/w/out/other/c1")), None);
    }

    #[test]
    fn ids_are_main_or_the_worktree_name() {
        let common = Path::new("/r/.git");
        assert_eq!(id_of(common, common).expect("main"), MAIN_ID);
        assert_eq!(
            id_of(&common.join("worktrees/snap-x3"), common).expect("worktree"),
            "snap-x3"
        );
        assert!(id_of(&common.join("worktrees/repo"), common).is_err());
        assert!(id_of(Path::new("/elsewhere/.git"), common).is_err());
    }

    #[test]
    fn workspace_of_inverts_dir_of() {
        let ws = Path::new("/home/u/atl-next");
        assert_eq!(workspace_of(&dir_of(ws, "a")), Some(ws));
    }

    #[test]
    fn live_ids_skip_worktrees_whose_checkout_is_gone() {
        let d = scratch("live");
        let common = d.join("repo/.git");
        for (name, checkout) in [("here", d.join("wt/here")), ("gone", d.join("wt/gone"))] {
            put(&common.join("worktrees").join(name).join("gitdir"));
            std::fs::write(
                common.join("worktrees").join(name).join("gitdir"),
                format!("{}\n", checkout.join(".git").display()),
            )
            .expect("gitdir");
        }
        std::fs::create_dir_all(d.join("wt/here")).expect("checkout");
        let ids = live_ids(&common).expect("ids");
        assert_eq!(ids, BTreeSet::from([MAIN_ID.to_owned(), "here".to_owned()]));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// Both tiers copy the same tree and leave no temp name; `/dev/shm` (tmpfs, no reflink) must probe `Copy`. The `Reflink` tier is gated on the btrfs pool by hand (`cargo xtask out show` after a fresh worktree's first build): a test cannot assume a reflink filesystem.
    #[test]
    fn copy_tree_copies_through_a_temp_name_with_the_probed_tier() {
        let d = PathBuf::from("/dev/shm").join(format!("xtask-out-tier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        put(&d.join("base/c1/build.ninja"));
        put(&d.join("base/c1/sub/x.o"));
        std::os::unix::fs::symlink("sub/x.o", d.join("base/c1/link")).expect("link");
        std::fs::create_dir_all(d.join("ws")).expect("ws");
        assert_eq!(
            probe(&d.join("base/c1"), &d.join("ws")).expect("probe"),
            Tier::Copy
        );
        let tier = copy_tree(&d.join("base/c1"), &d.join("ws/a"), Some("c1"), &[]).expect("copy");
        assert_eq!(tier, Tier::Copy);
        assert!(d.join("ws/a/sub/x.o").is_file());
        assert_eq!(
            std::fs::read_link(d.join("ws/a/link")).expect("link kept"),
            Path::new("sub/x.o")
        );
        assert_eq!(
            std::fs::read_to_string(d.join("ws/a").join(BASE_FILE)).expect(".base"),
            "c1\n"
        );
        let names: Vec<String> = std::fs::read_dir(d.join("ws"))
            .expect("ws")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a".to_owned()],
            "a temp name or probe file stayed"
        );
        assert!(!d.join("base/c1").join(".atl-reflink-probe").exists());
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// A publish copy keeps the gate's target out of a base and every other entry in: a child reflinks the base, so a path-keyed target there would be inherited.
    #[test]
    fn a_base_copy_leaves_the_bootstrap_target_out() {
        let d = scratch("base-skip");
        put(&d.join("ws/land/target/x"));
        put(&d.join("ws/land/cargo/y"));
        put(&d.join("ws/land").join(BOOTSTRAP_DIR).join("z"));
        std::fs::create_dir_all(d.join("base")).expect("base");
        copy_base(&d.join("ws/land"), &d.join("base/c")).expect("copy");
        assert!(d.join("base/c/target/x").is_file());
        assert!(d.join("base/c/cargo/y").is_file());
        assert!(!d.join("base/c").join(BOOTSTRAP_DIR).exists());
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// Holds are per workspace root: a second root's hold is taken although the id is held under the first (the table was keyed by id alone), and releasing one root leaves the other held.
    #[test]
    fn a_hold_is_taken_for_each_workspace_root() {
        let (a, b) = (scratch("hold-a"), scratch("hold-b"));
        hold(&a, "x").expect("hold a");
        hold(&b, "x").expect("hold b");
        let busy = |ws: &Path| build_pool::try_lock_dir(ws, "x").expect("probe").is_none();
        assert!(busy(&a) && busy(&b), "a root's hold was skipped");
        assert_eq!(
            release_holds(&a).expect("release a"),
            BTreeSet::from(["x".to_owned()])
        );
        assert!(!busy(&a), "releasing root a left it held");
        assert!(busy(&b), "releasing root a dropped root b's hold");
        release_holds(&b).expect("release b");
        std::fs::remove_dir_all(&a).expect("cleanup a");
        std::fs::remove_dir_all(&b).expect("cleanup b");
    }

    /// A publish's temp under base/ is a prune candidate only while no publish can be writing it (ws/land's lock free); bases() never ranks it.
    #[test]
    fn base_temps_are_pruned_only_with_land_idle() {
        let ws = scratch("base-temps");
        let tmp = pool(&ws).join("base").join(".c1.pub-1");
        put(&tmp.join("f"));
        put(&base_of(&ws, "c0").join("f"));
        assert_eq!(bases(&ws).expect("bases"), ["c0"]);
        let live = BTreeSet::from([MAIN_ID.to_owned()]);
        let (_, old, _) = orphans(&ws, &live).expect("orphans");
        assert_eq!(old, vec![tmp.clone()]);
        let held = build_pool::try_lock_dir(&ws, LAND_ID)
            .expect("lock")
            .expect("free");
        let (_, old, _) = orphans(&ws, &live).expect("orphans");
        assert!(old.is_empty(), "a publish may be writing it: {old:?}");
        drop(held);
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Land on the ram tier, end to end without a build tool: laid over base/A, a "build" through the mounted overlay (modify, delete, replace a dir, gate state), publish -> base/B equals the merged view the build saw (minus gate state and `.base`, plus the stamps), ws/land laid again over base/B with the gate state carried and an otherwise empty upper; reset keeps ram; with the store gone reset falls back to the pool and keeps the gate state; a pool dir that would stay pool is kept by publish.
    #[test]
    fn land_on_ram_publishes_and_lays_again() {
        let ws = scratch("land-ram");
        let store =
            Path::new("/dev/shm").join(format!("xtask-out-{}-land-ram", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).expect("store");
        // tiny peaks: the tier rule, not this host's free RAM, is under test
        let toml = |store: &Path| {
            format!(
                "[delta]\nram_store = \"{}\"\npeak_native_gib = 0.01\nram_margin_gib = 0\n",
                store.display()
            )
        };
        std::fs::write(ws.join("workspace.toml"), toml(&store)).expect("toml");
        let repo = ws.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let g = |dir: &Path, args: &[&str]| git(Some(dir), args).expect("git");
        let commit = |msg: &str| {
            g(&repo, &["add", "-A"]);
            g(
                &repo,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ],
            );
            g(&repo, &["rev-parse", "HEAD"])
        };
        g(&repo, &["init", "-q", "-b", "main"]);
        put(&repo.join("f"));
        let a = commit("a");
        let land = ws.join("wt").join("land");
        g(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                land.to_str().expect("utf-8"),
                &a,
            ],
        );
        assert_eq!(checkout_id(&land).expect("id"), LAND_ID);
        let base_a = base_of(&ws, &a);
        for p in ["target/debug/own", "gone", "dir/x", "kept"] {
            put(&base_a.join(p));
        }
        let laid = relay_land(&land, &ws, false).expect("lay");
        assert!(laid.contains("tier ram"), "{laid}");
        let dir = dir_of(&ws, LAND_ID);
        let rec = crate::delta::record(&dir)
            .expect("record")
            .expect("a ram dir");
        assert_eq!(rec.base, a);
        assert!(
            !rec.dir(LAND_ID).join("fresh").exists(),
            "land's units are its own"
        );
        assert_eq!(
            entry(&land.join(TARGET)).expect("entry"),
            Entry::Link(boot_of(&target_of(&dir)).expect("boot"))
        );
        // the build, through the overlay as stage mount mounts it; the merged listing taken there
        let merged = ws.join("merged.list");
        let list = "find . -mindepth 1 \\( -type d -printf '%P d %m %T@\\n' \\) -o -printf '%P %y %m %T@ %s\\n' | LC_ALL=C sort";
        let build = format!(
            "set -e; mount -t overlay overlay -o \"$1\" \"$2\"; cd \"$2\"; echo more >> target/debug/own; touch -d 2001-01-01 target/debug/own; rm gone; rm -rf dir && mkdir dir && echo y > dir/y; mkdir -p bootstrap/debug && echo s > bootstrap/debug/state; {list} > \"$3\""
        );
        let ok = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
                "--",
                "sh",
                "-c",
            ])
            .arg(&build)
            .arg("sh")
            .arg(crate::delta::mount_options(&ws, LAND_ID, &rec).expect("opts"))
            .arg(&dir)
            .arg(&merged)
            .status()
            .expect("unshare")
            .success();
        assert!(ok, "build in the overlay");
        put(&repo.join("g"));
        let b = commit("b");
        g(&land, &["checkout", "-q", "--detach", &b]);
        let line = publish(&land).expect("publish");
        assert!(
            line.contains("ram upper") && line.contains("tier ram"),
            "{line}"
        );
        let base_b = base_of(&ws, &b);
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("cd \"$1\" && {list}"))
            .arg("sh")
            .arg(&base_b)
            .output()
            .expect("find");
        let strip = |text: &str, extra: &str| -> String {
            text.lines()
                .filter(|l| {
                    !l.starts_with("bootstrap") && !l.starts_with(".base ") && !l.starts_with(extra)
                })
                .map(|l| format!("{l}\n"))
                .collect()
        };
        let got = String::from_utf8(out.stdout).expect("utf-8");
        let want = std::fs::read_to_string(&merged).expect("merged");
        assert!(
            want.contains("dir/y f")
                && !want.contains("gone")
                && want.contains("bootstrap/debug/state"),
            "{want}"
        );
        assert_eq!(
            strip(&got, crate::checkout_stamps::FILE),
            strip(&want, "-"),
            "base/B vs the merged view"
        );
        assert!(
            base_b.join(crate::checkout_stamps::FILE).is_file()
                && !base_b.join(BOOTSTRAP_DIR).exists()
                && !base_b.join(BASE_FILE).exists()
        );
        // laid again: over base/B, gate state carried, nothing else of the old upper
        let rec = crate::delta::record(&dir)
            .expect("record")
            .expect("still ram");
        assert_eq!(rec.base, b);
        let upper = rec.dir(LAND_ID).join("upper");
        let mut names: Vec<_> = std::fs::read_dir(&upper)
            .expect("upper")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [BASE_FILE, BOOTSTRAP_DIR]);
        assert!(upper.join("bootstrap/debug/state").is_file());
        // reset after a failed verify: ram again, gate state kept
        let line = reset_land(&land).expect("recopy");
        assert!(
            line.contains("tier ram") && line.contains("gate state carried"),
            "{line}"
        );
        // the store gone: the pool tier, gate state copied off the store
        std::fs::write(ws.join("workspace.toml"), toml(&ws.join("no-ram-store"))).expect("toml");
        let line = reset_land(&land).expect("recopy to pool");
        assert!(
            line.contains("tier pool") && line.contains("gate state carried"),
            "{line}"
        );
        assert!(crate::delta::record(&dir).expect("record").is_none());
        assert!(dir.join("bootstrap/debug/state").is_file() && dir.join("dir/y").is_file());
        assert!(!rec.dir(LAND_ID).exists(), "the old store dir stays behind");
        // a pool dir that stays pool is kept as it is by publish
        put(&repo.join("h"));
        let c = commit("c");
        g(&land, &["checkout", "-q", "--detach", &c]);
        let line = publish(&land).expect("publish pool");
        assert!(line.contains("stays on tier pool"), "{line}");
        assert!(base_of(&ws, &c).join("dir/y").is_file());
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&store)
            .status();
        std::fs::remove_dir_all(&store).expect("cleanup store");
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The land's gate state is its own: publish leaves it out of the base (other checkouts would reuse it as fresh), and reset_land puts the land's back, so the next land's gate runs warm.
    #[test]
    fn a_land_reset_keeps_the_gate_state() {
        let ws = scratch("recopy");
        let repo = ws.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let g = |args: &[&str]| git(Some(&repo), args).expect("git");
        g(&["init", "-q", "-b", "main"]);
        put(&repo.join("f"));
        g(&["add", "f"]);
        g(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "a",
        ]);
        let head = g(&["rev-parse", "HEAD"]);
        let land = ws.join("land");
        let land_str = land.to_str().expect("utf-8 path");
        g(&["worktree", "add", "-q", "--detach", land_str, head.as_str()]);
        let dir = dir_of(&ws, LAND_ID);
        put(&dir.join(BOOTSTRAP_DIR).join("debug/own"));
        put(&dir.join(TARGET).join("debug/own"));
        // the base as publish copies it (copy_base, without publish's lock)
        std::fs::create_dir_all(pool(&ws).join("base")).expect("base dir");
        copy_base(&dir, &base_of(&ws, &head)).expect("base");
        assert!(
            !base_of(&ws, &head).join(BOOTSTRAP_DIR).exists(),
            "the base carries the gate state"
        );
        reset_land(&land).expect("reset");
        assert!(
            dir.join(BOOTSTRAP_DIR).join("debug/own").is_file(),
            "reset dropped the land's gate state"
        );
        assert!(
            dir.join(TARGET).join("debug/own").is_file(),
            "reset did not copy the base"
        );
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// The threshold: the two measured cases (D11: 0 upstream files changed, README-only and 40 crate/bp2ninja files) keep at the default; any upstream change refreshes.
    #[test]
    fn refresh_wants_an_upstream_change_at_the_default_threshold() {
        let min = crate::ws_config::DEFAULT_REFRESH_MIN_UPSTREAM_FILES;
        assert!(
            !wants_refresh(0, min),
            "D11 cases changed no upstream input"
        );
        assert!(wants_refresh(1, min));
    }

    /// A checkout rebased past a newer base: native keeps its dir while the rebase changed no upstream input, refreshes it from the newest base (keeping target/) once one changed, and is then a no-op; a dir whose base is not older is left alone.
    #[test]
    fn a_rebased_checkout_is_refreshed_from_the_newer_base_keeping_target() {
        let ws = scratch("refresh");
        let repo = ws.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let g = |args: &[&str]| git(Some(&repo), args).expect("git");
        g(&["init", "-q", "-b", "main"]);
        // a commit writing `file` (None = empty)
        let commit = |m: &str, file: Option<&str>| {
            if let Some(f) = file {
                put(&repo.join(f));
                std::fs::write(repo.join(f), m).expect("write");
                g(&["add", f]);
            }
            g(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                m,
            ]);
            g(&["rev-parse", "HEAD"])
        };
        let a = commit("a", None);
        let b = commit("b", Some("crates/x/src/lib.rs"));
        let c = commit("c", Some("patches/aosp/x/0001-x.patch"));
        for (i, c) in [&a, &b, &c].iter().enumerate() {
            let base = base_of(&ws, c);
            put(&base.join("from"));
            std::fs::write(base.join("from"), c.as_str()).expect("mark");
            std::fs::create_dir_all(base.join(TARGET)).expect("base target");
            let t =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::File::open(&base)
                .expect("open")
                .set_modified(t)
                .expect("mtime");
        }
        // HEAD at a: the dir came from base a, nothing newer is an ancestor
        g(&["checkout", "-q", "--detach", &a]);
        std::fs::create_dir_all(pool(&ws).join("ws")).expect("pool ws");
        copy_tree(&base_of(&ws, &a), &dir_of(&ws, MAIN_ID), Some(&a), &[]).expect("copy a");
        put(&dir_of(&ws, MAIN_ID).join("target/debug/own"));
        put(&dir_of(&ws, MAIN_ID).join(BOOTSTRAP_DIR).join("debug/own"));
        let line = refresh(&repo).expect("refresh").expect("line");
        assert!(
            line.starts_with("out: keep") && line.contains("newest"),
            "{line}"
        );
        let dir = dir_of(&ws, MAIN_ID);
        // a small rebase: HEAD at b, only a crate changed -> kept
        g(&["checkout", "-q", "--detach", &b]);
        let line = refresh(&repo).expect("refresh").expect("line");
        assert!(
            line.starts_with("out: keep") && line.contains("0 upstream input files"),
            "{line}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("from")).expect("from"), a);
        // the large rebase: HEAD at c, a patch changed -> refreshed
        g(&["checkout", "-q", "--detach", &c]);
        let line = refresh(&repo).expect("refresh").expect("refreshed");
        assert!(
            line.starts_with("out: refresh")
                && line.contains(&format!("from base/{a} to base/{c}")),
            "{line}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("from")).expect("from"), c);
        assert!(dir.join("target/debug/own").is_file(), "target/ not kept");
        assert!(
            dir.join(BOOTSTRAP_DIR).join("debug/own").is_file(),
            "the gate's target not kept"
        );
        let line = refresh(&repo).expect("refresh").expect("line");
        assert!(line.starts_with("out: keep"), "second refresh: {line}");
        let leftovers: Vec<_> = std::fs::read_dir(dir.parent().expect("parent"))
            .expect("ls")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, [MAIN_ID], "old dir or temp left");
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    /// Boot dirs: a gone checkout's is listed and removed, a live checkout's is never listed, a held orphan is skipped.
    #[test]
    fn boot_dirs_of_gone_checkouts_are_pruned_unless_held() {
        let ws = scratch("boot-orphans");
        for id in ["live", "gone", "held"] {
            put(&pool(&ws).join(BOOT).join(id).join("debug/f"));
        }
        let live = BTreeSet::from([MAIN_ID.to_owned(), "live".to_owned()]);
        let ids = |ws: &Path| -> Vec<String> {
            boot_orphans(ws, &live)
                .expect("boots")
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        assert_eq!(ids(&ws), ["gone", "held"]);
        let lock = build_pool::try_lock_dir(&ws, "held")
            .expect("lock")
            .expect("free");
        for (id, path) in boot_orphans(&ws, &live).expect("boots") {
            let removed = remove_boot(&ws, &id, &path).expect("remove");
            assert_eq!(removed, id == "gone", "{id}");
        }
        assert_eq!(ids(&ws), ["held"]);
        assert!(pool(&ws).join(BOOT).join("live/debug/f").is_file());
        drop(lock);
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn prune_lists_dirs_of_gone_checkouts_and_bases_beyond_the_newest_two() {
        let ws = scratch("orphans");
        for id in ["repo", "live", "gone", ".x.tmp-1"] {
            std::fs::create_dir_all(dir_of(&ws, id)).expect("dir");
        }
        for (i, c) in ["c1", "c2", "c3"].iter().enumerate() {
            let b = base_of(&ws, c);
            std::fs::create_dir_all(&b).expect("base");
            let t =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::File::open(&b)
                .expect("open")
                .set_modified(t)
                .expect("mtime");
        }
        let live = BTreeSet::from(["repo".to_owned(), "live".to_owned()]);
        let (dirs, old, pinned) = orphans(&ws, &live).expect("orphans");
        let ids: Vec<&str> = dirs.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec![".x.tmp-1", "gone"]);
        assert_eq!(old, vec![base_of(&ws, "c1")]);
        assert!(pinned.is_empty());
        assert_eq!(bases(&ws).expect("bases"), vec!["c3", "c2", "c1"]);
        // a live ram dir over c1 keeps it; a gone checkout's ram dir pins nothing
        std::fs::write(
            dir_of(&ws, "live").join(crate::delta::RECORD),
            "base=c1\nstore=/s\n",
        )
        .expect("record");
        std::fs::write(
            dir_of(&ws, "gone").join(crate::delta::RECORD),
            "base=c2\nstore=/s\n",
        )
        .expect("record");
        let (_, old, pinned) = orphans(&ws, &live).expect("orphans");
        assert!(old.is_empty(), "{old:?}");
        assert_eq!(pinned, BTreeSet::from(["c1".to_owned()]));
        std::fs::remove_dir_all(&ws).expect("cleanup");
    }

    #[test]
    fn a_fresh_checkout_links_its_build_dir_target() {
        let d = scratch("link-fresh");
        let root = d.join("wt/this");
        std::fs::create_dir_all(&root).expect("checkout");
        let want = d.join("pool/ws/this/target");
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(
            steps,
            vec![Step::Link {
                at: root.join(TARGET),
                to: want.clone(),
            }]
        );
        apply_steps(&steps).expect("apply");
        assert_eq!(
            entry(&root.join(TARGET)).expect("entry"),
            Entry::Link(want.clone())
        );
        assert!(
            link_steps(&root.join(TARGET), &want)
                .expect("again")
                .is_empty()
        );
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    #[test]
    fn an_own_real_target_is_moved_into_an_absent_build_dir_target() {
        let d = scratch("link-adopt");
        let root = d.join("wt/this");
        put(&root.join(TARGET).join("debug/build.txt"));
        let want = d.join("pool/ws/this/target");
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(
            steps[0],
            Step::Adopt {
                from: root.join(TARGET),
                to: want.clone(),
            }
        );
        apply_steps(&steps).expect("apply");
        assert!(want.join("debug/build.txt").is_file());
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// The build dir on another filesystem (the pool's own mount; here /dev/shm's tmpfs): the own target is removed, not renamed (EXDEV, os error 18).
    #[test]
    fn an_own_real_target_on_another_filesystem_is_discarded() {
        let d = scratch("link-xdev");
        let root = d.join("wt/this");
        put(&root.join(TARGET).join("own.txt"));
        let disk = PathBuf::from("/dev/shm").join(format!("xtask-out-xdev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&disk);
        std::fs::create_dir_all(&disk).expect("disk");
        let want = disk.join(TARGET);
        assert!(
            !same_filesystem(&root.join(TARGET), &want).expect("dev"),
            "fixture: /dev/shm shares the scratch filesystem, the case is not exercised"
        );
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(steps[0], Step::Discard(root.join(TARGET)));
        apply_steps(&steps).expect("apply");
        assert!(want.is_dir());
        std::fs::remove_dir_all(&disk).expect("cleanup disk");
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// The hook's boot target, the build dir made without a target: the bootstrap build moves in (same pool filesystem), the checkout keeps no dir.
    #[test]
    fn a_boot_target_is_moved_into_an_absent_build_dir_target() {
        let d = scratch("link-boot-adopt");
        let root = d.join("wt/this");
        let boot = d.join("pool/boot/this");
        put(&boot.join("debug/xtask"));
        std::fs::create_dir_all(&root).expect("checkout");
        std::os::unix::fs::symlink(&boot, root.join(TARGET)).expect("hook link");
        let want = d.join("pool/ws/this/target");
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(
            steps[0],
            Step::Adopt {
                from: boot.clone(),
                to: want.clone(),
            }
        );
        apply_steps(&steps).expect("apply");
        assert!(want.join("debug/xtask").is_file());
        assert!(!boot.exists());
        assert_eq!(entry(&root.join(TARGET)).expect("entry"), Entry::Link(want));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// The build dir copied from a base has a target: the boot target goes, the link moves to the build dir's.
    #[test]
    fn a_boot_target_is_removed_when_the_build_dir_has_one() {
        let d = scratch("link-boot-remove");
        let root = d.join("wt/this");
        let boot = d.join("pool/boot/this");
        put(&boot.join("debug/xtask"));
        std::fs::create_dir_all(&root).expect("checkout");
        std::os::unix::fs::symlink(&boot, root.join(TARGET)).expect("hook link");
        let want = d.join("pool/ws/this/target");
        put(&want.join("kept.txt"));
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(steps[0], Step::Remove(boot.clone()));
        apply_steps(&steps).expect("apply");
        assert!(!boot.exists() && want.join("kept.txt").is_file());
        assert_eq!(entry(&root.join(TARGET)).expect("entry"), Entry::Link(want));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// Another checkout's boot dir is not ours to move: only relinked.
    #[test]
    fn a_link_to_another_boot_dir_is_only_relinked() {
        let d = scratch("link-boot-other");
        let root = d.join("wt/this");
        let other = d.join("pool/boot/other");
        put(&other.join("x"));
        std::fs::create_dir_all(&root).expect("checkout");
        std::os::unix::fs::symlink(&other, root.join(TARGET)).expect("link");
        let want = d.join("pool/ws/this/target");
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(
            steps,
            vec![Step::Link {
                at: root.join(TARGET),
                to: want,
            }]
        );
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// The generated hook, run as git runs it on `git worktree add` (previous HEAD all zeros) in a fresh repo: `target` links to `<pool>/boot/<git dir name>`, which exists; a checkout switch (non-zero previous HEAD) does nothing.
    #[test]
    fn the_hook_links_a_new_checkouts_target_into_the_pool() {
        let d = scratch("hook-run");
        let repo = d.join("wt/new");
        std::fs::create_dir_all(&repo).expect("repo");
        git(Some(&repo), &["init", "-q"]).expect("git init");
        let hook = d.join("post-checkout");
        assert!(
            write_hook(&hook, &hook_text(&d.join("pool")))
                .expect("write")
                .is_some()
        );
        let run = |prev: &str| {
            let st = Command::new(&hook)
                .args([prev, "1111111111111111111111111111111111111111", "1"])
                .current_dir(&repo)
                .status()
                .expect("run hook");
            assert!(st.success());
        };
        run("2222222222222222222222222222222222222222");
        assert_eq!(entry(&repo.join(TARGET)).expect("entry"), Entry::Absent);
        run("0000000000000000000000000000000000000000");
        let boot = d.join("pool").join(BOOT).join(".git");
        assert_eq!(
            entry(&repo.join(TARGET)).expect("entry"),
            Entry::Link(boot.clone())
        );
        assert!(boot.is_dir());
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    /// Ours is rewritten when its text changes and left alone when equal; a foreign hook is refused with the move-aside command.
    #[test]
    fn a_foreign_hook_is_refused_and_ours_is_kept_current() {
        let d = scratch("hook-write");
        let hook = d.join("hooks/post-checkout");
        assert!(
            write_hook(&hook, &hook_text(Path::new("/p1")))
                .expect("new")
                .is_some()
        );
        assert!(
            write_hook(&hook, &hook_text(Path::new("/p1")))
                .expect("same")
                .is_none()
        );
        assert!(
            write_hook(&hook, &hook_text(Path::new("/p2")))
                .expect("changed")
                .is_some()
        );
        std::fs::write(&hook, "#!/bin/sh\necho mine\n").expect("foreign");
        let err = write_hook(&hook, &hook_text(Path::new("/p2"))).expect_err("refused");
        assert!(format!("{err:#}").contains(".local"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(&hook).expect("kept"),
            "#!/bin/sh\necho mine\n"
        );
        std::fs::remove_dir_all(&d).expect("cleanup");
    }

    #[test]
    fn an_own_real_target_is_removed_when_the_build_dir_has_one() {
        let d = scratch("link-remove");
        let root = d.join("wt/this");
        let want = d.join("pool/ws/this/target");
        put(&want.join("kept.txt"));
        put(&root.join(TARGET).join("own.txt"));
        let steps = link_steps(&root.join(TARGET), &want).expect("steps");
        assert_eq!(steps[0], Step::Remove(root.join(TARGET)));
        apply_steps(&steps).expect("apply");
        assert!(want.join("kept.txt").is_file());
        assert_eq!(entry(&root.join(TARGET)).expect("entry"), Entry::Link(want));
        std::fs::remove_dir_all(&d).expect("cleanup");
    }
}
