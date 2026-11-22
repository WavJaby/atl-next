// SPDX-License-Identifier: MIT
//! Run dirs and seeds: regenerable run data (app data roots, bulky one-off outputs) of a checkout, kept apart from its evidence (logs, screenshots: `out/<task>/`, out_tasks.rs) and never cited by a report.
//!
//! Run dir: one per checkout, keyed by the build dir's id (out.rs `checkout_id`), made on first use by `out show --run` or `out seed use`. Two capability tiers of one interface (the dir path a command prints), chosen when the dir is made and never re-decided while it exists:
//! `ram` = `<delta.ram_store>/.run/<id>/` when the store is RAM-backed and admission (delta.rs, class `run`) fits one data root; `pool` = `out/pool/run/<id>/`, the baseline that works everywhere (same filesystem as the seeds: reflink).
//! The ram dirs sit in their own subtree, not in the build delta's `<store>/<id>/`: a delta refresh, tier change or lost-delta remake drops `<store>/<id>/` whole, and data roots do not depend on the base. A WSL restart empties the store: the record goes with the dir, so nothing dangles; the next make decides afresh.
//! Its `.run` record names its tier (`tier=`), a ram dir's workspace (`workspace=`: one store serves every workspace on the host) and every data root a seed gave it (`use=<name> <seed>`). One line per make (`out: run ram|pool (<why>)`, counters `run-dir-ram|pool`), one per app run (run-apk: [`admit_run`], counters `run-ram|pool`); `run-full` counts a store found full (seed use or after a run), whereupon the next `seed use` moves the checkout's run dir to the pool (design: no mid-run migration).
//! Locks: the build dir's (`build_pool`, `ws-<id>.lock`). Making a run dir or copying a seed into it holds the lock shared, as a build does; leave and prune remove the run dir under the exclusive lock they take for the build dir, so a run dir never goes while a build or run of its checkout holds the lock.
//! Seed `out/pool/seed/<package>-<versionCode>/`: an installed data root, made once (`out seed make`, copied, reflinked when the source is on the pool) and read-only by convention (`.seed` record; modes are kept, so a copy of it is writable). `seed use` reflinks it into a pool run dir (no data bytes written) and copies it into a ram one (pool reads only). `out prune` lists seeds no live run dir names that are not their package's newest (highest versionCode), run dirs whose checkout is gone, and a ram run dir beside a pool one of the same id (made while the store was away).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool;
use crate::delta;
use crate::out;

/// Run dirs under the pool: `run/<id>`.
const RUN_DIRS: &str = "run";
/// Seeds under the pool: `seed/<package>-<versionCode>`.
const SEED_DIRS: &str = "seed";
/// In a run dir: its tier and the seeds it was given.
pub const RECORD: &str = ".run";
/// In a seed: package, versionCode and the data root it was copied from.
const SEED_RECORD: &str = ".seed";
/// Run-dir tier on the pool filesystem, same as the seeds (reflink): the baseline.
const TIER_POOL: &str = "pool";
/// Run-dir tier on the delta store (RAM-backed).
const TIER_RAM: &str = "ram";
/// Ram run dirs under the store: `.run/<id>` (a `.` name: never a checkout id, so never a build delta's `<store>/<id>`).
const RAM_RUN_DIRS: &str = ".run";
/// A run dir record's seed-use key.
const USE_KEY: &str = "use=";
/// A ram run dir record's owner key.
const WORKSPACE_KEY: &str = "workspace=";
/// aapt2 in a build dir (the Java pipeline's host tool, harness atld_run.rs `ApkTools`).
const AAPT2: &str = "host/aapt2/aapt2";

/// `out seed` subcommands.
#[derive(clap::Subcommand)]
pub enum Seed {
    /// Makes seed `out/pool/seed/<package>-<versionCode>` from an installed data root (dry run unless --execute).
    ///
    /// The versionCode is read from `<from>/apps/<package>/apk/base.apk` with aapt2 (`dump badging`, this checkout's build dir, else the newest base). The whole data root is copied (`apps/`, `system/`, `device/`): reflink when it is on the pool, else `cp -a`. A seed that exists is refused: a new app version is a new seed.
    Make {
        /// The app's package name.
        package: String,
        /// The installed data root (atld `--data-root`) to copy.
        #[arg(long, value_name = "DATA_ROOT")]
        from: PathBuf,
        /// Copy (default: print the plan only).
        #[arg(long)]
        execute: bool,
    },
    /// Copies the package's newest seed into this checkout's run dir as `<run>/<name>` (default name: the package) and prints its path; the run dir is made if missing.
    ///
    /// Pool tier: `cp --reflink=always` (no data bytes written). Pass the printed path as `run-apk --data-root`. An existing `<name>` is refused unless --fresh, which removes it first.
    Use {
        /// The app's package name.
        package: String,
        /// The data root's name inside the run dir.
        #[arg(long, value_name = "NAME")]
        into: Option<String>,
        /// Replace an existing `<run>/<name>`.
        #[arg(long)]
        fresh: bool,
    },
    /// Lists every seed: age, newest of its package or not, the run dirs that name it.
    List,
}

/// The pool's run dir of checkout `id`.
pub fn run_dir_of(ws: &Path, id: &str) -> PathBuf {
    out::pool(ws).join(RUN_DIRS).join(id)
}

/// The pool's seed of `package` at `version_code`.
pub fn seed_dir_of(ws: &Path, package: &str, version_code: u64) -> PathBuf {
    out::pool(ws)
        .join(SEED_DIRS)
        .join(seed_name(package, version_code))
}

fn seed_name(package: &str, version_code: u64) -> String {
    format!("{package}-{version_code}")
}

/// A seed dir name's package and versionCode. A package name has no `-` (Android: letters, digits, `_`, `.`), so the last `-` splits.
pub fn parse_seed_name(name: &str) -> Option<(&str, u64)> {
    let (pkg, vc) = name.rsplit_once('-')?;
    is_package(pkg).then_some(())?;
    Some((pkg, vc.parse().ok()?))
}

/// Android's package-name shape (PackageParser `validateName`): two or more `.`-separated segments, each a letter then letters, digits or `_`.
pub fn is_package(name: &str) -> bool {
    let segments: Vec<&str> = name.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|s| {
            let mut c = s.chars();
            c.next().is_some_and(|f| f.is_ascii_alphabetic())
                && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
}

/// A run dir name for a data root: one path component, not hidden, not the record.
fn check_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.starts_with('.') && !name.contains('/') && name != RECORD,
        "--into {name:?}: one path component, not starting with `.`"
    );
    Ok(())
}

/// A run dir's `.run` record.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub tier: String,
    /// The workspace that made a ram run dir (the store is shared by every workspace on the host); None on the pool tier.
    pub workspace: Option<PathBuf>,
    /// Data root name -> seed dir name.
    pub uses: BTreeMap<String, String>,
}

impl Record {
    fn pool() -> Self {
        Self {
            tier: TIER_POOL.to_owned(),
            workspace: None,
            uses: BTreeMap::new(),
        }
    }

    fn ram(ws: &Path) -> Self {
        Self {
            tier: TIER_RAM.to_owned(),
            workspace: Some(ws.to_owned()),
            uses: BTreeMap::new(),
        }
    }

    pub fn is_ram(&self) -> bool {
        self.tier == TIER_RAM
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut tier = None;
        let mut workspace = None;
        let mut uses = BTreeMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(t) = line.strip_prefix("tier=") {
                ensure!(
                    t == TIER_POOL || t == TIER_RAM,
                    "unknown run dir tier {t:?}"
                );
                tier = Some(t.to_owned());
            } else if let Some(w) = line.strip_prefix(WORKSPACE_KEY) {
                workspace = Some(PathBuf::from(w));
            } else if let Some(u) = line.strip_prefix(USE_KEY) {
                let (name, seed) = u
                    .split_once(' ')
                    .with_context(|| format!("bad use line {line:?}"))?;
                uses.insert(name.to_owned(), seed.to_owned());
            } else {
                bail!("unknown run dir record line {line:?}");
            }
        }
        let tier = tier.context("run dir record without tier=")?;
        ensure!(
            (tier == TIER_RAM) == workspace.is_some(),
            "run dir record: `{WORKSPACE_KEY}` belongs to tier {TIER_RAM} only"
        );
        Ok(Self {
            tier,
            workspace,
            uses,
        })
    }

    pub fn text(&self) -> String {
        let mut s = format!("tier={}\n", self.tier);
        if let Some(w) = &self.workspace {
            s.push_str(&format!("{WORKSPACE_KEY}{}\n", w.display()));
        }
        for (name, seed) in &self.uses {
            s.push_str(&format!("{USE_KEY}{name} {seed}\n"));
        }
        s
    }
}

/// The record of run dir `dir`; None when the dir has none (not a run dir, or a crashed make).
pub fn record(dir: &Path) -> Result<Option<Record>> {
    let p = dir.join(RECORD);
    match std::fs::read_to_string(&p) {
        Ok(t) => Record::parse(&t)
            .map(Some)
            .with_context(|| p.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn write_record(dir: &Path, rec: &Record) -> Result<()> {
    let p = dir.join(RECORD);
    let tmp = dir.join(format!("{RECORD}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, rec.text()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("renaming to {}", p.display()))
}

/// The store's ram run dir of checkout `id`.
pub fn ram_run_dir_of(store: &Path, id: &str) -> PathBuf {
    store.join(RAM_RUN_DIRS).join(id)
}

/// Checkout `id`'s run dir and record: the pool's first (one is made only when no ram dir was found, so one beside a ram dir means the store was away when it was made: the ram dir is stale), then the store's when its record names workspace `ws`.
pub fn locate(ws: &Path, store: &Path, id: &str) -> Result<Option<(PathBuf, Record)>> {
    let pool = run_dir_of(ws, id);
    if let Some(rec) = record(&pool)? {
        return Ok(Some((pool, rec)));
    }
    let ram = ram_run_dir_of(store, id);
    // an unreadable record in the shared store (another workspace's, a crash) is not ours
    match record(&ram) {
        Ok(Some(rec)) if rec.workspace.as_deref() == Some(ws) => Ok(Some((ram, rec))),
        _ => Ok(None),
    }
}

/// The run dir of the checkout at `root`, made when missing (through a temp name renamed into place, so a half-made dir never carries a record). Holds the build dir's lock shared while it works; the lock goes with the returned guard.
pub fn ensure(root: &Path) -> Result<(String, PathBuf, build_pool::DirLock)> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let cfg = crate::ws_config::load(&ws)?;
    let id = out::checkout_id(root)?;
    let held = hold(&ws, &id)?;
    let dir = make(&ws, &cfg.delta, &id, None)?;
    Ok((id, dir, held))
}

fn hold(ws: &Path, id: &str) -> Result<build_pool::DirLock> {
    build_pool::hold_dir(ws, id, || {
        eprintln!("out: run/{id} is being pruned or removed, waiting")
    })
}

/// The tier a new run dir of `id` takes: ram when the store probes RAM-backed and admission fits one data root of class `run` (the reservation ends with this call: the dir holds nothing yet), else pool. Returns (ram?, the line's reason).
fn choose(
    ws: &Path,
    id: &str,
    cfg: &crate::ws_config::DeltaConfig,
    probed: std::result::Result<delta::Stats, String>,
) -> Result<(bool, String)> {
    Ok(
        match delta::admit_once(ws, id, cfg, delta::Class::Run, probed)? {
            Ok((_reservation, stats, need_ram)) => (
                true,
                format!(
                    "class run fits: {:.2} GiB RAM per data root; store {}: {}",
                    need_ram as f64 / GIB,
                    cfg.ram_store.value.display(),
                    stats.line()
                ),
            ),
            Err(v) => {
                if matches!(v, delta::Verdict::Refuse(_)) {
                    out::count(ws, "run-full");
                }
                (false, format!("class run: {}", v.reason()))
            }
        },
    )
}

const GIB: f64 = 1_073_741_824.0;

/// [`ensure`]'s work once the lock is held: an existing run dir is returned as it is; a new one takes the tier [`choose`] picks (`pool_only`: the pool, said why by the caller).
fn make(
    ws: &Path,
    cfg: &crate::ws_config::DeltaConfig,
    id: &str,
    pool_only: Option<&str>,
) -> Result<PathBuf> {
    let store = &cfg.ram_store.value;
    if let Some((dir, _)) = locate(ws, store, id)? {
        return Ok(dir);
    }
    let (ram, why) = if let Some(why) = pool_only {
        (false, why.to_owned())
    } else {
        choose(ws, id, cfg, delta::probe(store, cfg)?)?
    };
    let (dir, rec) = if ram {
        (ram_run_dir_of(store, id), Record::ram(ws))
    } else {
        (run_dir_of(ws, id), Record::pool())
    };
    // stderr: `out show --run` prints only the path on stdout
    eprintln!("out: run {} ({why})", rec.tier);
    out::count(ws, &format!("run-dir-{}", rec.tier));
    let parent = dir.parent().context("run dir has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    if out::present(&dir)? {
        // a dir without a record: made by hand or by a crash after rename (cannot happen: the record is written before)
        bail!(
            "{} exists without its {RECORD} record: not a run dir xtask made; move it away",
            dir.display()
        );
    }
    let tmp = parent.join(format!(".{id}.tmp-{}", std::process::id()));
    if out::present(&tmp)? {
        std::fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    std::fs::create_dir(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    write_record(&tmp, &rec)?;
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => {
            eprintln!("out: run/{id} made: {} (tier {})", dir.display(), rec.tier);
        }
        // another process made it meanwhile (ENOTEMPTY / EEXIST): theirs stands
        Err(_) if record(&dir)?.is_some() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e).with_context(|| format!("renaming to {}", dir.display()));
        }
    }
    Ok(dir)
}

/// Run dirs under the pool: (id, path), sorted; temp names (`.`-prefixed) included.
pub fn run_dirs(ws: &Path) -> Result<Vec<(String, PathBuf)>> {
    list(&out::pool(ws).join(RUN_DIRS))
}

/// Ram run dirs in `store` whose record names workspace `ws`: (id, path), sorted. Temp names count when their record does (a crash between record and rename).
pub fn ram_run_dirs(store: &Path, ws: &Path) -> Result<Vec<(String, PathBuf)>> {
    Ok(list(&store.join(RAM_RUN_DIRS))?
        .into_iter()
        .filter(|(_, p)| {
            record(p)
                .ok()
                .flatten()
                .is_some_and(|r| r.workspace.as_deref() == Some(ws))
        })
        .collect())
}

/// Every run dir of workspace `ws`, both tiers: (id, path).
pub fn all_run_dirs(ws: &Path, store: &Path) -> Result<Dirs> {
    let mut all = run_dirs(ws)?;
    all.extend(ram_run_dirs(store, ws)?);
    Ok(all)
}

fn list(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let Ok(rd) = std::fs::read_dir(dir) else {
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

/// One seed dir: its name `<package>-<versionCode>`, the two keys, its path.
pub struct SeedDir {
    pub name: String,
    pub package: String,
    pub version_code: u64,
    pub path: PathBuf,
}

/// `(name, path)` of dirs.
pub type Dirs = Vec<(String, PathBuf)>;

/// Seeds under the pool, sorted by name; temp names and foreign entries skipped.
pub fn seeds(ws: &Path) -> Result<Vec<SeedDir>> {
    Ok(list(&out::pool(ws).join(SEED_DIRS))?
        .into_iter()
        .filter_map(|(name, path)| {
            let (pkg, vc) = parse_seed_name(&name)?;
            Some(SeedDir {
                package: pkg.to_owned(),
                version_code: vc,
                name,
                path,
            })
        })
        .collect())
}

/// What `out prune` removes of run data: run dirs whose id is not in `live`, and seeds that are neither their package's newest nor named by a kept run dir's record. Returns (orphan run dirs, old seeds).
/// Ram run dirs count as orphans also when a pool run dir of the same id exists ([`locate`] reads the pool's).
pub fn orphans(ws: &Path, store: &Path, live: &BTreeSet<String>) -> Result<(Dirs, Dirs)> {
    let pool_ids: BTreeSet<String> = run_dirs(ws)?.into_iter().map(|(id, _)| id).collect();
    let ram: Vec<_> = ram_run_dirs(store, ws)?
        .into_iter()
        .map(|(id, p)| {
            let stale = pool_ids.contains(&id);
            (id, p, stale)
        })
        .collect();
    let (gone, kept): (Vec<_>, Vec<_>) = run_dirs(ws)?
        .into_iter()
        .map(|(id, p)| (id, p, false))
        .chain(ram)
        .partition(|(id, _, stale)| *stale || !live.contains(id));
    let gone: Dirs = gone.into_iter().map(|(id, p, _)| (id, p)).collect();
    let kept: Dirs = kept.into_iter().map(|(id, p, _)| (id, p)).collect();
    let mut named = BTreeSet::new();
    for (_, path) in &kept {
        if let Some(rec) = record(path)? {
            named.extend(rec.uses.into_values());
        }
    }
    let all = seeds(ws)?;
    Ok((gone, old_seeds(&all, &named)))
}

/// The seeds of `all` that are not the newest (highest versionCode) of their package and that no name in `named` keeps.
fn old_seeds(all: &[SeedDir], named: &BTreeSet<String>) -> Dirs {
    let mut newest: BTreeMap<&str, u64> = BTreeMap::new();
    for s in all {
        let n = newest.entry(&s.package).or_insert(s.version_code);
        *n = (*n).max(s.version_code);
    }
    all.iter()
        .filter(|s| newest[s.package.as_str()] != s.version_code && !named.contains(&s.name))
        .map(|s| (s.name.clone(), s.path.clone()))
        .collect()
}

/// Removes run dir `dir` (the caller holds the build dir's lock exclusive). Data roots carry app-made modes (a 0500 dir would stop `remove_dir_all`), so dirs get `u+rwx` first.
pub fn remove(dir: &Path) -> Result<()> {
    let status = Command::new("chmod")
        .args(["-R", "u+rwX", "--"])
        .arg(dir)
        .status()
        .context("running chmod")?;
    ensure!(
        status.success(),
        "chmod -R u+rwX {}: {status}",
        dir.display()
    );
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

/// `versionCode` and `name` of aapt2's `dump badging` first line, `package: name='<p>' versionCode='<n>' versionName='<v>' ...`.
pub fn parse_badging(text: &str) -> Result<(String, u64)> {
    let line = text
        .lines()
        .find(|l| l.starts_with("package:"))
        .context("aapt2 dump badging printed no `package:` line")?;
    let field = |key: &str| -> Result<&str> {
        let start = line
            .find(&format!(" {key}='"))
            .with_context(|| format!("no {key} in {line:?}"))?
            + key.len()
            + 3;
        let len = line[start..]
            .find('\'')
            .with_context(|| format!("unterminated {key} in {line:?}"))?;
        Ok(&line[start..start + len])
    };
    let vc = field("versionCode")?;
    Ok((
        field("name")?.to_owned(),
        vc.parse()
            .with_context(|| format!("versionCode {vc:?} is not a number"))?,
    ))
}

/// The build dir whose aapt2 `seed make` runs: this checkout's (pool tier: a ram dir is an empty mount point out here), else the base a new dir would start from.
fn aapt2_build(root: &Path, ws: &Path) -> Result<PathBuf> {
    let own = out::dir_of(ws, &out::checkout_id(root)?);
    if own.join(AAPT2).is_file() {
        return Ok(own);
    }
    if let Some(c) = out::pick_base(root, ws)? {
        let b = out::base_of(ws, &c);
        if b.join(AAPT2).is_file() {
            return Ok(b);
        }
    }
    bail!(
        "no aapt2 in this checkout's build dir or the base ({AAPT2}): run `cargo xtask native --host x86_64` first"
    )
}

/// `aapt2 dump badging <apk>` with `build` bound at the canonical build path: aapt2's RUNPATH names `out/build/host/<lib>` (out.rs CANONICAL), so it runs only where a build dir is bound there, as in the build namespace (tree.rs `namespace_args`, the same bind).
fn badging(root: &Path, ws: &Path, apk: &Path) -> Result<String> {
    let build = aapt2_build(root, ws)?;
    let canonical = ws.join(out::CANONICAL);
    ensure!(
        canonical.is_dir(),
        "{} missing: run any build command once (it makes the mount point)",
        canonical.display()
    );
    let bind = crate::tree::Overlay {
        project: out::CANONICAL.to_owned(),
        copy: build,
        target: canonical.clone(),
    };
    let out = Command::new(atl_bwrap::PROGRAM)
        .args(crate::tree::namespace_args(&[bind]).end())
        .arg(canonical.join(AAPT2))
        .args(["dump", "badging"])
        .arg(apk)
        .output()
        .context("running aapt2 under bwrap")?;
    ensure!(
        out.status.success(),
        "aapt2 dump badging {}: {}",
        apk.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `out seed`.
pub fn run(root: &Path, cmd: &Seed) -> Result<ExitCode> {
    let ws = Layout::of(root)?.workspace().to_owned();
    match cmd {
        Seed::Make {
            package,
            from,
            execute,
        } => make_seed(root, &ws, package, from, *execute),
        Seed::Use {
            package,
            into,
            fresh,
        } => {
            let path = use_seed(root, &ws, package, into.as_deref(), *fresh)?;
            println!("{}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        Seed::List => {
            let live = out::live_ids_of(root)?;
            let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let store = crate::ws_config::load(&ws)?.delta.ram_store.value;
            for (id, path) in all_run_dirs(&ws, &store)? {
                if let Some(rec) = record(&path)? {
                    let state = if live.contains(&id) { "" } else { " (orphan)" };
                    for (name, seed) in rec.uses {
                        users
                            .entry(seed)
                            .or_default()
                            .push(format!("run/{id}/{name}{state}"));
                    }
                }
            }
            let all = seeds(&ws)?;
            let old: BTreeSet<String> = old_seeds(&all, &BTreeSet::new())
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            for SeedDir { name, path, .. } in &all {
                let rank = if old.contains(name) { "old" } else { "newest" };
                let by = users
                    .get(name)
                    .map_or("no run dir".to_owned(), |u| u.join(", "));
                println!("seed {name}: {rank}, {}, used by {by}", age(path));
            }
            println!(
                "out seed: {} seed(s) under {}",
                all.len(),
                out::pool(&ws).join(SEED_DIRS).display()
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn age(path: &Path) -> String {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or("?".to_owned(), |d| {
            format!("{:.1} h old", d.as_secs_f64() / 3600.0)
        })
}

fn make_seed(
    root: &Path,
    ws: &Path,
    package: &str,
    from: &Path,
    execute: bool,
) -> Result<ExitCode> {
    ensure!(is_package(package), "{package:?} is not a package name");
    let from = std::path::absolute(from)?;
    let apk = from.join("apps").join(package).join("apk").join("base.apk");
    ensure!(
        apk.is_file(),
        "{} missing: {} is not a data root with {package} installed (run it once with run-apk --data-root)",
        apk.display(),
        from.display()
    );
    let (name, vc) = parse_badging(&badging(root, ws, &apk)?)?;
    ensure!(
        name == package,
        "{} declares package {name}, not {package}",
        apk.display()
    );
    let dst = seed_dir_of(ws, package, vc);
    ensure!(
        !out::present(&dst)?,
        "seed {} exists: seeds are made once per versionCode",
        dst.display()
    );
    if !execute {
        println!(
            "out seed: would copy {} -> {} (dry run; add --execute)",
            from.display(),
            dst.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let parent = dst.parent().context("seed has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tier = out::copy_tree(&from, &dst, None, &[])?;
    let mark = dst.join(SEED_RECORD);
    std::fs::write(
        &mark,
        format!(
            "package={package}\nversion_code={vc}\nfrom={}\n",
            from.display()
        ),
    )
    .with_context(|| format!("writing {}", mark.display()))?;
    println!(
        "out seed: {} made from {} ({})",
        dst.display(),
        from.display(),
        tier.name()
    );
    Ok(ExitCode::SUCCESS)
}

/// `seed use`: the package's newest seed copied into the run dir as `into` (default: the package); returns the data root's path.
fn use_seed(
    root: &Path,
    ws: &Path,
    package: &str,
    into: Option<&str>,
    fresh: bool,
) -> Result<PathBuf> {
    ensure!(is_package(package), "{package:?} is not a package name");
    let name = into.unwrap_or(package);
    check_name(name)?;
    let SeedDir {
        name: seed,
        path: src,
        ..
    } = seeds(ws)?
        .into_iter()
        .filter(|s| s.package == package)
        .max_by_key(|s| s.version_code)
        .with_context(|| {
            format!(
                "no seed of {package} under {}: make one with `cargo xtask out seed make {package} --from <data-root> --execute`",
                out::pool(ws).join(SEED_DIRS).display()
            )
        })?;
    let cfg = crate::ws_config::load(ws)?;
    let id = out::checkout_id(root)?;
    let mut held = hold(ws, &id)?;
    let mut dir = make(ws, &cfg.delta, &id, None)?;
    let rec = record(&dir)?.context("run dir lost its record")?;
    // a ram run dir admits this data root's growth for the copy; a full store moves the run dir to the pool (its data is regenerable)
    let mut _reservation = None;
    if rec.is_ram() {
        let probed = delta::probe(&cfg.delta.ram_store.value, &cfg.delta)?;
        match delta::admit_once(ws, &id, &cfg.delta, delta::Class::Run, probed)? {
            Ok((r, _, _)) => _reservation = Some(r),
            Err(v) => {
                if matches!(v, delta::Verdict::Refuse(_)) {
                    out::count(ws, "run-full");
                }
                drop(held);
                let Some(_excl) = build_pool::try_lock_dir(ws, &id)? else {
                    bail!(
                        "run/{id} is on the ram store, which cannot take another data root ({}), and a running command holds it: not moved to the pool; retry when it ends",
                        v.reason()
                    );
                };
                remove(&dir)?;
                eprintln!(
                    "out: run/{id}: ram run dir {} dropped ({}): remade on the pool, its data roots are gone",
                    dir.display(),
                    v.reason()
                );
                drop(_excl);
                held = hold(ws, &id)?;
                dir = make(ws, &cfg.delta, &id, Some("moved off a full store"))?;
            }
        }
    }
    let _held = held;
    let dst = dir.join(name);
    if out::present(&dst)? {
        ensure!(
            fresh,
            "{} exists: pass --fresh to replace it with a copy of seed {seed}",
            dst.display()
        );
        remove(&dst)?;
    }
    if !rec.is_ram() || _reservation.is_none() {
        crate::disk_floor::pool_gate(ws, &cfg)?;
    }
    let tier = out::copy_tree(&src, &dst, None, &[SEED_RECORD])?;
    let mut rec = record(&dir)?.context("run dir lost its record")?;
    rec.uses.insert(name.to_owned(), seed.clone());
    write_record(&dir, &rec)?;
    out::count(ws, &format!("seed-{}", tier.name()));
    eprintln!("out: run/{id}/{name} from seed {seed} ({})", tier.name());
    Ok(dst)
}

/// `out tier pool|ram`'s run-dir half: a run dir on the other tier is dropped (its data roots are regenerable) and made again, on the pool for `pool`, by [`choose`] for `ram` (which may still say pool, with its reason). Dry run unless `execute`.
pub fn set_tier(
    root: &Path,
    ws: &Path,
    cfg: &crate::ws_config::DeltaConfig,
    ram: bool,
    execute: bool,
) -> Result<()> {
    let id = out::checkout_id(root)?;
    let Some((dir, rec)) = locate(ws, &cfg.ram_store.value, &id)? else {
        return Ok(());
    };
    let want = if ram { TIER_RAM } else { TIER_POOL };
    if rec.tier == want {
        println!("out tier: run/{id} is on tier {want} already");
        return Ok(());
    }
    if !execute {
        println!(
            "out tier: would drop run/{id} ({}, {} data root(s)) and make it on tier {want} (dry run; add --execute)",
            rec.tier,
            rec.uses.len()
        );
        return Ok(());
    }
    {
        let Some(_lock) = build_pool::try_lock_dir(ws, &id)? else {
            bail!("run/{id} is held by a running command: not changed");
        };
        remove(&dir)?;
    }
    let _held = hold(ws, &id)?;
    let dir = make(ws, cfg, &id, (!ram).then_some("out tier pool"))?;
    println!("out tier: run/{id} now {}", dir.display());
    Ok(())
}

/// An app run on a run dir's data root, from [`admit_run`] until the run ends: a ram run's reservation (class `run`, the run's growth) and the store check at the end.
pub struct RunAdmission {
    ws: PathBuf,
    id: String,
    cfg: crate::ws_config::DeltaConfig,
    ram: bool,
    _reservation: Option<delta::Reservation>,
}

/// Which run dir holds `data_root`: (id, ram?) when it lies inside one of workspace `ws`'s (either tier), else None (a gate's own root, the user's).
pub fn run_dir_holding(ws: &Path, store: &Path, data_root: &Path) -> Option<(String, bool)> {
    let p = std::path::absolute(data_root).ok()?;
    let p = std::fs::canonicalize(&p).unwrap_or(p);
    for (base, ram) in [
        (out::pool(ws).join(RUN_DIRS), false),
        (store.join(RAM_RUN_DIRS), true),
    ] {
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        if let Ok(rest) = p.strip_prefix(&base)
            && let Some(std::path::Component::Normal(id)) = rest.components().next()
        {
            let id = id.to_string_lossy().into_owned();
            let dir = base.join(&id);
            let ours = record(&dir)
                .ok()
                .flatten()
                .is_some_and(|r| r.is_ram() == ram && (!ram || r.workspace.as_deref() == Some(ws)));
            return ours.then_some((id, ram));
        }
    }
    None
}

/// run-apk's admission of an app run on `data_root`: one line + counter naming the run's tier (`run-ram` / `run-pool`); on the ram tier a class `run` reservation for the run's life. A ram run that admission refuses still runs (its data root is already on the store; a full store fails it cleanly), counted `run-full`. None = not a run dir's data root.
pub fn admit_run(root: &Path, data_root: &Path) -> Result<Option<RunAdmission>> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let cfg = crate::ws_config::load(&ws)?.delta;
    let Some((id, ram)) = run_dir_holding(&ws, &cfg.ram_store.value, data_root) else {
        return Ok(None);
    };
    let mut reservation = None;
    if ram {
        let probed = delta::probe(&cfg.ram_store.value, &cfg)?;
        match delta::admit_once(&ws, &id, &cfg, delta::Class::Run, probed)? {
            Ok((r, stats, need_ram)) => {
                println!(
                    "out: run ram (app run in run/{id}, {:.2} GiB RAM reserved; store {}: {})",
                    need_ram as f64 / GIB,
                    cfg.ram_store.value.display(),
                    stats.line()
                );
                reservation = Some(r);
            }
            Err(v) => {
                if matches!(v, delta::Verdict::Refuse(_)) {
                    out::count(&ws, "run-full");
                }
                println!(
                    "out: run ram (app run in run/{id}, not admitted: {}; runs on its data root, a full store fails it cleanly; the next `out seed use` moves run/{id} to the pool)",
                    v.reason()
                );
            }
        }
    } else {
        println!("out: run pool (app run in run/{id})");
    }
    out::count(&ws, if ram { "run-ram" } else { "run-pool" });
    Ok(Some(RunAdmission {
        ws,
        id,
        cfg,
        ram,
        _reservation: reservation,
    }))
}

impl Drop for RunAdmission {
    /// After a ram run: a store that cannot take another data root (statfs or zram RAM, delta::judge Refuse) counts `run-full`.
    fn drop(&mut self) {
        if !self.ram {
            return;
        }
        self._reservation = None;
        let Ok(Ok(stats)) = delta::probe(&self.cfg.ram_store.value, &self.cfg) else {
            return;
        };
        let v = delta::judge(
            &stats,
            delta::Class::Run.peak(&self.cfg),
            0,
            self.cfg.ram_margin_gib.value << 30,
        );
        if matches!(v, delta::Verdict::Refuse(_)) {
            out::count(&self.ws, "run-full");
            println!(
                "out: run/{}: store full after the run ({}): the next `out seed use` moves it to the pool",
                self.id,
                v.reason()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-run-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// Paths follow the layout the module doc states: `out/pool/run/<id>`, `out/pool/seed/<package>-<versionCode>`.
    #[test]
    fn paths() {
        let ws = Path::new("/w");
        assert_eq!(run_dir_of(ws, "t1"), Path::new("/w/out/pool/run/t1"));
        assert_eq!(
            seed_dir_of(ws, "com.tencent.mm", 2800),
            Path::new("/w/out/pool/seed/com.tencent.mm-2800")
        );
    }

    /// Seed keys: package + versionCode round-trip; a name that is no `<package>-<number>` is not a seed.
    #[test]
    fn seed_keys() {
        assert_eq!(
            parse_seed_name("org.fossify.math-10"),
            Some(("org.fossify.math", 10))
        );
        assert_eq!(
            parse_seed_name("com.tencent.mm-2800"),
            Some(("com.tencent.mm", 2800))
        );
        for bad in [
            "math-10",
            "org.fossify.math",
            "org.fossify.math-x",
            ".org.a-1",
            "org.a-b.c-1",
            "org.1a-2",
        ] {
            assert_eq!(parse_seed_name(bad), None, "{bad}");
        }
    }

    /// The badging line shape aapt2 prints (AOSP frameworks/base/tools/aapt2/dump/DumpManifest.cpp `ManifestExtractor` package line); versionCode 10 = test-apks.toml fossify-math.
    #[test]
    fn badging() {
        let text = "package: name='org.fossify.math' versionCode='10' versionName='1.0.0' platformBuildVersionName='14'\nsdkVersion:'26'\n";
        assert_eq!(
            parse_badging(text).unwrap(),
            ("org.fossify.math".to_owned(), 10)
        );
        assert!(parse_badging("sdkVersion:'26'\n").is_err());
        assert!(parse_badging("package: name='a.b' versionCode='x'\n").is_err());
    }

    #[test]
    fn record_round_trip() {
        let mut rec = Record::pool();
        rec.uses.insert("wx".into(), "com.tencent.mm-2800".into());
        let text = rec.text();
        assert_eq!(text, "tier=pool\nuse=wx com.tencent.mm-2800\n");
        assert_eq!(Record::parse(&text).unwrap(), rec);
        let ram = Record::ram(Path::new("/w"));
        assert_eq!(ram.text(), "tier=ram\nworkspace=/w\n");
        assert_eq!(Record::parse(&ram.text()).unwrap(), ram);
        // a ram dir without its owner, or a pool dir with one, is not a record xtask writes
        assert!(Record::parse("tier=ram\n").is_err());
        assert!(Record::parse("tier=pool\nworkspace=/w\n").is_err());
        assert!(Record::parse("tier=disk\n").is_err());
        assert!(Record::parse("use=a b\n").is_err());
    }

    /// A delta config whose store is `store`, with `extra` lines under `[delta]`.
    fn cfg_at(store: &Path, extra: &str) -> crate::ws_config::DeltaConfig {
        crate::ws_config::parse(
            &format!("[delta]\nram_store = \"{}\"\n{extra}", store.display()),
            Path::new("/w/workspace.toml"),
        )
        .unwrap()
        .delta
    }

    fn counter(ws: &Path, name: &str) -> u64 {
        std::fs::read_to_string(out::pool(ws).join(format!("{name}.count")))
            .map_or(0, |t| t.trim().parse().unwrap())
    }

    /// A RAM-backed scratch store: a dir on /dev/shm (tmpfs, which delta::probe reads as RAM-backed, ratio 1).
    fn shm_store(name: &str) -> Option<PathBuf> {
        let s = Path::new("/dev/shm").join(format!("xtask-run-{}-{name}", std::process::id()));
        Path::new("/dev/shm").is_dir().then(|| {
            let _ = std::fs::remove_dir_all(&s);
            std::fs::create_dir_all(&s).unwrap();
            s
        })
    }

    /// Tier choice by capability: a RAM-backed store that fits class `run` gives a ram dir (record + counter); a store absent, one that can never hold the class, and one full by its own accounting give a pool dir, each with its line's reason and `run-dir-pool` (full: also `run-full`).
    #[test]
    fn ram_tier_by_probe_and_admission_else_pool() {
        let Some(store) = shm_store("tiers") else {
            return;
        };
        let ws = scratch("tiers");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        // 0.01 GiB = 10 MiB (thousandths * 1024 / 1000): fits any tmpfs with MemAvailable past the 1 GiB margin
        let small = cfg_at(&store, "peak_run_gib = 0.01\nram_margin_gib = 0\n");
        let dir = make(&ws, &small, "r", None).unwrap();
        assert_eq!(dir, ram_run_dir_of(&store, "r"));
        assert_eq!(record(&dir).unwrap(), Some(Record::ram(&ws)));
        assert_eq!(counter(&ws, "run-dir-ram"), 1);
        assert_eq!(locate(&ws, &store, "r").unwrap().unwrap().0, dir);
        // another workspace never sees it
        assert!(
            locate(Path::new("/elsewhere"), &store, "r")
                .unwrap()
                .is_none()
        );
        // the store absent: pool
        let gone = cfg_at(&store.join("absent"), "");
        assert_eq!(make(&ws, &gone, "a", None).unwrap(), run_dir_of(&ws, "a"));
        // 1 PiB per data root: never fits -> pool
        let huge = cfg_at(&store, "peak_run_gib = 1048576\n");
        assert_eq!(make(&ws, &huge, "h", None).unwrap(), run_dir_of(&ws, "h"));
        assert_eq!(counter(&ws, "run-dir-pool"), 2);
        assert_eq!(counter(&ws, "run-full"), 0);
        // full by its own accounting: zram limit 10 GiB with 9.5 used, the default class (1612 MiB) at ratio 1.95 needs 0.81 + 1 margin > 0.5 free
        let full = delta::Stats {
            backing: delta::Backing::Zram,
            fs_size: 20 << 30,
            fs_free: 10 << 30,
            ram_limit: 10 << 30,
            ram_used: 19 << 29,
            ratio_milli: 1950,
            ratio_measured: true,
        };
        let def = cfg_at(&store, "");
        let (ram, why) = choose(&ws, "f", &def, Ok(full)).unwrap();
        assert!(!ram && why.contains("RAM free"), "{why}");
        assert_eq!(counter(&ws, "run-full"), 1);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A WSL restart empties the store: the ram dir and its record go together, nothing on the pool names it, prune lists nothing, and the next make decides afresh.
    #[test]
    fn wiped_store_leaves_nothing_dangling() {
        let Some(store) = shm_store("wipe") else {
            return;
        };
        let ws = scratch("wipe");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = cfg_at(&store, "peak_run_gib = 0.01\nram_margin_gib = 0\n");
        let dir = make(&ws, &cfg, "r", None).unwrap();
        std::fs::create_dir_all(dir.join("wx/apps")).unwrap();
        std::fs::remove_dir_all(&store).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        assert!(locate(&ws, &store, "r").unwrap().is_none());
        let live = BTreeSet::from(["r".to_owned()]);
        let (gone, old) = orphans(&ws, &store, &live).unwrap();
        assert!(gone.is_empty() && old.is_empty());
        assert!(!run_dir_of(&ws, "r").exists());
        assert_eq!(make(&ws, &cfg, "r", None).unwrap(), dir);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Prune: a ram run dir of a gone checkout, and one beside a pool dir of the same id (made while the store was away), are orphans; a live one is kept; another workspace's is never listed.
    #[test]
    fn ram_orphans() {
        let Some(store) = shm_store("orph") else {
            return;
        };
        let ws = scratch("orph");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = cfg_at(&store, "peak_run_gib = 0.01\nram_margin_gib = 0\n");
        for id in ["live", "gone", "both"] {
            make(&ws, &cfg, id, None).unwrap();
        }
        // the pool dir a make wrote while the store was away (make itself finds the ram one first)
        let both = run_dir_of(&ws, "both");
        std::fs::create_dir_all(&both).unwrap();
        write_record(&both, &Record::pool()).unwrap();
        let theirs = ram_run_dir_of(&store, "x");
        std::fs::create_dir_all(&theirs).unwrap();
        write_record(&theirs, &Record::ram(Path::new("/other"))).unwrap();
        let live = BTreeSet::from(["live".to_owned(), "both".to_owned()]);
        let (gone, _) = orphans(&ws, &store, &live).unwrap();
        let got: Vec<_> = gone
            .iter()
            .map(|(id, p)| (id.as_str(), p.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("both", ram_run_dir_of(&store, "both")),
                ("gone", ram_run_dir_of(&store, "gone"))
            ]
        );
        // which run dir holds a data root: the tier and id of its run dir, nothing for a path outside every run dir
        assert_eq!(
            run_dir_holding(&ws, &store, &ram_run_dir_of(&store, "live").join("wx")),
            Some(("live".to_owned(), true))
        );
        assert_eq!(
            run_dir_holding(&ws, &store, &run_dir_of(&ws, "both").join("wx")),
            Some(("both".to_owned(), false))
        );
        assert_eq!(run_dir_holding(&ws, &store, &theirs.join("wx")), None);
        assert_eq!(run_dir_holding(&ws, &store, &ws.join("data")), None);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Prune keeps each package's newest seed and every seed a kept run dir names; an orphan run dir's names keep nothing.
    #[test]
    fn prune_listing() {
        let ws = scratch("prune");
        let pool = out::pool(&ws);
        for s in ["a.b-1", "a.b-2", "a.b-3", "c.d-7", "not-a-seed"] {
            std::fs::create_dir_all(pool.join(SEED_DIRS).join(s)).unwrap();
        }
        for (id, uses) in [("live", "use=x a.b-1\n"), ("gone", "use=y a.b-2\n")] {
            let d = run_dir_of(&ws, id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(RECORD), format!("tier=pool\n{uses}")).unwrap();
        }
        let live = BTreeSet::from(["live".to_owned()]);
        let (gone, old) = orphans(&ws, &ws.join("no-store"), &live).unwrap();
        assert_eq!(
            gone.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(),
            ["gone"]
        );
        // a.b-3 newest, c.d-7 newest, a.b-1 named by live: only a.b-2 goes
        assert_eq!(
            old.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["a.b-2"]
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A made run dir carries its record; a second make returns it unchanged; removal clears app-made read-only dirs.
    #[test]
    fn make_and_remove() {
        let ws = scratch("make");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = cfg_at(&ws.join("no-store"), "");
        let dir = make(&ws, &cfg, "t", None).unwrap();
        assert_eq!(dir, run_dir_of(&ws, "t"));
        assert_eq!(record(&dir).unwrap(), Some(Record::pool()));
        let ro = dir.join("root/locked");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("f"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(make(&ws, &cfg, "t", None).unwrap(), dir);
        remove(&dir).unwrap();
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
