// SPDX-License-Identifier: MIT
//! Run dirs: regenerable run data (app data roots, bulky one-off outputs) of a checkout, kept apart from its evidence (logs, screenshots: `out/<task>/`, out_tasks.rs) and never cited by a report.
//!
//! Run dir: one per checkout, keyed by the build dir's id (out.rs `checkout_id`), made on first use by `out show --run` or an app run's [`fresh_root`]. Two capability tiers of one interface (the dir path a command prints), chosen when the dir is made and never re-decided while it exists:
//! `ram` = `<delta.ram_store>/.run/<id>/` when the store is RAM-backed and admission (delta.rs, class `run`) fits one data root; `pool` = `out/pool/run/<id>/`, the baseline that works everywhere.
//! The ram dirs sit in their own subtree, not in the build delta's `<store>/<id>/`: a delta refresh, tier change or lost-delta remake drops `<store>/<id>/` whole, and data roots do not depend on the base. A WSL restart empties the store: the record goes with the dir, so nothing dangles; the next make decides afresh.
//! Its `.run` record names its tier (`tier=`) and a ram dir's workspace (`workspace=`: one store serves every workspace on the host). One line per make (`out: run ram|pool (<why>)`, counters `run-dir-ram|pool`), one per app run (run-apk: [`admit_run`], counters `run-ram|pool`); `run-full` counts a store found full (at a fresh root or after a run), whereupon the next fresh root moves the checkout's run dir to the pool (no mid-run migration).
//! Data roots (maintainer 2026-11-22): every app run starts from a clean install on an empty root, `<run>/<name>` emptied and made by [`fresh_root`]; nothing is copied in or carried over from an earlier run. The last root of each name stays for inspection until the next run of that name, leave or prune.
//! Locks: the build dir's (`build_pool`, `ws-<id>.lock`). Making a run dir or a fresh root holds the lock shared, as a build does; leave and prune remove the run dir under the exclusive lock they take for the build dir, so a run dir never goes while a build or run of its checkout holds the lock. The one removal under the shared lock: [`fresh_root`] dropping its own checkout's ram run dir off a full store.
//! `out prune` lists run dirs whose checkout is gone and a ram run dir beside a pool one of the same id (made while the store was away).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool;
use crate::delta;
use crate::out;

/// Run dirs under the pool: `run/<id>`.
const RUN_DIRS: &str = "run";
/// In a run dir: its tier (and a ram dir's workspace).
pub const RECORD: &str = ".run";
/// Run-dir tier on the pool filesystem: the baseline.
const TIER_POOL: &str = "pool";
/// Run-dir tier on the delta store (RAM-backed).
const TIER_RAM: &str = "ram";
/// Ram run dirs under the store: `.run/<id>` (a `.` name: never a checkout id, so never a build delta's `<store>/<id>`).
const RAM_RUN_DIRS: &str = ".run";
/// A ram run dir record's owner key.
const WORKSPACE_KEY: &str = "workspace=";

/// The pool's run dir of checkout `id`.
pub fn run_dir_of(ws: &Path, id: &str) -> PathBuf {
    out::pool(ws).join(RUN_DIRS).join(id)
}

/// A data root name in a run dir: one path component, not hidden (so never the record or a temp name).
fn check_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.starts_with('.') && !name.contains('/') && name != "..",
        "data root name {name:?}: one path component, not starting with `.`"
    );
    Ok(())
}

/// A run dir's `.run` record.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub tier: String,
    /// The workspace that made a ram run dir (the store is shared by every workspace on the host); None on the pool tier.
    pub workspace: Option<PathBuf>,
}

impl Record {
    fn pool() -> Self {
        Self {
            tier: TIER_POOL.to_owned(),
            workspace: None,
        }
    }

    fn ram(ws: &Path) -> Self {
        Self {
            tier: TIER_RAM.to_owned(),
            workspace: Some(ws.to_owned()),
        }
    }

    pub fn is_ram(&self) -> bool {
        self.tier == TIER_RAM
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut tier = None;
        let mut workspace = None;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(t) = line.strip_prefix("tier=") {
                ensure!(
                    t == TIER_POOL || t == TIER_RAM,
                    "unknown run dir tier {t:?}"
                );
                tier = Some(t.to_owned());
            } else if let Some(w) = line.strip_prefix(WORKSPACE_KEY) {
                workspace = Some(PathBuf::from(w));
            } else {
                bail!(
                    "unknown run dir record line {line:?} (a record from an older xtask: the run dir is regenerable, remove it)"
                );
            }
        }
        let tier = tier.context("run dir record without tier=")?;
        ensure!(
            (tier == TIER_RAM) == workspace.is_some(),
            "run dir record: `{WORKSPACE_KEY}` belongs to tier {TIER_RAM} only"
        );
        Ok(Self { tier, workspace })
    }

    pub fn text(&self) -> String {
        let mut s = format!("tier={}\n", self.tier);
        if let Some(w) = &self.workspace {
            s.push_str(&format!("{WORKSPACE_KEY}{}\n", w.display()));
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
        // a dir without a record this xtask reads: made by hand, or by an older xtask whose record lines are gone
        bail!(
            "{} exists without a {RECORD} record this xtask reads: not a run dir it made; it is regenerable, remove it",
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

/// `(name, path)` of dirs.
pub type Dirs = Vec<(String, PathBuf)>;

/// The run dirs `out prune` removes: those whose id is not in `live`, and ram ones beside a pool run dir of the same id ([`locate`] reads the pool's).
pub fn orphans(ws: &Path, store: &Path, live: &BTreeSet<String>) -> Result<Dirs> {
    let pool = run_dirs(ws)?;
    let pool_ids: BTreeSet<&str> = pool.iter().map(|(id, _)| id.as_str()).collect();
    let ram = ram_run_dirs(store, ws)?;
    let stale_ram: Vec<_> = ram
        .iter()
        .filter(|(id, _)| pool_ids.contains(id.as_str()) || !live.contains(id))
        .cloned()
        .collect();
    Ok(pool
        .iter()
        .filter(|(id, _)| !live.contains(id))
        .cloned()
        .chain(stale_ram)
        .collect())
}

/// Removes `dir` (a run dir or a data root; module doc, Locks). Data roots carry app-made modes (a 0500 dir would stop `remove_dir_all`), so dirs get `u+rwx` first.
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

/// A fresh data root `<run>/<name>` for one app run of the checkout at `root`: a previous root of that name is removed and an empty dir made (the run installs its APK into it). The run dir is made when missing.
/// On a ram run dir the store must admit one data root of class `run`; when it cannot, the ram run dir is dropped (every root in it is regenerable) and remade on the pool. Returns the root and the build dir's lock, held shared: keep it for the run.
pub fn fresh_root(root: &Path, name: &str) -> Result<(PathBuf, build_pool::DirLock)> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let cfg = crate::ws_config::load(&ws)?;
    fresh_at(&ws, &cfg, &out::checkout_id(root)?, name)
}

/// [`fresh_root`] for checkout `id` of workspace `ws`.
fn fresh_at(
    ws: &Path,
    cfg: &crate::ws_config::WsConfig,
    id: &str,
    name: &str,
) -> Result<(PathBuf, build_pool::DirLock)> {
    check_name(name)?;
    let held = hold(ws, id)?;
    let mut dir = make(ws, &cfg.delta, id, None)?;
    let rec = record(&dir)?.context("run dir lost its record")?;
    if rec.is_ram() {
        let probed = delta::probe(&cfg.delta.ram_store.value, &cfg.delta)?;
        if let Err(v) = delta::admit_once(ws, id, &cfg.delta, delta::Class::Run, probed)? {
            if matches!(v, delta::Verdict::Refuse(_)) {
                out::count(ws, "run-full");
            }
            // under the shared lock: the caller (run-apk) holds it already, so the exclusive one leave/prune take is never free here. Only this checkout's commands use its run dir, and every root in it is regenerable.
            remove(&dir)?;
            eprintln!(
                "out: run/{id}: ram run dir {} dropped ({}): remade on the pool",
                dir.display(),
                v.reason()
            );
            dir = make(ws, &cfg.delta, id, Some("moved off a full store"))?;
        }
    }
    let data = dir.join(name);
    if out::present(&data)? {
        remove(&data)?;
    }
    std::fs::create_dir(&data).with_context(|| format!("creating {}", data.display()))?;
    Ok((data, held))
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
            "out tier: would drop run/{id} ({}) and make it on tier {want} (dry run; add --execute)",
            rec.tier
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
                    "out: run ram (app run in run/{id}, not admitted: {}; runs on its data root, a full store fails it cleanly; the next fresh data root moves run/{id} to the pool)",
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
        // full by the store's own accounting: land's hold is not fullness, so none here
        let v = delta::judge(
            &stats,
            delta::Need::of(delta::Class::Run, &self.cfg, &stats),
            0,
            delta::Hold::default(),
            self.cfg.ram_margin_gib.value << 30,
        );
        if matches!(v, delta::Verdict::Refuse(_)) {
            out::count(&self.ws, "run-full");
            println!(
                "out: run/{}: store full after the run ({}): the next fresh data root moves it to the pool",
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

    /// Paths follow the layout the module doc states: `out/pool/run/<id>`, `<store>/.run/<id>`.
    #[test]
    fn paths() {
        let ws = Path::new("/w");
        assert_eq!(run_dir_of(ws, "t1"), Path::new("/w/out/pool/run/t1"));
        assert_eq!(
            ram_run_dir_of(Path::new("/s"), "t1"),
            Path::new("/s/.run/t1")
        );
    }

    #[test]
    fn record_round_trip() {
        let rec = Record::pool();
        assert_eq!(rec.text(), "tier=pool\n");
        assert_eq!(Record::parse(&rec.text()).unwrap(), rec);
        let ram = Record::ram(Path::new("/w"));
        assert_eq!(ram.text(), "tier=ram\nworkspace=/w\n");
        assert_eq!(Record::parse(&ram.text()).unwrap(), ram);
        // a ram dir without its owner, or a pool dir with one, is not a record xtask writes
        assert!(Record::parse("tier=ram\n").is_err());
        assert!(Record::parse("tier=pool\nworkspace=/w\n").is_err());
        assert!(Record::parse("tier=disk\n").is_err());
        // a line this xtask does not write (an older record's) is refused, not skipped
        assert!(Record::parse("tier=pool\nuse=a b\n").is_err());
    }

    #[test]
    fn data_root_names() {
        for good in ["fossify-math", "wechat", "apk-app-debug"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".run", ".x", "a/b", ".."] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
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
        // full by its own accounting: zram limit 10 GiB with 9.5 used; any class peak at ratio 1.95 plus the 1 GiB margin > 0.5 free
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
        assert!(orphans(&ws, &store, &live).unwrap().is_empty());
        assert!(!run_dir_of(&ws, "r").exists());
        assert_eq!(make(&ws, &cfg, "r", None).unwrap(), dir);
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Prune: a run dir of a gone checkout (either tier), and a ram one beside a pool dir of the same id (made while the store was away), are orphans; a live one is kept; another workspace's is never listed.
    #[test]
    fn orphan_run_dirs() {
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
        let pool_gone = run_dir_of(&ws, "pgone");
        std::fs::create_dir_all(&pool_gone).unwrap();
        write_record(&pool_gone, &Record::pool()).unwrap();
        let theirs = ram_run_dir_of(&store, "x");
        std::fs::create_dir_all(&theirs).unwrap();
        write_record(&theirs, &Record::ram(Path::new("/other"))).unwrap();
        let live = BTreeSet::from(["live".to_owned(), "both".to_owned()]);
        let got = orphans(&ws, &store, &live).unwrap();
        assert_eq!(
            got,
            [
                ("pgone".to_owned(), pool_gone.clone()),
                ("both".to_owned(), ram_run_dir_of(&store, "both")),
                ("gone".to_owned(), ram_run_dir_of(&store, "gone"))
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

    /// Maintainer rule (2026-11-22): every app run starts on an empty root. Two consecutive fresh roots of one name: the second is empty, so it sees nothing the first run created (a file, an app-made read-only dir); another name's root is left alone.
    #[test]
    fn consecutive_fresh_roots_share_nothing() {
        let ws = scratch("fresh");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        // no RAM store: the pool tier
        let cfg = crate::ws_config::parse(
            &format!(
                "[delta]\nram_store = \"{}\"\n",
                ws.join("no-store").display()
            ),
            &ws.join("workspace.toml"),
        )
        .unwrap();
        let (first, lock) = fresh_at(&ws, &cfg, "t", "app").unwrap();
        assert_eq!(first, run_dir_of(&ws, "t").join("app"));
        assert_eq!(std::fs::read_dir(&first).unwrap().count(), 0);
        // what the first run leaves: an installed file and an app-made 0500 dir
        std::fs::create_dir_all(first.join("apps/a.b/apk")).unwrap();
        std::fs::write(first.join("apps/a.b/apk/base.apk"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            first.join("apps/a.b"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let (other, _l2) = fresh_at(&ws, &cfg, "t", "other").unwrap();
        std::fs::write(other.join("keep"), "y").unwrap();
        drop(lock);
        let (second, _l3) = fresh_at(&ws, &cfg, "t", "app").unwrap();
        assert_eq!(second, first);
        assert_eq!(std::fs::read_dir(&second).unwrap().count(), 0);
        assert!(other.join("keep").is_file());
        assert!(fresh_at(&ws, &cfg, "t", ".run").is_err());
        let _ = remove(&ws);
    }

    /// A ram run dir whose store cannot take another data root (here: a class larger than any store, NeverFits) moves to the pool at the next fresh root, while the caller already holds the build dir lock shared (as run-apk does): the ram dir is gone, the root is on the pool, `run-dir-pool` counted.
    #[test]
    fn full_store_moves_the_run_dir_to_the_pool() {
        let Some(store) = shm_store("move") else {
            return;
        };
        let ws = scratch("move");
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg_toml = |extra: &str| {
            crate::ws_config::parse(
                &format!("[delta]\nram_store = \"{}\"\n{extra}", store.display()),
                &ws.join("workspace.toml"),
            )
            .unwrap()
        };
        let small = cfg_toml("peak_run_gib = 0.01\nram_margin_gib = 0\n");
        let (first, _) = fresh_at(&ws, &small, "m", "app").unwrap();
        assert_eq!(first, ram_run_dir_of(&store, "m").join("app"));
        let _caller = hold(&ws, "m").unwrap();
        let huge = cfg_toml("peak_run_gib = 1048576\n");
        let (moved, _l) = fresh_at(&ws, &huge, "m", "app").unwrap();
        assert_eq!(moved, run_dir_of(&ws, "m").join("app"));
        assert!(!ram_run_dir_of(&store, "m").exists());
        assert_eq!(counter(&ws, "run-dir-pool"), 1);
        let _ = remove(&ws);
        let _ = std::fs::remove_dir_all(&store);
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
