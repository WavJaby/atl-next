// SPDX-License-Identifier: MIT
//! Build-dir delta store: where a checkout's writes on top of its base go. One interface, two capability tiers, chosen by probing (never by host name):
//! `ram` = overlayfs, lower `base/<commit>` on the pool, upper + work in `<delta.ram_store>/<id>/` when that store is RAM-backed (zram block device or tmpfs) and the build class fits it; `pool` = the reflinked/copied dir of out.rs, the baseline that works everywhere.
//! Every checkout may use `ram`, main and land included: land publishes a ram dir without mounting it (lower reflinked + upper applied, delta_publish.rs) and lays its dir again after each publish or reset (out::relay_land). A dir made before keeps its tier. The decision is recorded once: a ram dir's mount point `out/pool/ws/<id>` holds only [`RECORD`]; a pool dir has none.
//! Every outer build command prints `out: delta ram|pool (<reason>)` and counts `pool/delta-<outcome>.count`.
//!
//! Mount (unprivileged; plan ram-delta D4): the outer command re-executes as `unshare --user --map-root-user --mount` (stage `mount`: uid 0 in that userns mounts the overlay at the real dir path), which runs `unshare --user --map-user=<uid>` (stage `in`: the caller's uid, no capabilities, as bwrap demands) and continues into the build namespace (tree.rs), whose bind of the dir over `out/build` then shows the overlay.
//! One mount per upper: two overlays on one upper are undefined (the kernel only warns with index=off), so the outer command holds an exclusive flock whose fd every process of the namespace inherits; a second command of the checkout waits (the line names the holders).
//! A process still holding it when the command returns (a daemon a build spawned: sccache server, gradle, kotlin) may still use the overlay, so the outer command ends it ([`end_holders`]): the mount and everything using it end with the command.
//! Only this process tree sees the dir: outside, `ws/<id>` is the empty mount point. Cargo's bootstrap of xtask (before xtask runs) stays on `pool/boot/<id>`, the checkout's `target` link; cargo runs inside get the dir's target as `CARGO_TARGET_DIR` (out::pending_target).
//! Admission: the store's own accounting (zram mm_stat, statvfs) is the persistent term; a build reserves the growth it adds to its own upper for its run ([`Need::of`]: its class's peak less what its upper already holds, floored at `delta.regrow_gib`), and others' reservations count against the store ([`judge`]). Waiters queue FIFO ([`admit_keeping`]): a new dir waits out others' reservations and otherwise takes the pool; an existing ram dir also waits out a full store and land's hold, and is refused only when it never fits or inside land's queue ([`fall_to_pool`]): admission never drops a dir.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool;
use crate::flock::{Flock, Mode};
use crate::out;
use crate::ws_config::DeltaConfig;

/// In a ram dir's mount point (outside every namespace): `base=<commit>` and `store=<path>`.
pub const RECORD: &str = ".delta";
/// Set on the re-executed xtask: `mount:<uid>:<gid>` (stage mount) or `in` (inside, mounted). Like tree::ENV, internal plumbing between xtask's own processes.
pub const ENV: &str = "ATL_XTASK_DELTA";
/// In `<store>/<id>/`: the dir was just made; the first mount forgets the base's units reading a path this checkout's builds do not see (out::forget_runs_reading), then removes it.
const FRESH: &str = "fresh";
/// In `<store>/<id>/`: the workspace path that made it. Ids are unique per workspace only.
const OWNER: &str = "workspace";
/// In an evicted ram dir's mount point: why its upper is gone ([`evict_one`]); removed with the dir when it is made again.
const EVICTED: &str = "evicted";
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// statfs f_type of tmpfs (linux/magic.h TMPFS_MAGIC).
const TMPFS_MAGIC: i64 = 0x0102_1994;
/// zram's ratio is measured once the device holds this much original data; below it the configured default stands (a few MiB of metadata compress far better than a build).
const RATIO_MIN_ORIG: u64 = GIB;

/// Where this process runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// uid 0 in the mount userns, about to mount: carries the caller's uid and gid.
    Mount(u32, u32),
    /// inside the mounted namespace.
    In,
}

pub fn stage() -> Option<Stage> {
    let v = std::env::var(ENV).ok()?;
    if v == "in" {
        return Some(Stage::In);
    }
    let mut it = v.strip_prefix("mount:")?.split(':');
    Some(Stage::Mount(
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// Build classes: the upper growth a command reserves (`delta.peak_<key>_gib`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Small = 0,
    Check = 1,
    Native = 2,
    Cross = 3,
    /// Not a build: one app data root in a run dir on the store (out_run.rs), same accounting.
    Run = 4,
}

impl Class {
    pub const ALL: [Class; 5] = [
        Class::Small,
        Class::Check,
        Class::Native,
        Class::Cross,
        Class::Run,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Class::Small => "small",
            Class::Check => "check",
            Class::Native => "native",
            Class::Cross => "cross",
            Class::Run => "run",
        }
    }

    /// The class of workspace.toml key `peak_<name>_gib`.
    pub fn from_key(key: &str) -> Option<Class> {
        let name = key.strip_prefix("peak_")?.strip_suffix("_gib")?;
        Class::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Reserved upper growth, bytes.
    pub fn peak(self, cfg: &DeltaConfig) -> u64 {
        cfg.peak_mib[self as usize].value * MIB
    }
}

/// A ram dir's record ([`RECORD`]).
/// `parts`: arch -> commit of the arch part laid under the dir (`part=<arch>@<commit>` lines). Parsed so `out prune` keeps every part a record names; laying is out.rs `part`'s job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub base: String,
    pub store: PathBuf,
    pub parts: BTreeMap<String, String>,
}

impl Record {
    fn text(&self) -> String {
        let mut text = format!("base={}\nstore={}\n", self.base, self.store.display());
        for (arch, commit) in &self.parts {
            text.push_str(&format!("part={arch}@{commit}\n"));
        }
        text
    }

    fn parse(text: &str) -> Result<Record> {
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .with_context(|| format!("{RECORD} has no `{k}=` line: {text:?}"))
        };
        let mut parts = BTreeMap::new();
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("part=") else {
                continue;
            };
            let (arch, commit) = rest
                .split_once('@')
                .with_context(|| format!("{RECORD}: `{line}` is not `part=<arch>@<commit>`"))?;
            parts.insert(arch.to_owned(), commit.to_owned());
        }
        Ok(Record {
            base: field("base")?.to_owned(),
            store: PathBuf::from(field("store")?),
            parts,
        })
    }

    /// `<store>/<id>`: upper, work and the fresh marker.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.store.join(id)
    }
}

/// The record of build dir `dir`, read outside the namespace (inside, the overlay covers it); None for a pool dir or no dir.
pub fn record(dir: &Path) -> Result<Option<Record>> {
    match std::fs::read_to_string(dir.join(RECORD)) {
        Ok(t) => Record::parse(&t)
            .map(Some)
            .with_context(|| format!("{}", dir.join(RECORD).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.join(RECORD).display())),
    }
}

/// What backs a RAM store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// a zram block device: compressed, not swappable.
    Zram,
    /// tmpfs: uncompressed, its pages may swap (a disk swap file then receives them).
    Tmpfs,
}

/// A RAM store's accounting, bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub backing: Backing,
    pub fs_size: u64,
    pub fs_free: u64,
    /// RAM the store may use in all: zram `mem_limit` (0 = unlimited -> used + MemAvailable), tmpfs used + MemAvailable.
    pub ram_limit: u64,
    pub ram_used: u64,
    /// raw bytes per RAM byte, thousandths.
    pub ratio_milli: u64,
    /// The ratio was measured on the device (else the configured default).
    pub ratio_measured: bool,
}

impl Stats {
    pub fn line(&self) -> String {
        format!(
            "{:?} fs {:.1}/{:.1} GiB free, RAM {:.2}/{:.2} GiB used, ratio {:.2}{}",
            self.backing,
            gib(self.fs_free),
            gib(self.fs_size),
            gib(self.ram_used),
            gib(self.ram_limit),
            self.ratio_milli as f64 / 1000.0,
            if self.ratio_measured {
                ""
            } else {
                " (default)"
            }
        )
    }
}

fn gib(b: u64) -> f64 {
    b as f64 / GIB as f64
}

/// The reason of a tier decision with `delta.ram_store` unset: no RAM tier.
pub const NO_STORE: &str = "no delta.ram_store in workspace.toml: RAM tier off";

/// [`probe`] of the configured store; unset = Err([`NO_STORE`]).
pub fn probe_store(
    store: Option<&Path>,
    cfg: &DeltaConfig,
) -> Result<std::result::Result<Stats, String>> {
    match store {
        Some(path) => probe(path, cfg),
        None => Ok(Err(NO_STORE.to_owned())),
    }
}

/// The store at `path`: Ok(stats) when it is RAM-backed, Err(reason) when not (the pool tier's stated reason).
pub fn probe(path: &Path, cfg: &DeltaConfig) -> Result<std::result::Result<Stats, String>> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(Err(format!("no delta.ram_store at {}", path.display()))),
    };
    let fs = rustix::fs::statfs(path).with_context(|| format!("statfs {}", path.display()))?;
    let vfs = rustix::fs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    let fs_size = vfs.f_blocks * vfs.f_frsize;
    let fs_free = vfs.f_bavail * vfs.f_frsize;
    if fs.f_type as i64 == TMPFS_MAGIC {
        let used = fs_size.saturating_sub(vfs.f_bfree * vfs.f_frsize);
        return Ok(Ok(Stats {
            backing: Backing::Tmpfs,
            fs_size,
            fs_free,
            ram_limit: used + mem_available()?,
            ram_used: used,
            ratio_milli: 1000,
            ratio_measured: true,
        }));
    }
    let dev = meta.dev();
    let sys = PathBuf::from(format!(
        "/sys/dev/block/{}:{}",
        rustix::fs::major(dev),
        rustix::fs::minor(dev)
    ));
    let name = std::fs::read_link(&sys)
        .ok()
        .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if !name.starts_with("zram") {
        let shown = if name.is_empty() {
            "no block device".to_owned()
        } else {
            name
        };
        return Ok(Err(format!(
            "{} is not RAM-backed ({shown})",
            path.display()
        )));
    }
    let mm = std::fs::read_to_string(sys.join("mm_stat"))
        .with_context(|| format!("reading {}", sys.join("mm_stat").display()))?;
    let f: Vec<u64> = mm
        .split_whitespace()
        .map(|x| x.parse().unwrap_or(0))
        .collect();
    ensure!(f.len() >= 4, "{}: short mm_stat {mm:?}", sys.display());
    // mm_stat: orig_data_size compr_data_size mem_used_total mem_limit ...
    let (orig, used, limit) = (f[0], f[2], f[3]);
    let measured = orig >= RATIO_MIN_ORIG && used > 0;
    Ok(Ok(Stats {
        backing: Backing::Zram,
        fs_size,
        fs_free,
        ram_limit: if limit > 0 {
            limit
        } else {
            used + mem_available()?
        },
        ram_used: used,
        ratio_milli: if measured {
            orig * 1000 / used
        } else {
            cfg.ratio_milli.value
        },
        ratio_measured: measured,
    }))
}

fn mem_available() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
    let kib: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .context("/proc/meminfo has no MemAvailable")?;
    Ok(kib * 1024)
}

/// The admission decision of one build.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Reserve `need.ram` bytes and start.
    Admit { need: Need },
    /// Room exists once other builds' reservations end.
    Wait(String),
    /// Not now: the store is full by its own accounting.
    Refuse(String),
    /// The class can never fit this store: pool tier.
    NeverFits(String),
    /// Fits only without land's hold ([`Hold`], counted while land is queued or running). A new dir takes the pool; an existing ram dir queues ([`admit_keeping`]), or is refused inside land's queue.
    Held(String),
}

impl Verdict {
    /// The stated numbers of a non-admitting verdict.
    pub fn reason(&self) -> String {
        match self {
            Verdict::Admit { need } => format!("admitted, {:.2} GiB RAM", gib(need.ram)),
            Verdict::Wait(w) => format!("{w} (others' reservations)"),
            Verdict::Refuse(w) | Verdict::NeverFits(w) | Verdict::Held(w) => w.clone(),
        }
    }
}

/// Upper growth of one admission: raw bytes on the store fs and the RAM they take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Need {
    pub raw: u64,
    pub ram: u64,
}

impl Need {
    /// Upper growth a build of `class` adds when its upper already holds `own` bytes of its part: raw = min(peak, max(regrow, peak - own)).
    /// The store's `used` already counts `own`, so the full peak counted it twice (ram-store-admission S1). Regrow floors what a build over an existing upper still adds (measured over 14 windows).
    /// ram = ceil(raw / ratio); class run's ratio is capped at `delta.ratio_run` (app data compresses worse than the build uppers the device ratio mostly measures).
    pub fn of(class: Class, cfg: &DeltaConfig, s: &Stats, own: u64) -> Need {
        let peak = class.peak(cfg);
        let regrow = cfg.regrow_gib.value * GIB;
        let raw = peak.min(regrow.max(peak.saturating_sub(own)));
        let ratio = match class {
            Class::Run => s.ratio_milli.min(cfg.ratio_run_milli.value),
            _ => s.ratio_milli,
        };
        Need {
            raw,
            ram: (raw * 1000).div_ceil(ratio.max(1)),
        }
    }
}

/// Land's state as an admission sees it. `active`: land's queue holds `out/land.lock` (land-seq.sh takes it for the whole queue), so a land runs or waits behind one. `reserved`: land's own build reservation is live; it is in the others' bytes already.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LandState {
    pub active: bool,
    pub reserved: bool,
}

/// Room for land's next native build that every other admission leaves free: the land hold. Counted only while land is active and holds no reservation (a reservation counts once, in the others' bytes); zero for land itself, and when native never fits the store.
pub type Hold = Need;

/// The hold an admission by `id` counts on `s`, given land's state.
pub fn hold_for(id: &str, cfg: &DeltaConfig, s: &Stats, land: LandState) -> Hold {
    if id == out::LAND_ID || !land.active || land.reserved {
        return Hold::default();
    }
    // own 0: land's upper is not credited, so the hold stays the full native peak (conservative)
    let native = Need::of(Class::Native, cfg, s, 0);
    match judge(
        s,
        native,
        0,
        Hold::default(),
        cfg.ram_margin_gib.value * GIB,
    ) {
        Verdict::NeverFits(_) => Hold::default(),
        _ => native,
    }
}

/// Admission of a build needing `need` while others hold `reserved` RAM bytes and land's `hold` stays free; `margin` RAM bytes stay free.
/// Refuse / NeverFits: the store is short by its own accounting. Wait: others' reservations (RAM and raw) stand in the way.
pub fn judge(s: &Stats, need: Need, reserved: u64, hold: Hold, margin: u64) -> Verdict {
    let (need_raw, need_ram) = (need.raw, need.ram);
    let n = |b: u64| format!("{:.2} GiB", gib(b));
    if need_raw > s.fs_size || need_ram + margin > s.ram_limit {
        return Verdict::NeverFits(format!(
            "needs {} raw / {} RAM + {} margin, store holds {} raw / {} RAM",
            n(need_raw),
            n(need_ram),
            n(margin),
            n(s.fs_size),
            n(s.ram_limit)
        ));
    }
    if need_raw > s.fs_free {
        return Verdict::Refuse(format!(
            "store fs free {} < need {}",
            n(s.fs_free),
            n(need_raw)
        ));
    }
    let ram_free = s.ram_limit.saturating_sub(s.ram_used);
    if need_ram + margin > ram_free {
        return Verdict::Refuse(format!(
            "store RAM free {} < need {} + margin {}",
            n(ram_free),
            n(need_ram),
            n(margin)
        ));
    }
    if need_raw + hold.raw > s.fs_free || need_ram + hold.ram + margin > ram_free {
        return Verdict::Held(format!(
            "land hold {} RAM / {} raw: store RAM free {}, fs free {} < need {} RAM / {} raw + hold + margin {}",
            n(hold.ram),
            n(hold.raw),
            n(ram_free),
            n(s.fs_free),
            n(need_ram),
            n(need_raw),
            n(margin)
        ));
    }
    // others' reservations are RAM bytes: raw at the device ratio (the reservation file keeps one number); a shortfall they cause waits, as the RAM one does
    let others_raw = reserved.saturating_mul(s.ratio_milli) / 1000;
    if need_raw + others_raw + hold.raw > s.fs_free {
        return Verdict::Wait(format!(
            "store fs free {} < need {} + {} raw reserved by running builds + land hold {} raw",
            n(s.fs_free),
            n(need_raw),
            n(others_raw),
            n(hold.raw)
        ));
    }
    if need_ram + hold.ram + reserved + margin > ram_free {
        return Verdict::Wait(format!(
            "store RAM free {} < need {} + {} reserved by running builds + land hold {} + margin {}",
            n(ram_free),
            n(need_ram),
            n(reserved),
            n(hold.ram),
            n(margin)
        ));
    }
    Verdict::Admit { need }
}

/// The class and cross flag of `native --host <host>` in the checkout at `root` (cli.rs). The build machine's host: Native, not cross.
/// Another host is cross (its own dir `upper/<host>`): Native when its part is laid or would be laid (capability, not host identity), else Cross (a build from zero).
pub fn class_for_native(root: &Path, host: &str) -> Result<(Class, bool)> {
    let arches = crate::arches::load(root)?;
    let (build, _) = crate::arches::build_machine(root, &arches)?;
    if host == build {
        return Ok((Class::Native, false));
    }
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = out::checkout_id(root)?;
    let laid = match record(&out::dir_of(&ws, &id))? {
        Some(rec) if rec.parts.contains_key(host) => true,
        Some(rec) => {
            !upper_host_exists(&rec.dir(&id), host) && out::pick_part(root, &ws, host)?.is_some()
        }
        None => out::pick_part(root, &ws, host)?.is_some(),
    };
    Ok((if laid { Class::Native } else { Class::Cross }, true))
}

fn lock_dir(ws: &Path) -> PathBuf {
    build_pool::dir(ws).join("delta")
}

fn open_lock(path: &Path) -> Result<std::fs::File> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// The lock prune takes while it judges bases, and make takes while it records one: a base named by a record is never deleted.
pub fn bases_lock(ws: &Path) -> Result<Flock> {
    let p = lock_dir(ws).join("bases.lock");
    Flock::take(open_lock(&p)?, Mode::Exclusive).with_context(|| format!("flock {}", p.display()))
}

/// The mount lock of checkout `id`: held while its overlay may be mounted.
fn mount_lock_path(ws: &Path, id: &str) -> PathBuf {
    lock_dir(ws).join(format!("mount-{id}.lock"))
}

/// Exclusive mount lock without waiting: None while a namespace of the checkout lives.
pub fn try_mount_lock(ws: &Path, id: &str) -> Result<Option<Flock>> {
    let p = mount_lock_path(ws, id);
    Flock::try_take(open_lock(&p)?, Mode::Exclusive)
        .with_context(|| format!("flock {}", p.display()))
}

/// A running build's RAM reservation: a file holding the byte count under an exclusive flock, freed by the kernel when this process ends.
pub struct Reservation {
    path: PathBuf,
    _lock: Flock,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bytes reserved by live reservations, and whether one of them is land's (`land-<pid>`); files whose lock is free (their process gone) are removed.
fn reserved(ws: &Path) -> Result<(u64, bool)> {
    let dir = lock_dir(ws).join("reserve");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok((0, false));
    };
    let mut sum = 0;
    let mut land = false;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(f) = std::fs::File::open(&p) else {
            continue;
        };
        match Flock::try_take(f, Mode::Shared)? {
            Some(_free) => {
                let _ = std::fs::remove_file(&p);
            }
            None => {
                land |= e
                    .file_name()
                    .to_str()
                    .and_then(|n| n.rsplit_once('-'))
                    .is_some_and(|(id, _)| id == out::LAND_ID);
                sum += std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| t.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
    }
    Ok((sum, land))
}

/// Bytes other builds reserve, and land's state, as an admission sees them now.
fn land_now(ws: &Path) -> Result<(u64, LandState)> {
    let (others, land_reserved) = reserved(ws)?;
    let active = land_active(ws)?;
    Ok((
        others,
        LandState {
            active,
            reserved: land_reserved,
        },
    ))
}

/// `out/land.lock`: land-seq.sh's exclusive lock for its whole queue, inherited by every process the queue starts.
fn land_lock_path(ws: &Path) -> PathBuf {
    ws.join("out").join("land.lock")
}

/// Whether land's queue is active: land-seq.sh holds `out/land.lock` exclusively from its start to its exit (one land at a time), and a queued land waits behind that holder, so a held lock means a land running or queued. Read by probing the lock (released at once), not from a flag file; no file = no land yet.
fn land_active(ws: &Path) -> Result<bool> {
    let p = land_lock_path(ws);
    let file = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("opening {}", p.display())),
    };
    Ok(Flock::try_take(file, Mode::Shared)?.is_none())
}

/// [`judge_quiet`], counting `delta-land-hold` on a [`Verdict::Held`]: one try.
fn judge_now(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    own: u64,
    stats: &Stats,
) -> Result<Verdict> {
    let v = judge_quiet(ws, id, cfg, class, own, stats)?;
    if matches!(v, Verdict::Held(_)) {
        out::count(ws, "delta-land-hold");
    }
    Ok(v)
}

/// The verdict for `class` by `id` on `stats`, its upper holding `own` bytes of its part, against the live reservations and land's hold; the caller holds the admit gate. Counts nothing: a waiter re-judges every poll and counts per state change.
fn judge_quiet(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    own: u64,
    stats: &Stats,
) -> Result<Verdict> {
    let (others, hold) = inputs(ws, id, cfg, stats)?;
    Ok(judge(
        stats,
        Need::of(class, cfg, stats, own),
        others,
        hold,
        cfg.ram_margin_gib.value * GIB,
    ))
}

/// Others' RAM reservations and land's hold on `stats`, as [`judge`] takes them.
fn inputs(ws: &Path, id: &str, cfg: &DeltaConfig, stats: &Stats) -> Result<(u64, Hold)> {
    let (others, land) = land_now(ws)?;
    Ok((others, hold_for(id, cfg, stats, land)))
}

/// Bytes short of admitting `need` on `s` with `reserved` RAM held by others and `hold`: judge's checks summed, (raw, RAM), 0 = room. Others' reservations count in both, as a Wait counts them; a refusal is covered only with them.
fn shortfall(s: &Stats, need: Need, reserved: u64, hold: Hold, margin: u64) -> (u64, u64) {
    let others_raw = reserved.saturating_mul(s.ratio_milli) / 1000;
    let ram_free = s.ram_limit.saturating_sub(s.ram_used);
    (
        (need.raw + others_raw + hold.raw).saturating_sub(s.fs_free),
        (need.ram + hold.ram + reserved + margin).saturating_sub(ram_free),
    )
}

/// Land's hold on `stats` as a non-land admission sees it now (`out show`).
pub fn hold_now(ws: &Path, cfg: &DeltaConfig, stats: &Stats) -> Result<Hold> {
    let (_, land) = land_now(ws)?;
    Ok(hold_for("", cfg, stats, land))
}

fn reserve(ws: &Path, id: &str, need_ram: u64) -> Result<Reservation> {
    let path = lock_dir(ws)
        .join("reserve")
        .join(format!("{id}-{}", std::process::id()));
    let mut lock = Flock::take(open_lock(&path)?, Mode::Exclusive)?;
    use std::io::Write;
    lock.file().write_all(format!("{need_ram}\n").as_bytes())?;
    Ok(Reservation { path, _lock: lock })
}

/// An admitted reservation: the guard, the store's stats it was judged on, the growth reserved.
pub type Admitted = (Reservation, Stats, Need);

/// One admission try of `class` on `probed` (a [`probe`] result), its upper holding `own` bytes of its part (0: run dirs, a new dir), never waiting: run dirs take the pool tier when short (out_run.rs), a wait would only stall an app start. Ok(Err(verdict)) = not admitted, Wait included.
pub fn admit_once(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    own: u64,
    probed: std::result::Result<Stats, String>,
) -> Result<std::result::Result<Admitted, Verdict>> {
    let stats = match probed {
        Ok(s) => s,
        Err(why) => return Ok(Err(Verdict::NeverFits(why))),
    };
    let gate = lock_dir(ws).join("admit.lock");
    let _gate = Flock::take(open_lock(&gate)?, Mode::Exclusive)?;
    match judge_now(ws, id, cfg, class, own, &stats)? {
        Verdict::Admit { need } => Ok(Ok((reserve(ws, id, need.ram)?, stats, need))),
        other => Ok(Err(other)),
    }
}

/// Whether a build of `class` by `id` would be admitted on `store` now: one try, its reservation released at once (out::relay_land picks land's tier with it). Ok(the admitted line) or Err(why not).
pub fn fits_now(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    store: Option<&Path>,
) -> Result<std::result::Result<String, String>> {
    Ok(
        // a dir laid now starts with an empty upper: nothing of its own counts
        match admit_once(ws, id, cfg, class, 0, probe_store(store, cfg)?)? {
            Ok((_released, stats, need)) => Ok(format!(
                "{} build fits, {:.2} GiB RAM; store {}: {}",
                class.name(),
                gib(need.ram),
                store.map_or_else(|| "unset".to_owned(), |p| p.display().to_string()),
                stats.line()
            )),
            Err(v) => Err(format!("{} build: {}", class.name(), v.reason())),
        },
    )
}

/// [`admit_keeping`] for a new dir (`keep` false): a Wait queues, Refuse / Held / NeverFits return (the caller takes the pool).
#[cfg(test)]
fn admit(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    own: u64,
    probe_now: impl FnMut() -> Result<std::result::Result<Stats, String>>,
) -> Result<std::result::Result<Admitted, Verdict>> {
    admit_keeping(ws, id, cfg, class, || Ok(own), false, None, probe_now)
}

/// The store queue's directory: one [`crate::ticket`] per waiting admission.
fn queue_dir(ws: &Path) -> PathBuf {
    lock_dir(ws).join("queue")
}

/// What a waiter is blocked by, written into its ticket after the class name: a later small build may pass a head blocked by capacity only ([`may_bypass`]).
fn state_of(v: &Verdict) -> &'static str {
    match v {
        Verdict::Refuse(_) => "refuse",
        Verdict::Held(_) => "held",
        Verdict::Wait(_) => "wait",
        // a waiter not yet judged at the head: as restrictive as a Wait
        _ => "queued",
    }
}

/// Whether a build of `class` behind the queue head may be judged now: class small, and the head is blocked by the store's capacity (Refuse), never by reservations (Wait), land's hold or an unknown state. Mirrors the pool's light bypass (build_pool.rs `bypass_allowed`).
fn may_bypass(class: Class, head_payload: Option<&str>) -> bool {
    class == Class::Small
        && head_payload.and_then(|p| p.split_whitespace().nth(1)) == Some("refuse")
}

/// Admits a build of `class` on store stats from `probe_now`; `own` is its upper's credit, read under the gate at each judgement (an absent upper reads 0, so its need grows). Ok(Ok(reservation)), or Ok(Err(verdict)) for a verdict that does not wait.
/// Waiting: one FIFO [`crate::ticket`] per waiter in [`queue_dir`], judged only at the head ([`may_bypass`] excepted, counter `delta-bypass`). Wait always queues; Refuse and Held queue only for an existing ram dir (`keep`; a new dir takes the pool). NeverFits never waits.
/// A process inside land's queue ([`inside_land_queue`]) and the land checkout never join the FIFO: the land queue waits for them, and a FIFO head waiting on land's hold would wait for them in turn. They poll a Wait (others' reservations end without them); inside the queue Refuse and Held return at once.
/// Nothing is held while waiting (admit gate released between polls, no reservation, no mount lock: `enter` takes the lock after admission), so land's own admission and the land queue's commands in this checkout proceed. No timeout: an endless wait shows in `atl-job status`. A line per state change (`out: delta ws/<id>: waiting ...`, naming the store's holders from `store`), counter `delta-wait` once per wait.
/// Eviction ([`evict_for`]): a FIFO participant's Wait/Refuse/Held short of room first evicts idle checkouts' uppers; a process inside land's queue never does.
#[allow(clippy::too_many_arguments)]
fn admit_keeping(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    mut own: impl FnMut() -> Result<u64>,
    keep: bool,
    store: Option<&Path>,
    mut probe_now: impl FnMut() -> Result<std::result::Result<Stats, String>>,
) -> Result<std::result::Result<Admitted, Verdict>> {
    let inside = inside_land_queue(ws)?;
    // land's own builds never queue behind a waiter that may be waiting on land's hold (the pool's land tier, by checkout)
    let unqueued = inside || id == out::LAND_ID;
    let queue = queue_dir(ws);
    std::fs::create_dir_all(&queue).with_context(|| format!("creating {}", queue.display()))?;
    let mut ticket: Option<crate::ticket::Ticket> = None;
    let mut said: Option<(&'static str, usize)> = None;
    let t0 = std::time::Instant::now();
    loop {
        let stats = match probe_now()? {
            Ok(s) => s,
            Err(why) => return Ok(Err(Verdict::NeverFits(why))),
        };
        let gate = lock_dir(ws).join("admit.lock");
        let _gate = Flock::take(open_lock(&gate)?, Mode::Exclusive)?;
        let tickets = if unqueued {
            Vec::new()
        } else {
            crate::ticket::live(&queue)?
        };
        let (ahead, total) = match &ticket {
            Some(t) => crate::ticket::position(&tickets, &t.name),
            None => (tickets.len(), tickets.len() + 1),
        };
        let bypass = ahead > 0 && may_bypass(class, tickets.first().map(|(_, p)| p.as_str()));
        let (state, why) = if ahead == 0 || bypass {
            // read under the gate at every judgement: a changed or evicted upper shows here, not at the wait's start
            let own_now = own()?;
            let mut judged = stats;
            let mut v = judge_quiet(ws, id, cfg, class, own_now, &judged)?;
            // FIFO participants only: a process inside land's queue carries no ticket and never evicts (the queue waits for its commands)
            if let Some(store) = store
                && !unqueued
                && matches!(v, Verdict::Wait(_) | Verdict::Refuse(_) | Verdict::Held(_))
                && let Some(credited) =
                    evict_for(ws, id, cfg, store, class, own_now, &judged, &tickets)?
            {
                v = judge_quiet(ws, id, cfg, class, own_now, &credited)?;
                judged = credited;
            }
            if matches!(v, Verdict::Held(_)) && said.is_none_or(|(s, _)| s != "held") {
                out::count(ws, "delta-land-hold");
            }
            match v {
                Verdict::Admit { need } => {
                    if bypass {
                        out::count(ws, "delta-bypass");
                    }
                    let r = reserve(ws, id, need.ram)?;
                    drop(ticket);
                    if said.is_some() {
                        println!(
                            "out: delta ws/{id}: admitted after {:.0} s",
                            t0.elapsed().as_secs_f64()
                        );
                    }
                    return Ok(Ok((r, judged, need)));
                }
                Verdict::NeverFits(_) => return Ok(Err(v)),
                Verdict::Refuse(_) | Verdict::Held(_) if inside || !keep => return Ok(Err(v)),
                _ => (state_of(&v), v.reason()),
            }
        } else {
            ("queued", format!("{ahead} ahead in the store queue"))
        };
        if !unqueued {
            let payload = format!("{} {state} {id}", class.name());
            match &mut ticket {
                Some(t) => t.set_payload(&payload)?,
                None => {
                    ticket = Some(crate::ticket::Ticket::new(
                        &queue,
                        "",
                        std::process::id(),
                        &payload,
                    )?)
                }
            }
        }
        drop(_gate);
        if said != Some((state, ahead)) {
            if said.is_none() {
                out::count(ws, "delta-wait");
            }
            said = Some((state, ahead));
            println!(
                "out: delta ws/{id}: waiting ({} build, queue {total}, {ahead} ahead; an app start in this checkout waits too): {why}; held by: {}",
                class.name(),
                store.map_or_else(|| "store unset".to_owned(), |s| held_by(ws, s, id))
            );
        }
        std::thread::sleep(build_pool::POLL);
    }
}

/// Admission of one command of [`enter`]. `Some(class)`: judged on the store as a build (reservation, waits, refusals, the FIFO queue; [`admit_keeping`]).
/// `None` grows no upper: admitted at once, no reservation, never queued or refused. Do not judge a deletion as a build: the RAM margin refuses it on a full store (the land unit prune). Its whiteouts are inode metadata that nothing reserves.
#[allow(clippy::too_many_arguments)]
fn admit_command(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Option<Class>,
    own: impl FnMut() -> Result<u64>,
    keep: bool,
    store: Option<&Path>,
    probe_now: impl FnMut() -> Result<std::result::Result<Stats, String>>,
) -> Result<std::result::Result<Option<Admitted>, Verdict>> {
    match class {
        Some(class) => {
            Ok(admit_keeping(ws, id, cfg, class, own, keep, store, probe_now)?.map(Some))
        }
        None => Ok(Ok(None)),
    }
}

/// Checkout ids with a queued ticket: the payload's third field (`<class> <state> <id>`, [`admit_keeping`]).
fn queued_ids(tickets: &[(String, String)]) -> HashSet<String> {
    tickets
        .iter()
        .filter_map(|(_, p)| p.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect()
}

/// Checkout ids with a live reservation (admitted, not yet mounted): a reservation file is `<id>-<pid>` and lives while its process does.
fn reserving_ids(ws: &Path) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    let Ok(rd) = std::fs::read_dir(lock_dir(ws).join("reserve")) else {
        return Ok(ids);
    };
    for e in rd.flatten() {
        let Ok(f) = std::fs::File::open(e.path()) else {
            continue;
        };
        if Flock::try_take(f, Mode::Shared)?.is_none()
            && let Some((id, _)) = e.file_name().to_string_lossy().rsplit_once('-')
        {
            ids.insert(id.to_owned());
        }
    }
    Ok(ids)
}

/// A ram dir an admission may evict ([`victims`]).
struct Victim {
    id: String,
    /// Its whole upper, bytes: the `used` stamp.
    raw: u64,
    idle: std::time::Duration,
}

/// The ram dirs of `store` an admission by `me` may evict, longest idle first. Eligible: owned by `ws`; its record names `store`; its `used` stamp holds a size (written at each command's end; no stamp = unknown size = not eligible); its mount lock free (no command runs in it); not in `skip` (queued, reserving), not `me`, not land, not a dot name (`.run`, `.evict-` leftovers).
fn victims(ws: &Path, store: &Path, me: &str, skip: &HashSet<String>) -> Result<Vec<Victim>> {
    let rd = std::fs::read_dir(store).with_context(|| format!("reading {}", store.display()))?;
    let mut found = Vec::new();
    for e in rd.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        let sd = e.path();
        if id.starts_with('.') || id == me || id == out::LAND_ID || skip.contains(&id) {
            continue;
        }
        if !sd.join("upper").is_dir() || !owned(&sd, ws) {
            continue;
        }
        // an unreadable record leaves the dir unevictable, not the admission failed
        if !record(&out::dir_of(ws, &id))
            .ok()
            .flatten()
            .is_some_and(|r| r.store.as_path() == store)
        {
            continue;
        }
        let stamp = sd.join(USED);
        let Some(raw) = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
        else {
            continue;
        };
        let Some(idle) = std::fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
        else {
            continue;
        };
        if try_mount_lock(ws, &id)?.is_none() {
            continue;
        }
        found.push(Victim { id, raw, idle });
    }
    found.sort_by(|a, b| b.idle.cmp(&a.idle).then_with(|| a.id.cmp(&b.id)));
    Ok(found)
}

/// Evicts ram dir `id` of `store` under its mount lock `lock` (taken by the caller; released here): renamed out of place first (its upper is gone at once), then the reason written into its mount point (its next command prints it, [`lost`]), then removed. A crash leaves `<store>/.evict-<id>-<pid>` (an orphan, [`orphan_store_dirs`] removes it) or the marker alone (the dir reads as lost, without the reason).
fn evict_one(ws: &Path, store: &Path, id: &str, lock: Flock, why: &str) -> Result<()> {
    let sd = store.join(id);
    let tmp_name = format!(".evict-{id}-{}", std::process::id());
    let tmp = store.join(&tmp_name);
    std::fs::rename(&sd, &tmp)
        .with_context(|| format!("renaming {} -> {}", sd.display(), tmp.display()))?;
    std::fs::write(out::dir_of(ws, id).join(EVICTED), format!("{why}\n"))
        .with_context(|| format!("writing the evicted marker of ws/{id}"))?;
    // the leftover's own lock while it is removed: out prune's orphan sweep skips a dir whose removal runs here (`if let` keeps the guard alive)
    if let Some(_leftover) = try_mount_lock(ws, &tmp_name)? {
        remove_tree(&tmp)?;
    }
    drop(lock);
    Ok(())
}

/// Evicts idle ram dirs for a FIFO participant's judgement short of room (the caller holds the admit gate): longest idle first, and only when their stamped sizes cover the whole shortfall (else nothing: a wait that stays costs no warm upper). Returns `stats` credited with the bytes predicted from the stamps (zram frees late; the next poll re-probes), or None when nothing was evicted. One line and one `delta-evict` per eviction.
#[allow(clippy::too_many_arguments)]
fn evict_for(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    store: &Path,
    class: Class,
    own: u64,
    stats: &Stats,
    tickets: &[(String, String)],
) -> Result<Option<Stats>> {
    let (others, hold) = inputs(ws, id, cfg, stats)?;
    let need = Need::of(class, cfg, stats, own);
    let margin = cfg.ram_margin_gib.value * GIB;
    let (short_raw, short_ram) = shortfall(stats, need, others, hold, margin);
    if short_raw == 0 && short_ram == 0 {
        return Ok(None);
    }
    let mut skip = queued_ids(tickets);
    skip.extend(reserving_ids(ws)?);
    let cands = victims(ws, store, id, &skip)?;
    let ram_of = |raw: u64| raw * 1000 / stats.ratio_milli.max(1);
    let total_raw: u64 = cands.iter().map(|c| c.raw).sum();
    let total_ram: u64 = cands.iter().map(|c| ram_of(c.raw)).sum();
    if total_raw < short_raw || total_ram < short_ram {
        return Ok(None);
    }
    let (mut freed_raw, mut freed_ram) = (0u64, 0u64);
    for c in &cands {
        if freed_raw >= short_raw && freed_ram >= short_ram {
            break;
        }
        // taken now, under the gate: a command that started since the scan keeps its dir
        let Some(lock) = try_mount_lock(ws, &c.id)? else {
            continue;
        };
        let why = format!(
            "for ws/{id} {} build, idle {} min, {:.2} GiB raw freed",
            class.name(),
            c.idle.as_secs() / 60,
            gib(c.raw)
        );
        evict_one(ws, store, &c.id, lock, &why)?;
        out::count(ws, "delta-evict");
        println!("out: delta evict ws/{}: {why}", c.id);
        freed_raw += c.raw;
        freed_ram += ram_of(c.raw);
    }
    Ok((freed_raw > 0).then(|| Stats {
        fs_free: stats.fs_free + freed_raw,
        ram_used: stats.ram_used.saturating_sub(freed_ram),
        ..stats.clone()
    }))
}

/// The store's deltas other than `me`, largest first: `<id> <GiB> GiB running|idle <min> min` (running = its mount lock is held; idle since its `used` stamp, [`USED`]). What the PM acts on while a build waits.
fn held_by(ws: &Path, store: &Path, me: &str) -> String {
    let Ok(rd) = std::fs::read_dir(store) else {
        return format!("{} unreadable", store.display());
    };
    let mut rows: Vec<(u64, String)> = Vec::new();
    let queued = queued_ids(&crate::ticket::live(&queue_dir(ws)).unwrap_or_default());
    for e in rd.flatten() {
        let Some(id) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let upper = e.path().join("upper");
        if id == me || id.starts_with('.') || !upper.is_dir() {
            continue;
        }
        // whole upper, every host dir: what the delta holds on the store
        let size = own_bytes(&upper, Class::Small, None, &[]).unwrap_or(0);
        let running = try_mount_lock(ws, &id).ok().flatten().is_none();
        let state = if queued.contains(&id) {
            "queued".to_owned()
        } else if running {
            "running".to_owned()
        } else {
            let mins = std::fs::metadata(e.path().join(USED))
                .or_else(|_| std::fs::metadata(&upper))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map_or_else(|| "?".to_owned(), |d| (d.as_secs() / 60).to_string());
            format!("idle {mins} min")
        };
        rows.push((size, format!("{id} {:.1} GiB {state}", gib(size))));
    }
    if rows.is_empty() {
        return "no other delta".to_owned();
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    rows.into_iter()
        .map(|(_, r)| r)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Stamp in a store dir (`<store>/<id>/used`), touched at the end of every ram command: a delta's idle time in the wait line ([`held_by`]).
const USED: &str = "used";

/// Whether this process inherited `out/land.lock`: the queue's fd reaches every process it starts, and the lock lives while any of them does. The queue waits for such a process, so it must not wait for the queue. No lock file: no land yet.
fn inside_land_queue(ws: &Path) -> Result<bool> {
    let p = land_lock_path(ws);
    if !p.exists() {
        return Ok(false);
    }
    Ok(holders(&p, 0)?.iter().any(|h| h.pid == std::process::id()))
}

/// Makes ram dir `ws/<id>` over the newest base that is an ancestor of HEAD: record (under [`bases_lock`]), empty upper/work with `.base`, the fresh marker (land's too: on ram its `target` link does not reach the dir, out::foreign_paths), the checkout's mtimes restored from the base when `restore` (out::make's rule). Err when there is no base (a build from zero is the pool tier's).
pub fn make_ram(root: &Path, ws: &Path, id: &str, store: &Path, restore: bool) -> Result<Record> {
    let _pin = bases_lock(ws)?;
    let base = out::pick_base(root, ws)?.context("no base to lay a ram delta over")?;
    lay_ram(root, ws, id, store, &base, restore)
}

/// [`make_ram`] over base/`base`; the caller holds [`bases_lock`] since it chose `base`.
fn lay_ram(
    root: &Path,
    ws: &Path,
    id: &str,
    store: &Path,
    base: &str,
    restore: bool,
) -> Result<Record> {
    let dir = out::dir_of(ws, id);
    ensure!(
        !out::present(&dir)?,
        "{} exists: drop it first (`cargo xtask out tier`)",
        dir.display()
    );
    let base = base.to_owned();
    let rec = Record {
        base: base.clone(),
        store: store.to_owned(),
        parts: BTreeMap::new(),
    };
    let sd = rec.dir(id);
    if out::present(&sd)? {
        ensure!(
            owned(&sd, ws),
            "{} belongs to another workspace (its {OWNER} file): a store is shared by id, so rename this worktree",
            sd.display()
        );
        remove_tree(&sd)?;
    }
    std::fs::create_dir_all(sd.join("upper"))
        .with_context(|| format!("creating {}", sd.display()))?;
    std::fs::create_dir_all(sd.join("work"))?;
    std::fs::write(sd.join(OWNER), format!("{}\n", ws.display()))?;
    std::fs::write(sd.join("upper").join(out::BASE_FILE), format!("{base}\n"))?;
    std::fs::write(sd.join(FRESH), "")?;
    let parent = dir.parent().context("build dir has no parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{id}.tmp-{}", std::process::id()));
    std::fs::create_dir(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    std::fs::write(tmp.join(RECORD), rec.text())?;
    std::fs::rename(&tmp, &dir)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), dir.display()))?;
    let stamps = if restore {
        out::restore_stamps(root, &out::base_of(ws, &base))?
    } else {
        "mtimes kept: outputs carried in".to_owned()
    };
    println!(
        "out: ws/{id} ram delta over base/{base} in {}; {stamps}",
        sd.display()
    );
    Ok(rec)
}

/// Removes ram dir `ws/<id>`: its store dir and mount point. Refused while a namespace of the checkout lives.
pub fn drop_ram(ws: &Path, id: &str, rec: &Record) -> Result<()> {
    let Some(_m) = try_mount_lock(ws, id)? else {
        bail!("ws/{id}: its ram delta is mounted by a running command: not removed");
    };
    remove_ram(ws, id, rec)
}

/// [`drop_ram`] under the mount lock the caller holds.
fn remove_ram(ws: &Path, id: &str, rec: &Record) -> Result<()> {
    let sd = rec.dir(id);
    if out::present(&sd)? {
        remove_tree(&sd)?;
    }
    let dir = out::dir_of(ws, id);
    if out::present(&dir)? {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// remove_dir_all after giving every dir owner rwx: overlay leaves `work/work` mode 000.
fn remove_tree(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![path.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&d) else {
            continue;
        };
        if !m.is_dir() {
            continue;
        }
        if m.permissions().mode() & 0o700 != 0o700 {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod {}", d.display()))?;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            stack.extend(rd.flatten().map(|e| e.path()));
        }
    }
    std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
}

/// Whether store dir `sd` names workspace `ws` as its owner ([`OWNER`]).
fn owned(sd: &Path, ws: &Path) -> bool {
    std::fs::read_to_string(sd.join(OWNER)).is_ok_and(|t| Path::new(t.trim()) == ws)
}

/// Store dirs `<store>/<id>` of workspace `ws` (their [`OWNER`] names it) whose id is not in `keep`. Never another workspace's: one store serves every workspace on the host, and a scratch workspace's prune once deleted a live checkout's upper mid-build (2026-11-21).
pub fn orphan_store_dirs(
    store: &Path,
    ws: &Path,
    keep: &std::collections::BTreeSet<String>,
) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    let mut found: Vec<_> = rd
        .flatten()
        .filter(|e| owned(&e.path(), ws))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(id, _)| !keep.contains(id))
        .collect();
    found.sort();
    found
}

/// Removes an orphan store dir (its checkout and build dir gone).
pub fn remove_orphan_store(ws: &Path, id: &str, path: &Path) -> Result<bool> {
    let Some(_m) = try_mount_lock(ws, id)? else {
        return Ok(false);
    };
    remove_tree(path)?;
    Ok(true)
}

/// A ram dir's refresh after a rebase (out::refresh decided `pick`): the upper is a delta of the old lower, so it is dropped whole and the dir remade over base/`pick`. Returns the line; a mounted delta is kept.
pub fn refresh(
    root: &Path,
    ws: &Path,
    id: &str,
    rec: &Record,
    pick: &str,
    why: &str,
) -> Result<String> {
    if try_mount_lock(ws, id)?.is_none() {
        return Ok(format!(
            "out: keep ws/{id}: not refreshed to base/{pick}: its ram delta is mounted"
        ));
    }
    drop_ram(ws, id, rec)?;
    make_ram(root, ws, id, &rec.store, true)?;
    Ok(format!(
        "out: refresh ws/{id} ram delta from base/{} to base/{pick} ({why}); the upper was dropped (a delta of the old base)",
        rec.base
    ))
}

/// Bytes of upper `upper` that a build of `class` grows, allocated (`st_blocks * 512`, each inode once; a missing upper is 0).
/// A cross host = `upper/<cross_host>` only, whatever the class (a part-laid cross build is Native). Other builds = all of `upper` but the cross host dirs `cross_dirs`: a cross host's outputs are not credited across arches (rhs2-rv-20: 14.4 GiB of x86 upper, its first riscv64 build still grew 10.32).
/// Run dirs grow from an empty root: 0. A cross command without a host dir (`cross`: cargo writes `target/<triple>`): 0, so its growth is reserved whole and not sampled.
pub fn own_bytes(
    upper: &Path,
    class: Class,
    cross_host: Option<&str>,
    cross_dirs: &[String],
) -> Result<u64> {
    let (top, skip): (PathBuf, Vec<PathBuf>) = match (class, cross_host) {
        (Class::Run, _) | (Class::Cross, None) => return Ok(0),
        // the cross host's dir whatever the class: a part laid under it makes the build Native, its growth is still that dir
        (_, Some(host)) => (upper.join(host), Vec::new()),
        _ => (
            upper.to_owned(),
            cross_dirs.iter().map(|d| upper.join(d)).collect(),
        ),
    };
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    let mut stack = vec![top];
    while let Some(p) = stack.pop() {
        let m = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", p.display())),
        };
        if !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            for entry in
                std::fs::read_dir(&p).with_context(|| format!("reading {}", p.display()))?
            {
                let child = entry?.path();
                if !skip.contains(&child) {
                    stack.push(child);
                }
            }
        }
    }
    Ok(total)
}

/// The cross host dirs of a build dir: every `[host]` row but the build machine's (`native::host_out` writes a cross host's outputs to `<out>/<host>`).
fn cross_dirs(root: &Path) -> Result<Vec<String>> {
    let arches = crate::arches::load(root)?;
    let (build, _) = crate::arches::build_machine(root, &arches)?;
    Ok(arches
        .iter()
        .filter(|(name, arch)| arch.host.is_some() && name.as_str() != build)
        .map(|(name, _)| name.clone())
        .collect())
}

/// One ram build's growth sample: its own part of the upper before and after the command, against the raw growth reserved. Returns the line and whether the growth exceeded the reservation (counter `delta-over`: the class peaks or regrow are too small).
fn growth(id: &str, before: u64, after: u64, reserved: u64) -> (String, bool) {
    let line = format!(
        "out: delta ws/{id} upper {:.2} -> {:.2} GiB raw (reserved {:.2})",
        gib(before),
        gib(after),
        gib(reserved)
    );
    (line, after.saturating_sub(before) > reserved)
}

/// A checkout's own upper credit ([`own_bytes`]), read under the admit gate at each judgement. A full walk costs ~1 s per 200k entries, so it re-walks only when the upper changed: every command's end rewrites the `used` stamp ([`USED`]); an absent upper is 0 (evicted, or not made yet). `seen`: the stamp text and credit of the last walk.
fn own_at(
    store_dir: &Path,
    upper: &Path,
    class: Option<Class>,
    cross_host: Option<&str>,
    dirs: &[String],
    seen: &mut Option<(String, u64)>,
) -> Result<u64> {
    if !upper.is_dir() {
        return Ok(0);
    }
    let stamp = std::fs::read_to_string(store_dir.join(USED)).unwrap_or_default();
    if let Some((text, own)) = seen
        && *text == stamp
    {
        return Ok(*own);
    }
    let own = match class {
        Some(c) => own_bytes(upper, c, cross_host, dirs)?,
        None => 0,
    };
    *seen = Some((stamp, own));
    Ok(own)
}

/// The outer side of every build command: decides the tier, admits, and for `ram` runs the rest of the command inside the mounted namespace. Some(code) = ran there; None = continue here (pool tier, or already inside).
/// `class` None: a command that grows no upper (a prune deletes, a status read changes nothing). It enters an existing ram dir with no admission ([`admit_command`]): no reservation, never queued or refused. No record yet: nothing to enter, None.
/// `cross_host`: the host a cross build writes (`upper/<host>`, [`own_bytes`]); None for every other command.
pub fn enter(
    root: &Path,
    class: Option<Class>,
    cross_host: Option<&str>,
) -> Result<Option<ExitCode>> {
    match stage() {
        Some(Stage::Mount(uid, gid)) => return mount_stage(root, uid, gid).map(Some),
        Some(Stage::In) => return Ok(None),
        None => {}
    }
    if std::env::var_os(crate::tree::ENV).is_some() {
        return Ok(None);
    }
    let ws = Layout::of(root)?.workspace().to_owned();
    let cfg = crate::ws_config::load(&ws)?;
    let id = out::checkout_id(root)?;
    let dir = out::dir_of(&ws, &id);
    let rec = record(&dir)?;
    let pool = |reason: &str| -> Result<Option<ExitCode>> {
        println!("out: delta pool ({reason})");
        out::count(&ws, "delta-pool");
        Ok(None)
    };
    if rec.is_none() && out::present(&dir)? {
        return pool(&format!("ws/{id} is a pool dir"));
    }
    // an existing ram dir keeps its store; a new one takes the configured store, and an unset store is the pool
    let Some(store) = rec.as_ref().map_or_else(
        || cfg.delta.ram_store.value.clone(),
        |r| Some(r.store.clone()),
    ) else {
        return pool(NO_STORE);
    };
    // this checkout's own part of its upper (a new dir: none yet)
    let dirs = cross_dirs(root)?;
    let upper = store.join(&id).join("upper");
    if class.is_none() && rec.is_none() {
        return Ok(None);
    }
    let mut seen = None;
    let admitted = admit_command(
        &ws,
        &id,
        &cfg.delta,
        class,
        || {
            own_at(
                &store.join(&id),
                &upper,
                class,
                cross_host,
                &dirs,
                &mut seen,
            )
        },
        rec.is_some(),
        Some(&store),
        || probe(&store, &cfg.delta),
    )?;
    // the own credit the last judgement used (0: no walk, an absent upper)
    let own = seen.as_ref().map_or(0, |(_, b)| *b);
    let (reservation, stats, need) = match admitted {
        Ok(Some((reservation, stats, need))) => (Some(reservation), Some(stats), need),
        Ok(None) => (None, None, Need::default()),
        Err(Verdict::Admit { .. }) => bail!("delta admission returned Admit as a refusal"),
        Err(v) => match class {
            Some(class) => return fall_to_pool(&ws, &id, class, rec.as_ref(), v),
            // admit_command admits a command that grows nothing: no refusal reaches here
            None => bail!("a command that grows nothing was refused: {}", v.reason()),
        },
    };
    // the mount lock, right after admission and before the upper is read or made: nothing is held while queued, and an upper exists only under its lock, so no eviction sees it idle
    let lock_path = mount_lock_path(&ws, &id);
    let lock = match try_mount_lock(&ws, &id)? {
        Some(l) => l,
        None => {
            let held_by = holders(&lock_path, std::process::id())?;
            println!(
                "out: ws/{id} ram delta in use by another command of this checkout ({}), waiting",
                holders_text(&held_by)
            );
            out::count(&ws, "delta-wait");
            Flock::take(open_lock(&lock_path)?, Mode::Exclusive)?
        }
    };
    match rec {
        Some(r) => {
            if let Some(why) = lost(&ws, &id, &r)? {
                out::count(&ws, "delta-lost");
                println!("out: ws/{id} ram delta lost ({why}): remade");
                // under the mount lock taken above: drop_ram would wait for itself
                remove_ram(&ws, &id, &r)?;
                make_ram(root, &ws, &id, &store, true)?;
            }
        }
        None => {
            if let Err(e) = make_ram(root, &ws, &id, &store, true) {
                return pool(&format!("{e:#}"));
            }
        }
    }
    match (class, &stats) {
        (Some(class), Some(stats)) => println!(
            "out: delta ram ({} build, {:.2} GiB RAM reserved (own {:.2} GiB counted); store {}: {})",
            class.name(),
            gib(need.ram),
            gib(own),
            store.display(),
            stats.line()
        ),
        _ => println!(
            "out: delta ram (no growth, not admitted; store {})",
            store.display()
        ),
    }
    out::count(&ws, "delta-ram");
    // a cross host's part, laid under the mount lock (no mount of this upper runs now) and before unshare; the record is re-read: make_ram / lost above may have rewritten it
    if let Some(host) = cross_host {
        let mut rec = record(&dir)?.context("ram dir has no record after its make")?;
        if let Some(line) = lay_part(root, &ws, &id, &mut rec, host)? {
            println!("{line}");
        }
    }
    // every process of the namespace inherits the fd, and the lock is never unlocked by hand: it lives while any of them does (crate::flock's unlock-on-drop would free it for all)
    let mut lock = std::mem::ManuallyDrop::new(lock);
    rustix::io::fcntl_setfd(&*lock.file(), rustix::io::FdFlags::empty())
        .context("keeping the mount lock across exec")?;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let mut cmd = Command::new("unshare");
    cmd.args([
        "--user",
        "--map-root-user",
        "--mount",
        "--propagation",
        "private",
        "--",
    ])
    .arg(std::env::current_exe().context("finding xtask's executable")?)
    .args(std::env::args_os().skip(1))
    .env(ENV, format!("mount:{uid}:{gid}"));
    // the growth sample: own part of the upper before and after, the reservation still held (a build only)
    let before = class
        .map(|c| own_bytes(&upper, c, cross_host, &dirs))
        .transpose()?;
    let code = run(cmd, "unshare (delta mount namespace)")?;
    // the namespace's processes may still use the overlay (a daemon a build spawned: sccache server, gradle, ...); the mount ends with the command, so they do too
    end_holders(&ws, &id, &lock_path, std::process::id())?;
    let after = class
        .map(|c| own_bytes(&upper, c, cross_host, &dirs))
        .transpose()?;
    // the whole upper's size at this command's end, under the mount lock: the `used` stamp (idle time in the wait lines, sizes for eviction, the re-walk signal of own_at); a lost stamp only shows the upper's mtime
    let total = own_bytes(&upper, Class::Small, None, &[])?;
    let _ = std::fs::write(store.join(&id).join(USED), format!("{total}\n"));
    if let (Some(before), Some(after)) = (before, after) {
        let (line, over) = growth(&id, before, after, need.raw);
        println!("{line}");
        if over {
            out::count(&ws, "delta-over");
        }
    }
    drop(reservation);
    Ok(Some(code))
}

/// The verdict of a build that admission does not admit. A new dir takes the pool: nothing to keep, one line. An existing ram dir reaches here only on NeverFits, or on Refuse / Held inside land's queue ([`admit_keeping`] queues every other case). It is never dropped here: its upper holds the outputs and the `built-from` record of the checkout's last build, and a pool dir from base starts without them (check then refuses it as built from another source state). The build is refused (exit 1) and the dir stays, counted `delta-refused`; the line names the verdict and whether a running command holds the dir. Drops stay explicit: `out tier`, a lost store dir, a rebase's refresh.
fn fall_to_pool(
    ws: &Path,
    id: &str,
    class: Class,
    rec: Option<&Record>,
    v: Verdict,
) -> Result<Option<ExitCode>> {
    let why = format!("{} build: {}", class.name(), v.reason());
    if rec.is_none() {
        println!("out: delta pool (ws/{id}: {why}; no ram delta yet: nothing to keep)");
        out::count(ws, "delta-pool");
        return Ok(None);
    }
    out::count(ws, "delta-refused");
    // the probe only: nothing is removed, so the lock goes back at once
    let busy = try_mount_lock(ws, id)?.is_none();
    let tail = if busy {
        "in use by a running command of this checkout; run it again when that command ends"
    } else if matches!(v, Verdict::NeverFits(_)) {
        "the class never fits this store, or the store is unavailable"
    } else {
        "refused inside land's queue, which cannot wait for itself"
    };
    bail!(
        "ws/{id}: {why}; its ram delta is kept (outputs and record intact): this build did not start ({tail})"
    )
}

/// A process sharing the locked open file of a mount lock: it inherited the fd from the outer command.
#[derive(Debug, PartialEq)]
pub struct Holder {
    pub pid: u32,
    /// `/proc/<pid>/cmdline` with spaces, or `comm` when that is empty.
    pub cmd: String,
}

/// Processes holding the flock on `lock` through its locked open file description: an fd on the file's inode whose fdinfo carries an exclusive FLOCK line.
/// A process that merely opened the file (a failed `try_mount_lock` of another command) has its own description without the lock and is not one. `except` = the caller. Unreadable /proc entries (other users, exited) are skipped.
fn holders(lock: &Path, except: u32) -> Result<Vec<Holder>> {
    let meta = std::fs::metadata(lock).with_context(|| format!("stat {}", lock.display()))?;
    let (dev, ino) = (meta.dev(), meta.ino());
    let mut found = Vec::new();
    for e in std::fs::read_dir("/proc")
        .context("reading /proc")?
        .flatten()
    {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == except {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            std::fs::metadata(fd.path()).is_ok_and(|m| m.dev() == dev && m.ino() == ino)
                && std::fs::read_to_string(e.path().join("fdinfo").join(fd.file_name())).is_ok_and(
                    |t| {
                        t.lines().any(|l| {
                            l.starts_with("lock:") && l.contains("FLOCK") && l.contains("WRITE")
                        })
                    },
                )
        });
        if holds {
            let cmd = std::fs::read(e.path().join("cmdline"))
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .replace('\0', " ")
                        .trim()
                        .to_owned()
                })
                .unwrap_or_default();
            let cmd = if cmd.is_empty() {
                std::fs::read_to_string(e.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            } else {
                cmd
            };
            found.push(Holder { pid, cmd });
        }
    }
    Ok(found)
}

fn holders_text(h: &[Holder]) -> String {
    if h.is_empty() {
        return "no holder visible in /proc: another user's process, or it just ended".to_owned();
    }
    let list: Vec<String> = h
        .iter()
        .map(|h| format!("pid {} `{}`", h.pid, h.cmd))
        .collect();
    format!("held by {}", list.join(", "))
}

/// How long a holder left at command end gets after SIGTERM before SIGKILL: a daemon's own shutdown (sccache server measured 34 ms from SIGTERM to exit; JVM daemons run shutdown hooks), not build work, which has ended.
const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// Poll step while holders exit: well under the measured 34 ms sccache exit, so the wait adds little past the last exit.
const EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// Ends every process still holding mount lock `lock` of checkout `id` (except the caller): SIGTERM, then SIGKILL past [`TERM_GRACE`]. Returns once none holds it, so the next command of the checkout takes the lock at once.
/// Counter `delta-linger` (once per command that left any). Err when a holder survives SIGKILL past the grace (uninterruptible sleep), naming it.
fn end_holders(ws: &Path, id: &str, lock: &Path, except: u32) -> Result<usize> {
    let first = holders(lock, except)?;
    if first.is_empty() {
        return Ok(0);
    }
    println!(
        "out: ws/{id}: ending {} process(es) left in the ram delta namespace at command end: {}",
        first.len(),
        holders_text(&first)
    );
    out::count(ws, "delta-linger");
    let signal_all = |sig: rustix::process::Signal, h: &[Holder]| {
        for p in h {
            if let Some(pid) = i32::try_from(p.pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            {
                // gone between listing and signalling: nothing to end
                let _ = rustix::process::kill_process(pid, sig);
            }
        }
    };
    let wait_gone = || -> Result<Vec<Holder>> {
        let until = std::time::Instant::now() + TERM_GRACE;
        loop {
            let left = holders(lock, except)?;
            if left.is_empty() || std::time::Instant::now() >= until {
                return Ok(left);
            }
            std::thread::sleep(EXIT_POLL);
        }
    };
    signal_all(rustix::process::Signal::TERM, &first);
    let left = wait_gone()?;
    if !left.is_empty() {
        signal_all(rustix::process::Signal::KILL, &left);
        let left = wait_gone()?;
        ensure!(
            left.is_empty(),
            "ws/{id}: {} survived SIGKILL; the ram delta stays locked until it ends",
            holders_text(&left)
        );
    }
    Ok(first.len())
}

/// Why ram dir `rec` of checkout `id` has no intact upper (upper or work missing); None = intact. The test `lost` and [`pins`] share.
pub fn lost_upper(id: &str, rec: &Record) -> Option<String> {
    let sd = rec.dir(id);
    (!sd.join("upper").is_dir() || !sd.join("work").is_dir())
        .then(|| format!("no upper in {}", sd.display()))
}

/// Whether ram dir `rec` of checkout `id` still pins its base (`out prune`): no intact upper, no claim.
/// A store that is not a usable RAM store proves nothing about its uppers (the mount may be invisible in this namespace), so the pin stands.
/// Base presence is `lost`'s other half, not here.
pub fn pins(id: &str, rec: &Record, store_usable: bool) -> bool {
    !store_usable || lost_upper(id, rec).is_none()
}

/// Why ram dir `rec` cannot be mounted (its upper or base is gone, e.g. a WSL restart emptied the store, or a laid part is gone under an arch dir the upper built over it); None = intact.
/// A gone part with no `upper/<arch>` is no loss: [`lay_part`] drops its line and lays the newest part again.
fn lost(ws: &Path, id: &str, rec: &Record) -> Result<Option<String>> {
    if let Some(why) = lost_upper(id, rec) {
        // an evicted dir says why ([`evict_one`]'s marker), else the missing path
        return Ok(Some(
            match std::fs::read_to_string(out::dir_of(ws, id).join(EVICTED)) {
                Ok(t) => format!("evicted: {}", t.trim()),
                Err(_) => why,
            },
        ));
    }
    if !out::base_of(ws, &rec.base).is_dir() {
        return Ok(Some(format!("base/{} is gone", rec.base)));
    }
    let sd = rec.dir(id);
    for (arch, commit) in &rec.parts {
        if !out::part_root(ws, arch).join(commit).is_dir() && upper_host_exists(&sd, arch) {
            return Ok(Some(format!(
                "part {arch}@{commit} is gone under the upper's {arch}/"
            )));
        }
    }
    Ok(None)
}

/// Whether `upper/<host>` exists in any form (dir, whiteout, link): an arch dir there shadows a part laid under it.
fn upper_host_exists(sd: &Path, host: &str) -> bool {
    std::fs::symlink_metadata(sd.join("upper").join(host)).is_ok()
}

/// Lays the newest part of `host` under ram dir `rec` (`enter` calls it under the mount lock, before unshare, after make_ram's restore).
/// Drops recorded parts whose dir is gone (lost() took those with an `upper/<arch>`). Lays only when no part is recorded for `host` and no `upper/<host>` exists: a part under an arch dir would be shadowed.
/// Order: touch (paths differing from the part get now), then record: a crash between them leaves no record, so the next run touches again. Returns the line; None = nothing laid.
fn lay_part(
    root: &Path,
    ws: &Path,
    id: &str,
    rec: &mut Record,
    host: &str,
) -> Result<Option<String>> {
    let _pin = bases_lock(ws)?;
    let sd = rec.dir(id);
    let before = rec.parts.len();
    rec.parts
        .retain(|arch, commit| out::part_root(ws, arch).join(commit).is_dir());
    let dropped = rec.parts.len() != before;
    let mut line = None;
    if !rec.parts.contains_key(host)
        && !upper_host_exists(&sd, host)
        && let Some(part) = out::pick_part(root, ws, host)?
    {
        if part != rec.base {
            crate::checkout_stamps::touch_changed_since(root, &part)?;
        }
        rec.parts.insert(host.to_owned(), part.clone());
        out::count(ws, "delta-part");
        line = Some(format!("out: ws/{id} part {host}@{part} laid"));
    }
    if dropped || line.is_some() {
        write_record(&out::dir_of(ws, id), rec)?;
    }
    Ok(line)
}

/// Rewrites the record of ram dir `dir` (temp + rename: a crash keeps the old text). Called only outside the namespace, under the mount lock.
fn write_record(dir: &Path, rec: &Record) -> Result<()> {
    let tmp = dir.join(format!("{RECORD}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, rec.text()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(RECORD))
        .with_context(|| format!("renaming {} over the record", tmp.display()))
}

/// The overlay mount options of ram dir `rec` of checkout `id`. Overlay splits options at `,` and lowerdir at `:`, which a path may not hold.
pub fn mount_options(ws: &Path, id: &str, rec: &Record) -> Result<String> {
    let sd = rec.dir(id);
    let lower = out::base_of(ws, &rec.base);
    let (upper, work) = (sd.join("upper"), sd.join("work"));
    // each recorded part's commit dir (`pool/part/<arch>/<commit>`) is a lower after the base: trees are disjoint, so order has no effect
    let parts: Vec<PathBuf> = rec
        .parts
        .iter()
        .map(|(arch, commit)| out::part_root(ws, arch).join(commit))
        .collect();
    for p in [&lower, &upper, &work].into_iter().chain(parts.iter()) {
        let s = p.to_string_lossy();
        ensure!(
            !s.contains(',') && !s.contains(':'),
            "{s}: an overlay path may not contain ',' or ':'"
        );
    }
    let mut lowers = vec![lower.display().to_string()];
    lowers.extend(parts.iter().map(|p| p.display().to_string()));
    Ok(format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        lowers.join(":"),
        upper.display(),
        work.display()
    ))
}

/// Stage mount: uid 0 in a fresh user + mount namespace; mounts the overlay at the dir's real path, forgets a fresh dir's units reading foreign paths (out::foreign_paths; counter `delta-forget` per pass), then runs the command as `uid`/`gid` again (stage in).
fn mount_stage(root: &Path, uid: u32, gid: u32) -> Result<ExitCode> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = out::checkout_id(root)?;
    let dir = out::dir_of(&ws, &id);
    let rec = record(&dir)?.with_context(|| format!("{} has no {RECORD}", dir.display()))?;
    let status = Command::new("mount")
        .args(["-t", "overlay", "overlay", "-o"])
        .arg(mount_options(&ws, &id, &rec)?)
        .arg(&dir)
        .status()
        .context("running mount")?;
    ensure!(
        status.success(),
        "mounting the overlay at {} failed: {status}",
        dir.display()
    );
    let fresh = rec.dir(&id).join(FRESH);
    if fresh.exists() {
        let n = out::forget_runs_reading(&dir, &out::foreign_paths(&ws, &id, true))?;
        std::fs::remove_file(&fresh).with_context(|| format!("removing {}", fresh.display()))?;
        // the line may go to a captured child (land --build-dir): the counter is the record that the pass ran
        out::count(&ws, "delta-forget");
        println!(
            "out: ws/{id}: {n} test/bin unit(s) reading a path this checkout's builds do not see forgotten"
        );
    }
    let mut cmd = Command::new("unshare");
    cmd.args(["--user", "--map-user"])
        .arg(uid.to_string())
        .arg("--map-group")
        .arg(gid.to_string())
        .arg("--")
        .arg(std::env::current_exe().context("finding xtask's executable")?)
        .args(std::env::args_os().skip(1))
        .env(ENV, "in");
    run(cmd, "unshare (delta caller uid)")
}

fn run(mut cmd: Command, what: &str) -> Result<ExitCode> {
    let status = cmd
        .current_dir(std::env::current_dir()?)
        .status()
        .with_context(|| format!("running {what}"))?;
    Ok(match status.code() {
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(limit_gib: f64, used_gib: f64, ratio_milli: u64) -> Stats {
        let g = |x: f64| (x * GIB as f64) as u64;
        Stats {
            backing: Backing::Zram,
            fs_size: 16 * GIB,
            fs_free: 14 * GIB,
            ram_limit: g(limit_gib),
            ram_used: g(used_gib),
            ratio_milli,
            ratio_measured: true,
        }
    }

    /// `b` raw bytes at `s`'s ratio (the D3 formula, as `Need::of` for a build class).
    fn raw(s: &Stats, b: u64) -> Need {
        Need {
            raw: b,
            ram: (b * 1000).div_ceil(s.ratio_milli),
        }
    }

    /// Expected values from the D3 formula, worked by hand: need_ram = ceil(need_raw * 1000 / ratio_milli).
    /// check: 4915 MiB = 5_153_751_040 B at 1.95 -> 5_153_751_040_000 / 1950 = 2_642_949_251.3 -> ceil 2_642_949_252 B (2.46 GiB); limit 8, used 2.5, margin 1 -> RAM free 5.5 GiB.
    #[test]
    fn admission_arithmetic() {
        let s = stats(8.0, 2.5, 1950);
        let check = 4915 * MIB;
        assert_eq!(
            judge(&s, raw(&s, check), 0, Hold::default(), GIB),
            Verdict::Admit {
                need: Need {
                    raw: check,
                    ram: 2_642_949_252,
                }
            }
        );
        // 2.46 + 2.5 reserved + 1 = 5.96 > 5.5 free: others' reservations stand in the way -> wait
        assert!(matches!(
            judge(&s, raw(&s, check), 5 * GIB / 2, Hold::default(), GIB),
            Verdict::Wait(_)
        ));
        // used 5.0: free 3.0 < 2.46 + 1 -> refused before starting, whatever is reserved
        assert!(matches!(
            judge(
                &stats(8.0, 5.0, 1950),
                raw(&stats(8.0, 5.0, 1950), check),
                0,
                Hold::default(),
                GIB
            ),
            Verdict::Refuse(_)
        ));
        // cross 20 GiB at 1.95 = 10.26 GiB RAM > limit 8 - margin: never fits -> pool tier; also > fs size 16
        assert!(matches!(
            judge(&s, raw(&s, 20480 * MIB), 0, Hold::default(), GIB),
            Verdict::NeverFits(_)
        ));
        // tmpfs ratio 1.0: 4.8 GiB raw = 4.8 RAM; limit 6, used 0: 4.8 + 1 = 5.8 <= 6
        let t = Stats {
            backing: Backing::Tmpfs,
            ratio_milli: 1000,
            ..stats(6.0, 0.0, 1000)
        };
        assert_eq!(
            judge(&t, raw(&t, check), 0, Hold::default(), GIB),
            Verdict::Admit {
                need: Need {
                    raw: check,
                    ram: 4915 * MIB,
                }
            }
        );
        // raw room is judged too: fs free 14 GiB < 15 GiB need with RAM to spare
        let big = Stats {
            ram_limit: 64 * GIB,
            ..stats(64.0, 0.0, 4000)
        };
        assert!(matches!(
            judge(&big, raw(&big, 15 * GIB), 0, Hold::default(), GIB),
            Verdict::Refuse(_)
        ));
    }

    /// Growth of a build over its own upper: raw = min(peak, max(regrow 4 GiB, peak - own)), hand values at ratio 1.95.
    /// native 9 GiB: own 0 -> 9 GiB; own 3 -> 6 GiB (3_303_820_997 B RAM); own 6.5 -> regrow floor 4 GiB (2_202_547_332 B); own 9.5 (above peak) -> 4 GiB. small 2 GiB: regrow never raises above peak.
    #[test]
    fn need_credits_own_upper_above_the_regrow_floor() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let s = stats(64.0, 0.0, 1950);
        let need = |class, own| Need::of(class, &cfg, &s, own);
        assert_eq!(
            need(Class::Native, 0),
            Need {
                raw: 9 * GIB,
                ram: 4_955_731_496
            }
        );
        assert_eq!(
            need(Class::Native, 3 * GIB),
            Need {
                raw: 6 * GIB,
                ram: 3_303_820_997
            }
        );
        assert_eq!(
            need(Class::Native, 13 * GIB / 2),
            Need {
                raw: 4 * GIB,
                ram: 2_202_547_332
            }
        );
        assert_eq!(
            need(Class::Native, 19 * GIB / 2),
            Need {
                raw: 4 * GIB,
                ram: 2_202_547_332
            }
        );
        assert_eq!(
            need(Class::Small, GIB),
            Need {
                raw: 2 * GIB,
                ram: 1_101_273_666
            }
        );
        // delta.regrow_gib = 2: own 6.5 -> max(2, 2.5) = 2.5 GiB
        let two =
            crate::ws_config::parse("[delta]\nregrow_gib = 2\n", Path::new("/w/workspace.toml"))
                .expect("cfg")
                .delta;
        assert_eq!(
            Need::of(Class::Native, &two, &s, 13 * GIB / 2),
            Need {
                raw: 5 * GIB / 2,
                ram: 1_376_592_083
            }
        );
    }

    /// Two native builds on 12 GiB fs free: the first is admitted; while its reservation stands, the other's 9_663_676_417 raw B (4_955_731_496 RAM at 1.95) makes 9 + 9 GiB > 12, so the second waits. Red control: the raw of others uncounted admits both.
    #[test]
    fn reservations_count_their_raw_on_the_fs() {
        let s = Stats {
            fs_free: 12 * GIB,
            ..stats(64.0, 0.0, 1950)
        };
        let native = Need {
            raw: 9 * GIB,
            ram: 4_955_731_496,
        };
        assert!(matches!(
            judge(&s, native, 0, Hold::default(), GIB),
            Verdict::Admit { .. }
        ));
        assert!(matches!(
            judge(&s, native, 4_955_731_496, Hold::default(), GIB),
            Verdict::Wait(_)
        ));
        // a small build (2 GiB raw, 1_101_273_666 RAM) still fits beside it: 2 GiB + 9_663_676_417 B < 12 GiB
        let small = Need {
            raw: 2 * GIB,
            ram: 1_101_273_666,
        };
        assert!(matches!(
            judge(&s, small, 4_955_731_496, Hold::default(), GIB),
            Verdict::Admit { .. }
        ));
    }

    /// Measured store of gnw-dod2 (8.97 GiB = 9184 MiB fs free, own 6.5 GiB = 6656 MiB, native): without the own credit the 9 GiB peak refuses on raw; with it the 4 GiB floor admits.
    #[test]
    fn own_credit_admits_a_native_over_an_existing_upper() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let s = Stats {
            fs_free: 9184 * MIB,
            ..stats(64.0, 0.0, 1950)
        };
        assert!(matches!(
            judge(
                &s,
                Need::of(Class::Native, &cfg, &s, 0),
                0,
                Hold::default(),
                GIB
            ),
            Verdict::Refuse(_)
        ));
        assert!(matches!(
            judge(
                &s,
                Need::of(Class::Native, &cfg, &s, 6656 * MIB),
                0,
                Hold::default(),
                GIB
            ),
            Verdict::Admit { .. }
        ));
    }

    /// `own_bytes` on a fixture upper (8 KiB files, one hard link, dirs included): native counts everything but `upper/riscv64`, cross only `upper/riscv64`, run 0, a missing upper 0.
    /// The oracle sums st_blocks * 512 over the listed entries, each inode once.
    #[test]
    fn own_bytes_counts_each_class_its_part() {
        let root = std::env::temp_dir().join(format!("xtask-delta-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let upper = root.join("upper");
        std::fs::create_dir_all(upper.join("riscv64").join("deep")).unwrap();
        std::fs::create_dir_all(upper.join("sub")).unwrap();
        let blob = vec![7u8; 8192];
        std::fs::write(upper.join("a.bin"), &blob).unwrap();
        std::fs::hard_link(upper.join("a.bin"), upper.join("a-link.bin")).unwrap();
        std::fs::write(upper.join("sub").join("b.bin"), &blob).unwrap();
        std::fs::write(upper.join("riscv64").join("c.bin"), &blob).unwrap();
        std::fs::write(upper.join("riscv64").join("deep").join("d.bin"), &blob).unwrap();
        let dirs = vec!["riscv64".to_owned()];
        let x86_side = [
            upper.clone(),
            upper.join("a.bin"),
            upper.join("a-link.bin"),
            upper.join("sub"),
            upper.join("sub").join("b.bin"),
        ];
        let riscv_side = [
            upper.join("riscv64"),
            upper.join("riscv64").join("c.bin"),
            upper.join("riscv64").join("deep"),
            upper.join("riscv64").join("deep").join("d.bin"),
        ];
        assert_eq!(
            own_bytes(&upper, Class::Native, None, &dirs).unwrap(),
            allocated(&x86_side)
        );
        assert_eq!(
            own_bytes(&upper, Class::Cross, Some("riscv64"), &dirs).unwrap(),
            allocated(&riscv_side)
        );
        assert_eq!(own_bytes(&upper, Class::Run, None, &dirs).unwrap(), 0);
        assert_eq!(
            own_bytes(&root.join("missing"), Class::Native, None, &dirs).unwrap(),
            0
        );
        assert_eq!(own_bytes(&upper, Class::Cross, None, &dirs).unwrap(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// st_blocks * 512 summed over `paths`, each inode once (the oracle of `own_bytes_counts_each_class_its_part`).
    fn allocated(paths: &[PathBuf]) -> u64 {
        let mut seen = std::collections::HashSet::new();
        let mut total = 0;
        for p in paths {
            let m = std::fs::symlink_metadata(p).unwrap();
            if seen.insert((m.dev(), m.ino())) {
                total += m.blocks() * 512;
            }
        }
        total
    }

    /// The growth sample: the line names the own part before -> after and the raw reservation; over = growth past it (counter `delta-over`).
    #[test]
    fn growth_sample_flags_growth_past_the_reservation() {
        let (line, over) = growth("c", 2 * GIB, 7 * GIB, 4 * GIB);
        assert_eq!(
            line,
            "out: delta ws/c upper 2.00 -> 7.00 GiB raw (reserved 4.00)"
        );
        assert!(over, "grew 5 GiB against 4 reserved");
        assert!(
            !growth("c", 2 * GIB, 6 * GIB, 4 * GIB).1,
            "grew exactly the reservation"
        );
        assert!(growth("c", 0, 4 * GIB + 1, 4 * GIB).1);
    }

    /// Land first, hand arithmetic: zram limit 10 GiB, used 3, margin 1 -> RAM free 7 GiB = 7_516_192_768 B; ratio 1.95.
    /// native 9216 MiB = 9_663_676_416 B -> ram ceil(9_663_676_416_000 / 1950) = 4_955_731_496 B (4.62 GiB, plan S3 "land hold 4.62"); check 2_642_949_252 B (admission_arithmetic).
    #[test]
    fn land_hold_counts_while_land_is_active() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let s = stats(10.0, 3.0, 1950);
        let native = Hold {
            raw: 9 * GIB,
            ram: 4_955_731_496,
        };
        let active = LandState {
            active: true,
            reserved: false,
        };
        assert_eq!(hold_for("child", &cfg, &s, active), native);
        let check = Need::of(Class::Check, &cfg, &s, 0);
        assert_eq!(check.ram, 2_642_949_252);
        // with the hold: 2.46 + 4.62 + 1 = 8.07 GiB > 7 free -> held (a new dir takes the pool)
        assert!(matches!(judge(&s, check, 0, native, GIB), Verdict::Held(_)));
        // land's own admission ignores the hold: 4.62 + 1 = 5.62 <= 7
        assert_eq!(hold_for(out::LAND_ID, &cfg, &s, active), Hold::default());
        let land = Need::of(Class::Native, &cfg, &s, 0);
        assert_eq!(
            judge(&s, land, 0, Hold::default(), GIB),
            Verdict::Admit {
                need: Need {
                    raw: 9 * GIB,
                    ram: 4_955_731_496,
                }
            }
        );
        // red control, no hold: the child is admitted, and land behind its reservation waits (4.62 + 2.46 + 1 > 7)
        assert_eq!(
            judge(&s, check, 0, Hold::default(), GIB),
            Verdict::Admit {
                need: Need {
                    raw: 4915 * MIB,
                    ram: 2_642_949_252,
                }
            }
        );
        assert!(matches!(
            judge(&s, land, check.ram, Hold::default(), GIB),
            Verdict::Wait(_)
        ));
        // land's live reservation stands in for the hold; a store native never fits (limit 4 < 4.62 + 1) holds nothing
        let land_reserved = LandState {
            active: true,
            reserved: true,
        };
        assert_eq!(hold_for("child", &cfg, &s, land_reserved), Hold::default());
        assert_eq!(
            hold_for("child", &cfg, &stats(4.0, 0.0, 1950), active),
            Hold::default()
        );
        // the hold on fs raw: fs free 14, check 4.8 + native 9 = 13.8 fits; with fs free 13 it is held though RAM has room
        let roomy = Stats {
            fs_free: 13 * GIB,
            ..stats(64.0, 0.0, 1950)
        };
        assert!(matches!(
            judge(
                &roomy,
                check,
                0,
                hold_for("child", &cfg, &roomy, active),
                GIB
            ),
            Verdict::Held(_)
        ));
    }

    /// Idle land holds nothing: the hold counts only while land's lock is held. On the store above the check fits without the hold; a land reservation counts once, in the others' bytes.
    #[test]
    fn land_hold_is_off_while_land_is_idle() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let roomy = Stats {
            fs_free: 13 * GIB,
            ..stats(64.0, 0.0, 1950)
        };
        let check = Need::of(Class::Check, &cfg, &roomy, 0);
        let idle = LandState::default();
        assert_eq!(hold_for("child", &cfg, &roomy, idle), Hold::default());
        assert_eq!(
            judge(&roomy, check, 0, hold_for("child", &cfg, &roomy, idle), GIB),
            Verdict::Admit {
                need: Need {
                    raw: 4915 * MIB,
                    ram: 2_642_949_252,
                }
            }
        );
    }

    /// Land's state is the kernel's view of `out/land.lock`: no file or a free lock = idle; a lock held by another process (land-seq.sh's exclusive flock) = active, until that process exits.
    #[test]
    fn land_queue_lock_is_read_as_held() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-land-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("out")).unwrap();
        let lock = ws.join("out").join("land.lock");
        assert!(!land_active(&ws).unwrap(), "no lock file: no land yet");
        drop(open_lock(&lock).unwrap());
        assert!(!land_active(&ws).unwrap(), "a free lock: idle");
        // land-seq.sh's shape: an fd on the file, flocked exclusive by another process
        let mut land_seq = sleeper_on(&lock, true);
        assert!(
            land_active(&ws).unwrap(),
            "held by another process: a land runs or queues"
        );
        let _ = land_seq.kill();
        let _ = land_seq.wait();
        assert!(!land_active(&ws).unwrap(), "released when the holder exits");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// The hold follows land's lock on one store: a worker's check (room for it, not for land's native build beside it) is Held while land is active and admitted when idle; land's own reservation counts once, as others' bytes (Wait, not Held).
    #[test]
    fn land_hold_follows_the_land_lock() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-land-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let s = stats(10.0, 3.0, 1950);
        let check = || {
            admit_once(&ws, "c", &cfg, Class::Check, 0, Ok(s.clone()))
                .unwrap()
                .map(|_| ())
        };
        assert_eq!(check(), Ok(()), "land idle: no hold");
        let queued = hold_land_lock(&ws);
        assert!(
            matches!(check(), Err(Verdict::Held(_))),
            "land active: held"
        );
        assert_eq!(count_of(&ws, "delta-land-hold"), 1);
        drop(queued);
        assert_eq!(check(), Ok(()), "land idle again: admitted");
        // land's reservation (4.62 GiB) + the check (2.46) + margin 1 > 7 free: Wait on the reservation; the hold is not counted again
        let land = reserve(&ws, out::LAND_ID, 4_955_731_496).unwrap();
        let queued = hold_land_lock(&ws);
        assert!(
            matches!(check(), Err(Verdict::Wait(_))),
            "land reserved: counted once"
        );
        assert_eq!(count_of(&ws, "delta-land-hold"), 1);
        drop((land, queued));
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A command that grows no upper (a prune) is admitted on a full store with no reservation. Judged as a small build it is refused there: 0.8 GiB RAM free, under the 1 GiB margin.
    /// Break (the None arm judged as a small build, `keep` false: refused at once, no wait): red at the prune's assertion.
    #[test]
    fn a_command_that_grows_nothing_is_admitted_on_a_full_store() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-no-growth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let full = stats(10.0, 9.2, 1950);
        let small = admit_once(&ws, "land", &cfg, Class::Small, 0, Ok(full.clone())).unwrap();
        assert!(
            matches!(small, Err(Verdict::Refuse(_))),
            "a small build on the full store: {:?}",
            small.as_ref().err()
        );
        let prune = admit_command(
            &ws,
            "land",
            &cfg,
            None,
            || Ok(0),
            true,
            None,
            || Ok(Ok(full.clone())),
        )
        .unwrap();
        assert!(
            matches!(prune, Ok(None)),
            "a prune on the full store: {:?}",
            prune.as_ref().err()
        );
        assert_eq!(
            reserved(&ws).unwrap(),
            (0, false),
            "a prune reserves nothing"
        );
        let roomy = stats(10.0, 3.0, 1950);
        let got = admit_command(
            &ws,
            "land",
            &cfg,
            None,
            || Ok(0),
            true,
            None,
            || Ok(Ok(roomy.clone())),
        )
        .unwrap();
        assert!(
            matches!(got, Ok(None)),
            "a prune on a roomy store was judged (Admit reserves) or refused: {:?}",
            got.as_ref().err()
        );
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A land arriving while workers' reservations fill the store waits: its verdict is Wait naming them, the wait is counted `delta-wait` and it is admitted when they end. The hold never applies to land.
    #[test]
    fn land_waits_while_workers_reserve_the_store() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-land-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let s = stats(10.0, 3.0, 1950);
        let queued = hold_land_lock(&ws);
        let mut worker = Some(reserve(&ws, "worker", 3 * GIB).unwrap());
        // land 4.62 + worker 3 + margin 1 > 7 free: Wait, and the reason names the reservations
        let waiting = judge_now(&ws, out::LAND_ID, &cfg, Class::Native, 0, &s).unwrap();
        assert!(
            matches!(&waiting, Verdict::Wait(why) if why.contains("reserved by running builds")),
            "{waiting:?}"
        );
        let mut probes = 0;
        let (land, _, need) = admit(&ws, out::LAND_ID, &cfg, Class::Native, 0, || {
            probes += 1;
            if probes == 2 {
                worker.take();
            }
            Ok(Ok(s.clone()))
        })
        .unwrap()
        .expect("land admitted once the worker's reservation ends");
        assert_eq!((probes, need.ram), (2, 4_955_731_496));
        assert_eq!(count_of(&ws, "delta-wait"), 1);
        assert_eq!(count_of(&ws, "delta-land-hold"), 0, "land is never held");
        drop((land, queued));
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A land's new dir on a store full by its own accounting (8 GiB limit, 7.5 used: no reservation ends) takes the pool, counted `delta-pool`; land holds nothing.
    #[test]
    fn land_takes_the_pool_when_the_store_is_full() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-land-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let queued = hold_land_lock(&ws);
        let Err(v) = admit(&ws, out::LAND_ID, &cfg, Class::Native, 0, || {
            Ok(Ok(stats(8.0, 7.5, 1950)))
        })
        .unwrap() else {
            panic!("land admitted on a full store");
        };
        assert!(matches!(v, Verdict::Refuse(_)), "{v:?}");
        assert!(
            fall_to_pool(&ws, out::LAND_ID, Class::Native, None, v)
                .unwrap()
                .is_none()
        );
        assert_eq!(count_of(&ws, "delta-pool"), 1);
        assert_eq!(count_of(&ws, "delta-land-hold"), 0);
        drop(queued);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Land queued or running on `ws`: this process holds `out/land.lock` exclusively, as land-seq.sh does, until the guard drops.
    fn hold_land_lock(ws: &Path) -> Flock {
        Flock::take(
            open_lock(&ws.join("out").join("land.lock")).unwrap(),
            Mode::Exclusive,
        )
        .unwrap()
    }

    /// Class run takes min(device ratio, delta.ratio_run 1.37): 797 MiB = 835_715_072 B -> ceil(835_715_072_000 / 1370) = 610_011_002 B at device 1.95 (not 428_571_833 at 1.95); tmpfs 1.0 stays 1.0.
    #[test]
    fn run_class_reserves_at_the_app_data_ratio() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let run = Need::of(Class::Run, &cfg, &stats(10.0, 0.0, 1950), 0);
        assert_eq!((run.raw, run.ram), (835_715_072, 610_011_002));
        let t = stats(10.0, 0.0, 1000);
        assert_eq!(Need::of(Class::Run, &cfg, &t, 0).ram, 835_715_072);
        // build classes keep the device ratio
        assert_eq!(
            Need::of(Class::Check, &cfg, &stats(10.0, 0.0, 1950), 0).ram,
            2_642_949_252
        );
    }

    /// Red control: a full store refuses in the admission step, so the build (here a closure that would record its start) never runs.
    #[test]
    fn full_store_refuses_before_the_build_starts() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = crate::ws_config::parse("", &ws.join("workspace.toml"))
            .unwrap()
            .delta;
        let mut started = false;
        let got = admit(&ws, "child", &cfg, Class::Check, 0, || {
            Ok(Ok(stats(8.0, 7.5, 1950)))
        })
        .unwrap();
        if got.is_ok() {
            started = true;
        }
        assert!(matches!(got, Err(Verdict::Refuse(_))), "{:?}", got.err());
        assert!(!started);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// [`sleeper_on`]'s child: killed and reaped on drop, so a test that panics leaves no holder of a lock behind (a `sleep` holding a checkout's mount lock makes its next ram command wait until it exits). Derefs to the child: kill, wait, id.
    struct Sleeper(std::process::Child);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            // std's kill is a no-op once the child is waited: a test's own kill + wait before this is harmless
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl std::ops::Deref for Sleeper {
        type Target = std::process::Child;
        fn deref(&self) -> &std::process::Child {
            &self.0
        }
    }

    impl std::ops::DerefMut for Sleeper {
        fn deref_mut(&mut self) -> &mut std::process::Child {
            &mut self.0
        }
    }

    /// A `sleep 300` whose fd 9 is open on `path`, flocked exclusive when `lock`; returns once the fd is set up (the child says so on stdout).
    /// The child opens and locks the file itself: an inheritable fd in this test process would leak into children other tests spawn meanwhile.
    fn sleeper_on(path: &Path, lock: bool) -> Sleeper {
        let script = if lock {
            r#"exec 9<>"$1"; flock -nx 9 || exit 1; echo ready; exec sleep 300"#
        } else {
            r#"exec 9<>"$1"; echo ready; exec sleep 300"#
        };
        let mut c = Sleeper(
            Command::new("sh")
                .args(["-c", script, "sh"])
                .arg(path)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(c.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line, "ready\n", "fixture {}", path.display());
        // "ready" is printed by the shell before its exec: wait for the exec itself, or a reader of /proc/<pid>/cmdline still sees the `sh -c ...` line (seen once under load). No timeout: the exec always follows.
        let cmdline = format!("/proc/{}/cmdline", c.id());
        while !std::fs::read(&cmdline).unwrap().starts_with(b"sleep\0") {
            std::thread::yield_now();
        }
        c
    }

    /// Fixture of a lingering daemon: a process alone holding checkout `id`'s mount lock, as a daemon left by a build does after the outer command closed its fd.
    /// Plus a bystander that opened the file without locking it (a failed try of another command): never a holder.
    fn lingering_holder(ws: &Path, id: &str) -> (Sleeper, Sleeper) {
        let p = mount_lock_path(ws, id);
        drop(open_lock(&p).unwrap());
        let bystander = sleeper_on(&p, false);
        (sleeper_on(&p, true), bystander)
    }

    /// A test that panics after [`sleeper_on`] leaves no `sleep` behind: the guard kills and reaps it while the panic unwinds.
    /// Break (no guard): the sleep outlives the test; killed here, so the run leaves nothing.
    #[test]
    fn a_panicking_test_leaves_no_sleeper() {
        let path = std::env::temp_dir().join(format!("xtask-delta-sleeper-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let pid = std::cell::Cell::new(0u32);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sleeper = sleeper_on(&path, true);
            pid.set(sleeper.id());
            panic!("fixture: the test fails while its sleeper runs");
        }));
        assert!(caught.is_err());
        let pid = pid.get();
        let alive = Path::new(&format!("/proc/{pid}")).exists();
        if alive {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let _ = std::fs::remove_file(&path);
        assert!(!alive, "sleeper {pid} outlived its panicking test");
    }

    #[test]
    fn wait_line_names_the_holder() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-holder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let (mut holder, mut bystander) = lingering_holder(&ws, "c");
        let got = holders(&mount_lock_path(&ws, "c"), std::process::id()).unwrap();
        let text = holders_text(&got);
        let _ = holder.kill();
        let _ = bystander.kill();
        let _ = holder.wait();
        let _ = bystander.wait();
        assert_eq!(
            got,
            [Holder {
                pid: holder.id(),
                cmd: "sleep 300".to_owned()
            }]
        );
        assert!(
            text.contains(&format!("pid {} `sleep 300`", holder.id())),
            "{text}"
        );
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A daemon left holding the lock blocks the next command (red before end_holders existed); once end_holders returns, the lock is free at once: its return is the bound, no wait time is assumed.
    #[test]
    fn lingering_holder_ends_with_the_command() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-linger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let (mut holder, mut bystander) = lingering_holder(&ws, "c");
        assert!(
            try_mount_lock(&ws, "c").unwrap().is_none(),
            "fixture holds the lock"
        );
        let ended = end_holders(&ws, "c", &mount_lock_path(&ws, "c"), std::process::id());
        let free = try_mount_lock(&ws, "c").unwrap().is_some();
        let bystander_alive = bystander.try_wait().unwrap().is_none();
        let _ = holder.kill();
        let _ = bystander.kill();
        let _ = holder.wait();
        let _ = bystander.wait();
        assert_eq!(ended.unwrap(), 1);
        assert!(free, "lock still held after end_holders");
        assert!(
            bystander_alive,
            "a process that only opened the file was ended"
        );
        assert_eq!(
            std::fs::read_to_string(out::pool(&ws).join("delta-linger.count"))
                .unwrap()
                .trim(),
            "1"
        );
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A live reservation counts for others; one whose holder is gone is swept (its flock is free).
    #[test]
    fn reservations_count_while_held() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let r = reserve(&ws, "a", 3 * GIB).unwrap();
        assert_eq!(reserved(&ws).unwrap(), (3 * GIB, false));
        let stale = lock_dir(&ws).join("reserve").join("land-1");
        std::fs::write(&stale, "7\n").unwrap();
        // a gone land's file is swept and is no live land reservation
        assert_eq!(reserved(&ws).unwrap(), (3 * GIB, false));
        assert!(!stale.exists());
        // a checkout named land-x is not land
        let lx = reserve(&ws, "land-x", GIB).unwrap();
        assert_eq!(reserved(&ws).unwrap(), (4 * GIB, false));
        let land = reserve(&ws, out::LAND_ID, GIB).unwrap();
        assert_eq!(reserved(&ws).unwrap(), (5 * GIB, true));
        drop((r, lx, land));
        assert_eq!(reserved(&ws).unwrap(), (0, false));
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// The same small build on each tier gives the same tree: pool = a copy of the base, ram = the overlay [`mount_options`] makes over it, mounted as stage mount does (uid 0 in a user + mount namespace, then back to the caller's uid), upper on /dev/shm (tmpfs: RAM-backed).
    /// The build does what cargo and ninja do to a base's outputs: append, rewrite, hard link (incremental), rename over, delete, set an old mtime, and make a dir; the listing (type, size, mtime, content) is taken where the dir is mounted.
    #[test]
    fn small_build_matches_on_both_tiers() {
        let pid = std::process::id();
        let ws = std::env::temp_dir().join(format!("xtask-delta-tiers-{pid}"));
        let _ = std::fs::remove_dir_all(&ws);
        let base = out::base_of(&ws, "c1");
        std::fs::create_dir_all(base.join("target/debug/deps")).unwrap();
        std::fs::write(base.join("target/debug/deps/a.rlib"), "a1").unwrap();
        std::fs::write(base.join("target/debug/deps/b.rlib"), "b1").unwrap();
        std::fs::write(base.join("build.ninja"), "rule cc\n").unwrap();
        std::fs::write(base.join("gone.o"), "x").unwrap();
        let build = ws.join("build.sh");
        std::fs::write(
            &build,
            "set -e; d=$1; cd \"$d\"\necho a2 >> target/debug/deps/a.rlib\necho rule ld > build.ninja.tmp && mv build.ninja.tmp build.ninja\nln target/debug/deps/b.rlib target/debug/deps/b-hl.rlib\nrm gone.o\nmkdir -p gen/x && echo g > gen/x/y.h\ntouch -d 2001-02-03 target/debug/deps/a.rlib\nfind . -path ./.delta -prune -o -type d -printf '%P d\\n' -o -type f -printf '%P f %s %n\\n' | sort > \"$2\"\nfind . -type f ! -name .delta -exec cksum {} + | sort >> \"$2\"\nstat -c '%n %Y' target/debug/deps/a.rlib target/debug/deps/b.rlib >> \"$2\"\n",
        )
        .unwrap();
        // pool: a copy
        let pool_dir = out::dir_of(&ws, "p");
        std::fs::create_dir_all(pool_dir.parent().unwrap()).unwrap();
        let ok = Command::new("cp")
            .args(["-a", "--"])
            .arg(&base)
            .arg(&pool_dir)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let pool_list = ws.join("pool.list");
        let ok = Command::new("sh")
            .arg(&build)
            .arg(&pool_dir)
            .arg(&pool_list)
            .status()
            .unwrap()
            .success();
        assert!(ok, "pool build");
        // ram: overlay over the base, upper in a RAM store
        let store = Path::new("/dev/shm").join(format!("xtask-delta-tiers-{pid}"));
        let _ = std::fs::remove_dir_all(&store);
        let rec = Record {
            base: "c1".into(),
            store: store.clone(),
            parts: BTreeMap::new(),
        };
        std::fs::create_dir_all(rec.dir("r").join("upper")).unwrap();
        std::fs::create_dir_all(rec.dir("r").join("work")).unwrap();
        let ram_dir = out::dir_of(&ws, "r");
        std::fs::create_dir_all(&ram_dir).unwrap();
        std::fs::write(ram_dir.join(RECORD), rec.text()).unwrap();
        let ram_list = ws.join("ram.list");
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let inner = format!(
            "mount -t overlay overlay -o \"$1\" \"$2\" && exec unshare --user --map-user={uid} --map-group={gid} -- sh \"$3\" \"$2\" \"$4\""
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
            .arg(&inner)
            .arg("sh")
            .arg(mount_options(&ws, "r", &rec).unwrap())
            .arg(&ram_dir)
            .arg(&build)
            .arg(&ram_list)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ram build");
        let (p, r) = (
            std::fs::read_to_string(&pool_list).unwrap(),
            std::fs::read_to_string(&ram_list).unwrap(),
        );
        // "b1" (2 bytes) under two names: the hard link the build made
        assert!(p.contains("deps/b-hl.rlib f 2 2"), "{p}");
        assert_eq!(p, r, "pool vs ram tree");
        // the build wrote the store, not the base or the mount point
        assert_eq!(
            std::fs::read_to_string(base.join("target/debug/deps/a.rlib")).unwrap(),
            "a1"
        );
        assert!(base.join("gone.o").exists());
        assert!(rec.dir("r").join("upper/gen/x/y.h").exists());
        let under: Vec<_> = std::fs::read_dir(&ram_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(under, [RECORD]);
        // drop_ram clears the store dir (overlay's mode-000 work dir included) and the mount point
        drop_ram(&ws, "r", &rec).unwrap();
        assert!(!rec.dir("r").exists() && !ram_dir.exists());
        std::fs::remove_dir_all(&ws).unwrap();
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Regression (2026-11-21): a scratch workspace's prune deleted a live checkout's upper in the shared store. Only dirs whose owner file names this workspace are candidates.
    #[test]
    fn orphan_store_dirs_are_this_workspaces_only() {
        let store = std::env::temp_dir().join(format!("xtask-delta-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let (me, other) = (Path::new("/w/me"), Path::new("/w/other"));
        for (id, owner) in [("mine", Some(me)), ("theirs", Some(other)), ("none", None)] {
            std::fs::create_dir_all(store.join(id).join("upper")).unwrap();
            if let Some(o) = owner {
                std::fs::write(store.join(id).join(OWNER), format!("{}\n", o.display())).unwrap();
            }
        }
        let keep = std::collections::BTreeSet::new();
        let got: Vec<String> = orphan_store_dirs(&store, me, &keep)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(got, ["mine"]);
        let keep = std::collections::BTreeSet::from(["mine".to_owned()]);
        assert!(orphan_store_dirs(&store, me, &keep).is_empty());
        std::fs::remove_dir_all(&store).unwrap();
    }

    /// Value of counter `name` of workspace `ws` (0 when never counted).
    fn count_of(ws: &Path, name: &str) -> u64 {
        std::fs::read_to_string(out::pool(ws).join(format!("{name}.count")))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Checkout `c`'s ram dir over base `b` in `store`, laid as make_ram lays it: the mount point with its record, and the store dir with upper, work and owner.
    fn ram_dir_fixture(ws: &Path, store: &Path) -> Record {
        let rec = Record {
            base: "b".into(),
            store: store.to_owned(),
            parts: BTreeMap::new(),
        };
        let sd = rec.dir("c");
        std::fs::create_dir_all(sd.join("upper")).unwrap();
        std::fs::create_dir_all(sd.join("work")).unwrap();
        std::fs::write(sd.join(OWNER), format!("{}\n", ws.display())).unwrap();
        let dir = out::dir_of(ws, "c");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RECORD), rec.text()).unwrap();
        rec
    }

    /// Admission refuses an existing ram dir: its upper holds the outputs of the checkout's last build (and the `built-from` record), its `.delta` record sits in the mount point. Both must survive the refusal; nothing falls to the pool (a fall dropped them, and the next check said "built from another source state"). `want` names the verdict the case must produce; `queued`: this process holds land's lock (as a process inside the queue does), so a hold is refused, not waited for.
    fn refusal_keeps_outputs(
        tag: &str,
        queued: bool,
        probe: impl FnMut() -> Result<std::result::Result<Stats, String>>,
        want: fn(&Verdict) -> bool,
    ) {
        let pid = std::process::id();
        let ws = std::env::temp_dir().join(format!("xtask-delta-kept-{tag}-{pid}"));
        let store = std::env::temp_dir().join(format!("xtask-delta-kept-store-{tag}-{pid}"));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let rec = ram_dir_fixture(&ws, &store);
        let _queued = queued.then(|| hold_land_lock(&ws));
        let built = rec.dir("c").join("upper/target/app.bin");
        std::fs::create_dir_all(built.parent().unwrap()).unwrap();
        std::fs::write(&built, "built").unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let Err(v) =
            admit_keeping(&ws, "c", &cfg, Class::Check, || Ok(0), true, None, probe).unwrap()
        else {
            panic!("{tag}: the check was admitted: no refusal to test");
        };
        assert!(want(&v), "{tag}: {v:?}");
        let msg = format!(
            "{:#}",
            fall_to_pool(&ws, "c", Class::Check, Some(&rec), v)
                .expect_err("a refused ram dir is refused, not dropped")
        );
        assert!(msg.contains("outputs and record intact"), "{tag}: {msg}");
        assert_eq!(std::fs::read_to_string(&built).unwrap(), "built", "{tag}");
        assert!(out::dir_of(&ws, "c").join(RECORD).is_file(), "{tag}");
        assert!(rec.dir("c").join("upper").is_dir(), "{tag}");
        assert_eq!(count_of(&ws, "delta-refused"), 1, "{tag}");
        assert_eq!(count_of(&ws, "delta-pool"), 0, "{tag}");
        std::fs::remove_dir_all(&ws).unwrap();
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Held inside the queue (the land hold of `land_hold_counts_while_land_is_active`: 10 GiB limit, 3 used; this process holds land's lock) is refused and keeps the dir: it cannot wait for the queue.
    #[test]
    fn held_ram_delta_keeps_its_outputs() {
        refusal_keeps_outputs(
            "held",
            true,
            || Ok(Ok(stats(10.0, 3.0, 1950))),
            |v| matches!(v, Verdict::Held(_)),
        );
    }

    /// Refuse (store RAM full by its own accounting: 8 GiB limit, 7.5 used) inside land's queue keeps the dir.
    #[test]
    fn refused_ram_delta_keeps_its_outputs() {
        refusal_keeps_outputs(
            "refused",
            true,
            || Ok(Ok(stats(8.0, 7.5, 1950))),
            |v| matches!(v, Verdict::Refuse(_)),
        );
    }

    /// NeverFits from a failed probe (store unavailable) keeps the dir too: a probe failure is no reason to remove a checkout's state.
    #[test]
    fn unprobed_store_keeps_its_ram_delta() {
        refusal_keeps_outputs(
            "unprobed",
            false,
            || Ok(Err("store not mounted".to_owned())),
            |v| matches!(v, Verdict::NeverFits(_)),
        );
    }

    /// A build with no ram dir yet takes the pool when admission refuses it: nothing to keep, counted `delta-pool`, nothing refused.
    #[test]
    fn new_ram_delta_falls_to_pool() {
        let pid = std::process::id();
        let ws = std::env::temp_dir().join(format!("xtask-delta-new-{pid}"));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let _queued = hold_land_lock(&ws);
        let Err(v) = admit(&ws, "c", &cfg, Class::Check, 0, || {
            Ok(Ok(stats(10.0, 3.0, 1950)))
        })
        .unwrap() else {
            panic!("the check was admitted: no land hold while land is queued, so no fall to test");
        };
        assert!(matches!(v, Verdict::Held(_)), "{v:?}");
        assert!(
            fall_to_pool(&ws, "c", Class::Check, None, v)
                .unwrap()
                .is_none()
        );
        assert_eq!(count_of(&ws, "delta-pool"), 1);
        assert_eq!(count_of(&ws, "delta-refused"), 0);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// The same Held verdict (inside the queue) while a command of this checkout still has the dir mounted (its mount lock held by a live process): the upper is live state, so nothing is dropped and the build stops, counted `delta-refused`.
    #[test]
    fn ram_delta_in_use_stops_the_build() {
        let pid = std::process::id();
        let ws = std::env::temp_dir().join(format!("xtask-delta-busy-{pid}"));
        let store = std::env::temp_dir().join(format!("xtask-delta-busy-store-{pid}"));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let rec = ram_dir_fixture(&ws, &store);
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let (mut holder, mut bystander) = lingering_holder(&ws, "c");
        let _queued = hold_land_lock(&ws);
        let verdict = admit_keeping(
            &ws,
            "c",
            &cfg,
            Class::Check,
            || Ok(0),
            true,
            None,
            || Ok(Ok(stats(10.0, 3.0, 1950))),
        )
        .unwrap();
        let got = match verdict {
            Err(v) => fall_to_pool(&ws, "c", Class::Check, Some(&rec), v),
            Ok(_) => panic!("the check was admitted: no hold, so no stop to test"),
        };
        let _ = holder.kill();
        let _ = bystander.kill();
        let _ = holder.wait();
        let _ = bystander.wait();
        let msg = format!("{:#}", got.expect_err("a mounted ram dir is not dropped"));
        assert!(msg.contains("in use by a running command"), "{msg}");
        assert!(out::dir_of(&ws, "c").exists());
        assert!(rec.dir("c").join("upper").is_dir());
        assert_eq!(count_of(&ws, "delta-refused"), 1);
        assert_eq!(count_of(&ws, "delta-pool"), 0);
        std::fs::remove_dir_all(&ws).unwrap();
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A Held existing ram dir waits for land's queue: another process holds `out/land.lock` until it is killed, and the build is admitted after that release. One wait counted; the hold counted once.
    #[test]
    fn held_ram_delta_waits_for_the_land_queue() {
        let ws = std::env::temp_dir().join(format!("xtask-delta-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let mut queue = sleeper_on(&land_lock_path(&ws), true);
        // released from another thread: a wait that never ends fails the assertions below instead of hanging the test
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = queue.kill();
            let _ = queue.wait();
        });
        let got = admit_keeping(
            &ws,
            "c",
            &cfg,
            Class::Check,
            || Ok(0),
            true,
            None,
            || Ok(Ok(stats(10.0, 3.0, 1950))),
        );
        release.join().unwrap();
        let (reservation, _, need) = match got.unwrap() {
            Ok(admitted) => admitted,
            Err(v) => panic!("waited for the queue, then refused: {v:?}"),
        };
        assert_eq!(need.ram, 2_642_949_252);
        assert_eq!(count_of(&ws, "delta-wait"), 1);
        assert_eq!(count_of(&ws, "delta-land-hold"), 1);
        drop(reservation);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Fresh store dir of the eviction tests (ram dirs live in it).
    fn fresh_store(tag: &str) -> PathBuf {
        let s = std::env::temp_dir().join(format!(
            "xtask-delta-evict-store-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&s);
        std::fs::create_dir_all(&s).unwrap();
        s
    }

    /// Ram dir `id` of workspace `ws` in `store` as a command leaves it: record in the mount point, upper, work, owner, and the `used` stamp (`raw` bytes) aged `idle_min` minutes.
    fn idle_ram_dir(ws: &Path, store: &Path, id: &str, raw: u64, idle_min: u64) -> Record {
        let rec = Record {
            base: "b".into(),
            store: store.to_owned(),
            parts: BTreeMap::new(),
        };
        let sd = rec.dir(id);
        std::fs::create_dir_all(sd.join("upper")).unwrap();
        std::fs::create_dir_all(sd.join("work")).unwrap();
        std::fs::write(sd.join(OWNER), format!("{}\n", ws.display())).unwrap();
        let stamp = sd.join(USED);
        std::fs::write(&stamp, format!("{raw}\n")).unwrap();
        let aged = std::time::SystemTime::now() - std::time::Duration::from_secs(idle_min * 60);
        std::fs::File::options()
            .write(true)
            .open(&stamp)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        let dir = out::dir_of(ws, id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RECORD), rec.text()).unwrap();
        rec
    }

    /// A queued waiter holds no mount lock, and its ticket names its checkout (`<class> <state> <id>`): the land queue's command in that checkout (land-seq's prune in `repo`) takes the lock while the waiter waits. Break (lock taken before the queue): red.
    #[test]
    fn queued_waiter_holds_no_mount_lock() {
        let ws = fresh_ws("no-mount-lock");
        let store = Store::used(8);
        let rx = waiter(&ws, "c", Class::Check, &store);
        queued(&ws, 1);
        let tickets = crate::ticket::live(&queue_dir(&ws)).unwrap();
        assert_eq!(
            tickets[0].1.split_whitespace().nth(2),
            Some("c"),
            "the ticket names no checkout: {:?}",
            tickets[0].1
        );
        let lock = try_mount_lock(&ws, "c").unwrap();
        assert!(
            lock.is_some(),
            "a queued waiter holds its checkout's mount lock"
        );
        drop(lock);
        store.set(0);
        rx.recv_timeout(ADMITTED)
            .expect("still waiting after the release")
            .expect("refused after the release");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// The land deadlock (ram-delta D3 Queue): a `repo` build waits behind land's hold while land's queue runs a command in `repo` that takes its mount lock. The queue waits for that command, so a queued waiter holding the lock would hold the queue. Break (lock taken before the queue): the take times out, red.
    #[test]
    fn queue_command_in_a_queued_checkout_is_not_blocked() {
        let ws = fresh_ws("land-deadlock");
        let mut queue = sleeper_on(&land_lock_path(&ws), true);
        // Held: land's native hold (9 GiB raw, 4.6 GiB RAM) does not fit beside a 3 GiB store
        let store = Store::used(3);
        let rx = waiter(&ws, "repo", Class::Check, &store);
        queued(&ws, 1);
        let (tx, took) = std::sync::mpsc::channel();
        let w = ws.clone();
        std::thread::spawn(move || {
            let lock = Flock::take(
                open_lock(&mount_lock_path(&w, "repo")).unwrap(),
                Mode::Exclusive,
            );
            let _ = tx.send(lock.is_ok());
        });
        assert!(
            took.recv_timeout(ADMITTED)
                .expect("the queue's command waits for a queued waiter's mount lock"),
            "the queue's command could not take the lock"
        );
        let _ = queue.kill();
        let _ = queue.wait();
        rx.recv_timeout(ADMITTED)
            .expect("the waiter stuck after the queue released")
            .expect("refused after the queue released");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Own credit is read under the admit gate at each judgement: a waiter judged after its upper is gone (evicted, or removed) needs the class peak, not the credit it queued with. Break (own read once, before the queue): the need keeps the old credit, red.
    #[test]
    fn queued_waiter_whose_upper_is_gone_needs_the_full_peak() {
        let ws = fresh_ws("own-gone");
        let upper = ws.join("upper-c");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("f"), vec![0u8; MIB as usize]).unwrap();
        let store = Store::used(8);
        let own_upper = upper.clone();
        let rx = waiter_on(&ws, "c", Class::Native, &store, None, move || {
            Ok(if own_upper.is_dir() { MIB } else { 0 })
        });
        queued(&ws, 1);
        std::fs::remove_dir_all(&upper).unwrap();
        store.set(0);
        let need = rx
            .recv_timeout(ADMITTED)
            .expect("still waiting after the release")
            .expect("refused after the release");
        assert_eq!(
            need.raw,
            9 * GIB,
            "the need kept the credit of a gone upper"
        );
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Two waiters: the head `h` judges, `w` queues behind it with an idle upper that would cover `h`'s shortfall alone; `x` is idle but covers it with no one's help (4 GiB RAM 2.05 < 4.6). Nothing is evicted: queued uppers are never victims. Break (queued uppers not excluded): `w` goes, red.
    #[test]
    fn queued_uppers_are_never_evicted() {
        let ws = fresh_ws("queued-kept");
        let store_dir = fresh_store("queued-kept");
        idle_ram_dir(&ws, &store_dir, "x", 4 * GIB, 1);
        let store = Store::used(9);
        let head = waiter_on(
            &ws,
            "h",
            Class::Native,
            &store,
            Some(store_dir.clone()),
            || Ok(0),
        );
        queued(&ws, 1);
        idle_ram_dir(&ws, &store_dir, "w", 16 * GIB, 60);
        let behind = waiter_on(
            &ws,
            "w",
            Class::Check,
            &store,
            Some(store_dir.clone()),
            || Ok(0),
        );
        queued(&ws, 2);
        assert!(
            head.recv_timeout(NOT_ADMITTED).is_err(),
            "the head was admitted on a full store"
        );
        assert!(
            store_dir.join("w").join("upper").is_dir(),
            "a queued waiter's upper was evicted"
        );
        assert!(
            store_dir.join("x").join("upper").is_dir(),
            "x evicted though it alone does not cover the shortfall"
        );
        assert_eq!(count_of(&ws, "delta-evict"), 0);
        store.set(0);
        head.recv_timeout(ADMITTED)
            .expect("head waiting")
            .expect("head refused");
        behind
            .recv_timeout(ADMITTED)
            .expect("behind waiting")
            .expect("behind refused");
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// Longest idle first, only until the shortfall is covered: native needs 4.6 GiB RAM beyond the 1 GiB free; p (30 min) and q (20 min) give 4.1 each, so both go, r (1 min) stays. The judgement credits the freed bytes and admits at once; one count per eviction; the evicted dir reads as lost, with its reason. Break (no eviction): red.
    #[test]
    fn short_judgement_evicts_longest_idle_until_covered() {
        let ws = fresh_ws("evict-order");
        let store_dir = fresh_store("evict-order");
        let p = idle_ram_dir(&ws, &store_dir, "p", 8 * GIB, 30);
        idle_ram_dir(&ws, &store_dir, "q", 8 * GIB, 20);
        idle_ram_dir(&ws, &store_dir, "r", 8 * GIB, 1);
        let store = Store::used(9);
        let rx = waiter_on(
            &ws,
            "a",
            Class::Native,
            &store,
            Some(store_dir.clone()),
            || Ok(0),
        );
        let need = rx
            .recv_timeout(ADMITTED)
            .expect("not admitted by the eviction")
            .expect("refused despite the eviction");
        assert_eq!(need.raw, 9 * GIB);
        assert!(
            !store_dir.join("p").exists(),
            "p (longest idle) not evicted"
        );
        assert!(
            !store_dir.join("q").exists(),
            "q not evicted: p alone covers 4.1 of 4.6 GiB"
        );
        assert!(
            store_dir.join("r").join("upper").is_dir(),
            "r evicted past the cover"
        );
        assert_eq!(count_of(&ws, "delta-evict"), 2);
        let leftover = std::fs::read_dir(&store_dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".evict-"));
        assert!(!leftover, "a rename left behind");
        let why = lost(&ws, "p", &p)
            .unwrap()
            .expect("an evicted dir reads as lost");
        assert!(why.starts_with("evicted: for ws/a native build"), "{why}");
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// A shortfall the idle total does not cover waits as before: p and q (2.05 GiB RAM each, 4.1 < 4.6) stay. Break (evicts anyway): red.
    #[test]
    fn uncovered_shortfall_evicts_nothing() {
        let ws = fresh_ws("no-cover");
        let store_dir = fresh_store("no-cover");
        idle_ram_dir(&ws, &store_dir, "p", 4 * GIB, 30);
        idle_ram_dir(&ws, &store_dir, "q", 4 * GIB, 20);
        let store = Store::used(9);
        let rx = waiter_on(
            &ws,
            "a",
            Class::Native,
            &store,
            Some(store_dir.clone()),
            || Ok(0),
        );
        assert!(
            rx.recv_timeout(NOT_ADMITTED).is_err(),
            "admitted on an uncovered shortfall"
        );
        assert!(
            store_dir.join("p").join("upper").is_dir()
                && store_dir.join("q").join("upper").is_dir(),
            "an uncovered judgement evicted"
        );
        assert_eq!(count_of(&ws, "delta-evict"), 0);
        store.set(0);
        rx.recv_timeout(ADMITTED)
            .expect("still waiting after the release")
            .expect("refused after the release");
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// Eviction skips what runs or is about to: `run` (mount lock held), `res` (a live reservation: admitted, not yet mounted), `foreign` (owned by another workspace). Each is the longest idle, so a sweep ignoring the rules takes them first; `v` (1 min, 12 GiB) is the one eligible dir and alone covers the 5.6 GiB shortfall. Break (no exclusion): red.
    #[test]
    fn eviction_skips_running_reserved_and_foreign() {
        let ws = fresh_ws("exclusions");
        let store_dir = fresh_store("exclusions");
        idle_ram_dir(&ws, &store_dir, "run", 20 * GIB, 60);
        idle_ram_dir(&ws, &store_dir, "res", 20 * GIB, 60);
        idle_ram_dir(&ws, &store_dir, "foreign", 20 * GIB, 60);
        std::fs::write(store_dir.join("foreign").join(OWNER), "/elsewhere\n").unwrap();
        idle_ram_dir(&ws, &store_dir, "v", 12 * GIB, 1);
        let _running = try_mount_lock(&ws, "run")
            .unwrap()
            .expect("fixture: run's lock");
        let _reserved = reserve(&ws, "res", GIB).unwrap();
        let store = Store::used(9);
        let rx = waiter_on(
            &ws,
            "a",
            Class::Native,
            &store,
            Some(store_dir.clone()),
            || Ok(0),
        );
        rx.recv_timeout(ADMITTED)
            .expect("not admitted")
            .expect("refused");
        assert!(
            !store_dir.join("v").exists(),
            "v (the eligible dir) not evicted"
        );
        assert!(
            store_dir.join("run").join("upper").is_dir(),
            "a running checkout evicted"
        );
        assert!(
            store_dir.join("res").join("upper").is_dir(),
            "a reserved checkout evicted"
        );
        assert!(
            store_dir.join("foreign").join("upper").is_dir(),
            "another workspace's dir evicted"
        );
        assert_eq!(count_of(&ws, "delta-evict"), 1);
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// The wait line names a queued checkout as `queued` (its ticket), not `idle`: a queued waiter holds no lock, so the lock alone misreads it. Break (lock state only): red.
    #[test]
    fn held_by_names_queued_checkouts() {
        let ws = fresh_ws("held-by-queued");
        let store_dir = fresh_store("held-by-queued");
        idle_ram_dir(&ws, &store_dir, "q", 2 * GIB, 5);
        idle_ram_dir(&ws, &store_dir, "p", 3 * GIB, 5);
        std::fs::create_dir_all(queue_dir(&ws)).unwrap();
        let _ticket =
            crate::ticket::Ticket::new(&queue_dir(&ws), "", std::process::id(), "check refuse q")
                .unwrap();
        let text = held_by(&ws, &store_dir, "me");
        assert!(text.contains("q 0.0 GiB queued"), "{text}");
        assert!(text.contains("p 0.0 GiB idle"), "{text}");
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// The own credit is re-walked only when the upper changed: absent is 0; an unchanged `used` stamp serves the cached credit (no walk per poll, the 1 MiB file added without a stamp change is not seen); a rewritten stamp re-walks and sees it. Break (walk every judgement): red.
    #[test]
    fn own_credit_is_read_when_the_upper_changes() {
        let root =
            std::env::temp_dir().join(format!("xtask-delta-own-credit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sd = root.join("c");
        let upper = sd.join("upper");
        let mut seen = None;
        let own = |seen: &mut Option<(String, u64)>| {
            own_at(&sd, &upper, Some(Class::Native), None, &[], seen).unwrap()
        };
        assert_eq!(own(&mut seen), 0, "an absent upper");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(sd.join(USED), "0\n").unwrap();
        std::fs::write(upper.join("a"), vec![0u8; MIB as usize]).unwrap();
        let first = own(&mut seen);
        assert!(first >= MIB, "{first}");
        std::fs::write(upper.join("b"), vec![0u8; 2 * MIB as usize]).unwrap();
        assert_eq!(own(&mut seen), first, "re-walked without a stamp change");
        std::fs::write(sd.join(USED), "1\n").unwrap();
        let after = own(&mut seen);
        assert!(
            after > first + MIB,
            "a changed stamp did not re-walk: {after} vs {first}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A leftover `<store>/.evict-<id>-<pid>` of a crashed eviction is this workspace's orphan store dir, so out prune removes it.
    #[test]
    fn evict_leftover_is_an_orphan_store_dir() {
        let ws = fresh_ws("leftover");
        let store_dir = fresh_store("leftover");
        let left = store_dir.join(".evict-p-1");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::write(left.join(OWNER), format!("{}\n", ws.display())).unwrap();
        let orphans = orphan_store_dirs(&store_dir, &ws, &std::collections::BTreeSet::new());
        assert_eq!(
            orphans
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            [".evict-p-1"]
        );
        std::fs::remove_dir_all(&ws).unwrap();
        std::fs::remove_dir_all(&store_dir).unwrap();
    }

    /// Store whose RAM use a test changes while waiters poll it: limit 10 GiB, ratio 1.95, fs 14 GiB free. Native needs 4.615 GiB RAM / 9 raw, check 2.461 / 4.8, small 1.026 / 2 (default peaks, own 0); margin 1.
    #[derive(Clone)]
    struct Store(std::sync::Arc<std::sync::atomic::AtomicU64>);

    impl Store {
        fn used(gib: u64) -> Store {
            Store(std::sync::Arc::new(gib.into()))
        }
        fn set(&self, gib: u64) {
            self.0.store(gib, std::sync::atomic::Ordering::SeqCst);
        }
        fn stats(&self) -> Stats {
            stats(
                10.0,
                self.0.load(std::sync::atomic::Ordering::SeqCst) as f64,
                1950,
            )
        }
    }

    /// A waiter thread: `admit_keeping` of an existing ram dir `id` on `store`, no eviction store, own credit 0; the result goes to the receiver, the reservation dropped.
    fn waiter(
        ws: &Path,
        id: &'static str,
        class: Class,
        store: &Store,
    ) -> std::sync::mpsc::Receiver<std::result::Result<Need, String>> {
        waiter_on(ws, id, class, store, None, || Ok(0))
    }

    /// [`waiter`] with eviction on (store dir `dir`) and the own credit `own`, read at each judgement.
    fn waiter_on(
        ws: &Path,
        id: &'static str,
        class: Class,
        store: &Store,
        dir: Option<PathBuf>,
        own: impl FnMut() -> Result<u64> + Send + 'static,
    ) -> std::sync::mpsc::Receiver<std::result::Result<Need, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ws, store) = (ws.to_owned(), store.clone());
        std::thread::spawn(move || {
            let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
                .unwrap()
                .delta;
            let got = admit_keeping(&ws, id, &cfg, class, own, true, dir.as_deref(), || {
                Ok(Ok(store.stats()))
            })
            .unwrap();
            let _ = tx.send(got.map(|(_, _, need)| need).map_err(|v| format!("{v:?}")));
        });
        rx
    }

    /// Blocks until the store queue holds `n` live tickets (bounded: a waiter that never queues fails here).
    fn queued(ws: &Path, n: usize) {
        let t0 = std::time::Instant::now();
        while crate::ticket::live(&queue_dir(ws)).map_or(0, |t| t.len()) != n {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(20),
                "never {n} queued"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn fresh_ws(tag: &str) -> PathBuf {
        let ws = std::env::temp_dir().join(format!("xtask-delta-q-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        ws
    }

    /// Longer than two polls: a waiter that would be admitted is admitted within it.
    const NOT_ADMITTED: std::time::Duration = std::time::Duration::from_millis(2500);
    const ADMITTED: std::time::Duration = std::time::Duration::from_secs(20);

    /// S2: an existing ram dir refused for capacity (used 8 of 10 GiB, check needs 2.461 + margin 1) waits in the queue instead of exiting, and is admitted once the probe shows space. Nothing refused.
    #[test]
    fn refused_for_capacity_waits_until_the_store_frees() {
        let ws = fresh_ws("refuse-waits");
        let store = Store::used(8);
        let rx = waiter(&ws, "c", Class::Check, &store);
        queued(&ws, 1);
        assert!(
            rx.recv_timeout(NOT_ADMITTED).is_err(),
            "admitted on a full store"
        );
        store.set(0);
        let need = rx
            .recv_timeout(ADMITTED)
            .expect("still waiting")
            .expect("refused");
        assert_eq!(need.ram, 2_642_949_252);
        assert_eq!(count_of(&ws, "delta-wait"), 1);
        assert_eq!(count_of(&ws, "delta-refused"), 0);
        queued(&ws, 0);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// FIFO: native `a` arrives first and is refused for capacity (used 5: 4.615 + 1 > 5); check `b` behind it would fit (2.461 + 1 <= 5) but waits for the head. Freed (used 0): `a` first, then `b` (2.461 + 4.615 reserved + 1 <= 10).
    #[test]
    fn waiters_are_admitted_in_arrival_order() {
        let ws = fresh_ws("fifo");
        let store = Store::used(5);
        let a = waiter(&ws, "a", Class::Native, &store);
        queued(&ws, 1);
        let b = waiter(&ws, "b", Class::Check, &store);
        queued(&ws, 2);
        assert!(b.recv_timeout(NOT_ADMITTED).is_err(), "b passed the head");
        store.set(0);
        a.recv_timeout(ADMITTED)
            .expect("a waiting")
            .expect("a refused");
        b.recv_timeout(ADMITTED)
            .expect("b waiting")
            .expect("b refused");
        assert_eq!(count_of(&ws, "delta-bypass"), 0);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A small build passes a head refused for capacity (native at used 5; small 1.026 + 1 <= 5), counted `delta-bypass`.
    #[test]
    fn small_passes_a_head_refused_for_capacity() {
        let ws = fresh_ws("bypass-refuse");
        let store = Store::used(5);
        let head = waiter(&ws, "a", Class::Native, &store);
        queued(&ws, 1);
        let small = waiter(&ws, "s", Class::Small, &store);
        small
            .recv_timeout(ADMITTED)
            .expect("small waits behind a capacity head")
            .expect("small refused");
        assert_eq!(count_of(&ws, "delta-bypass"), 1);
        store.set(0);
        head.recv_timeout(ADMITTED)
            .expect("head waiting")
            .expect("head refused");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A small build never passes a head blocked by reservations: a live 5 GiB reservation of `z` makes native wait (fs: 9 + 9.75 raw reserved > 14), while small alone would be admitted (2 + 9.75 <= 14; 1.026 + 5 + 1 <= 10).
    #[test]
    fn small_does_not_pass_a_head_waiting_on_reservations() {
        let ws = fresh_ws("bypass-wait");
        let store = Store::used(0);
        let z = reserve(&ws, "z", 5 * GIB).unwrap();
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        let s = store.stats();
        assert!(matches!(
            judge_quiet(&ws, "a", &cfg, Class::Native, 0, &s).unwrap(),
            Verdict::Wait(_)
        ));
        assert!(matches!(
            judge_quiet(&ws, "s", &cfg, Class::Small, 0, &s).unwrap(),
            Verdict::Admit { .. }
        ));
        let head = waiter(&ws, "a", Class::Native, &store);
        queued(&ws, 1);
        let small = waiter(&ws, "s", Class::Small, &store);
        assert!(
            small.recv_timeout(NOT_ADMITTED).is_err(),
            "small passed a reservation head"
        );
        drop(z);
        head.recv_timeout(ADMITTED)
            .expect("head waiting")
            .expect("head refused");
        small
            .recv_timeout(ADMITTED)
            .expect("small waiting")
            .expect("small refused");
        assert_eq!(count_of(&ws, "delta-bypass"), 0);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// A waiter killed with SIGKILL leaves its ticket locked by nobody: the next waiter skips it. The ticket (earliest arrival, a native refused) is held by a child process, so `b` queues behind it until the child is killed.
    #[test]
    fn killed_waiters_ticket_is_skipped() {
        let ws = fresh_ws("killed");
        let q = queue_dir(&ws);
        std::fs::create_dir_all(&q).unwrap();
        let t = q.join(format!("{:020}-{:010}-{:010}", 1, 1, 0));
        std::fs::write(&t, "native refuse").unwrap();
        let mut dead = sleeper_on(&t, true);
        let store = Store::used(0);
        let b = waiter(&ws, "b", Class::Check, &store);
        queued(&ws, 2);
        assert!(
            b.recv_timeout(NOT_ADMITTED).is_err(),
            "b passed a live head"
        );
        dead.kill().unwrap();
        let _ = dead.wait();
        b.recv_timeout(ADMITTED)
            .expect("b behind a dead ticket")
            .expect("b refused");
        assert!(!t.exists(), "dead ticket not removed");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// No deadlock: a build waiting on land's queue holds neither the admit gate nor a reservation, so land's own admission goes through while it waits, and the build proceeds once the queue releases.
    #[test]
    fn waiting_build_does_not_block_the_land() {
        let ws =
            std::env::temp_dir().join(format!("xtask-delta-no-deadlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(out::pool(&ws)).unwrap();
        let mut queue = sleeper_on(&land_lock_path(&ws), true);
        let worker_ws = ws.clone();
        let worker = std::thread::spawn(move || {
            let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
                .unwrap()
                .delta;
            admit_keeping(
                &worker_ws,
                "c",
                &cfg,
                Class::Check,
                || Ok(0),
                true,
                None,
                || Ok(Ok(stats(10.0, 3.0, 1950))),
            )
        });
        let started = std::time::Instant::now();
        while count_of(&ws, "delta-wait") == 0 {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "the build never started waiting"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // land's admission, bounded: it would block forever if the waiting build held the admit gate
        let land_ws = ws.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
                .unwrap()
                .delta;
            let _ = tx.send(admit(
                &land_ws,
                out::LAND_ID,
                &cfg,
                Class::Native,
                0,
                || Ok(Ok(stats(10.0, 3.0, 1950))),
            ));
        });
        let land = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("land admitted while a build waits on the queue")
            .unwrap();
        assert!(land.is_ok(), "land refused: {:?}", land.as_ref().err());
        // land's build ends: its reservation goes, then the queue releases
        drop(land);
        let _ = queue.kill();
        let _ = queue.wait();
        let built = worker.join().unwrap().unwrap();
        assert!(
            built.is_ok(),
            "the waiting build refused after the release: {:?}",
            built.as_ref().err()
        );
        assert_eq!(
            count_of(&ws, "delta-land-hold"),
            1,
            "the hold counted once; land is never held"
        );
        drop(built);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// `delta.ram_store` has no default: unset is no RAM tier, with the reason the probe names; a file's value is the store.
    #[test]
    fn no_ram_store_by_default() {
        let d = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        assert_eq!(d.ram_store.value, None);
        assert_eq!(d.ram_store.origin, crate::ws_config::Origin::Default);
        assert_eq!(probe_store(None, &d).unwrap(), Err(NO_STORE.to_owned()));
        let set = crate::ws_config::parse(
            "[delta]\nram_store = \"/z\"\n",
            Path::new("/w/workspace.toml"),
        )
        .unwrap()
        .delta;
        assert_eq!(set.ram_store.value.as_deref(), Some(Path::new("/z")));
        assert_eq!(set.ram_store.origin, crate::ws_config::Origin::File);
    }

    /// Probe: a dir on a disk filesystem is not RAM-backed; /dev/shm (tmpfs on Linux) is, with ratio 1.
    #[test]
    fn probe_tells_ram_from_disk() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        assert!(
            probe(Path::new("/nonexistent-store"), &cfg)
                .unwrap()
                .is_err()
        );
        let tmp = std::env::temp_dir();
        if rustix::fs::statfs(&tmp).unwrap().f_type as i64 != TMPFS_MAGIC {
            assert!(probe(&tmp, &cfg).unwrap().is_err());
        }
        if Path::new("/dev/shm").is_dir() {
            let s = probe(Path::new("/dev/shm"), &cfg).unwrap().unwrap();
            assert_eq!((s.backing, s.ratio_milli), (Backing::Tmpfs, 1000));
        }
    }

    /// `pins`: a missing upper or work is no claim, on a usable store only; otherwise the pin stands whatever the dirs hold.
    #[test]
    fn pins_follow_the_upper_only_on_a_usable_store() {
        let pid = std::process::id();
        let store = std::env::temp_dir().join(format!("xtask-delta-pins-{pid}"));
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();
        let rec = Record {
            base: "b".into(),
            store: store.clone(),
            parts: BTreeMap::new(),
        };
        assert!(!pins("c", &rec, true), "no upper: no claim");
        assert!(pins("c", &rec, false), "unusable store: the pin stands");
        std::fs::create_dir_all(rec.dir("c").join("upper")).unwrap();
        assert!(!pins("c", &rec, true), "upper without work: no claim");
        std::fs::create_dir_all(rec.dir("c").join("work")).unwrap();
        assert!(pins("c", &rec, true), "upper and work: pinned");
        assert_eq!(lost_upper("c", &rec), None);
        std::fs::remove_dir_all(&store).unwrap();
    }

    /// A scratch workspace dir named `name` for this process, emptied first.
    fn scratch_ws(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = remove_tree(&d);
        d
    }

    /// Runs git in `dir` with a test identity; its trimmed stdout.
    fn git_t(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Sets `path`'s mtime to unix time `epoch`.
    fn touch_at(path: &Path, epoch: &str) {
        let ok = std::process::Command::new("touch")
            .args(["-d", &format!("@{epoch}")])
            .arg(path)
            .status()
            .expect("touch")
            .success();
        assert!(ok, "touch {}", path.display());
    }

    /// A complete part `commit` of `arch` under `ws`: `<arch>/.part`, the subtree's mtime `epoch` (publish order, out.rs parts_of).
    fn make_part(ws: &Path, arch: &str, commit: &str, epoch: &str) {
        let sub = out::part_root(ws, arch).join(commit).join(arch);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".part"), format!("commit={commit}\narch={arch}\n")).unwrap();
        touch_at(&sub, epoch);
    }

    /// A repo at `root`, `main` = c1 (a.txt "1", b.txt "b") -> c2 (a.txt "2"): returns (c1, c2).
    fn two_commit_repo(root: &Path) -> (String, String) {
        std::fs::create_dir_all(root).unwrap();
        git_t(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "1").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        git_t(root, &["add", "."]);
        git_t(root, &["commit", "-q", "-m", "c1"]);
        let c1 = git_t(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("a.txt"), "2").unwrap();
        git_t(root, &["commit", "-q", "-am", "c2"]);
        (c1, git_t(root, &["rev-parse", "HEAD"]))
    }

    /// `pick_part`: the newest complete part that is an ancestor of HEAD. A newer part on a sibling branch is not picked.
    /// Break: pick the newest complete part regardless of ancestry -> the side branch's s1 -> red.
    #[test]
    fn pick_is_the_newest_ancestor_part() {
        let ws = scratch_ws("xtask-delta-pick");
        let root = ws.join("repo");
        let (c1, _c2) = two_commit_repo(&root);
        git_t(&root, &["checkout", "-q", "-b", "side", &c1]);
        std::fs::write(root.join("a.txt"), "s").unwrap();
        git_t(&root, &["commit", "-q", "-am", "s1"]);
        let s1 = git_t(&root, &["rev-parse", "HEAD"]);
        git_t(&root, &["checkout", "-q", "main"]);
        make_part(&ws, "riscv64", &c1, "1000000000");
        make_part(&ws, "riscv64", &s1, "2000000000");
        assert_eq!(
            out::pick_part(&root, &ws, "riscv64").unwrap(),
            Some(c1.clone())
        );
        assert_eq!(out::pick_part(&root, &ws, "x86_64").unwrap(), None);
        remove_tree(&ws).unwrap();
    }

    /// `lay_part` refuses an upper that holds the arch, as a dir or a link (a whiteout is one of these too): a part under it would be shadowed.
    /// Break: drop the `upper_host_exists` check -> the part is recorded -> red.
    #[test]
    fn lay_refuses_an_upper_that_holds_the_arch() {
        let ws = scratch_ws("xtask-delta-lay-refuse");
        let root = ws.join("repo");
        let (c1, _) = two_commit_repo(&root);
        make_part(&ws, "riscv64", &c1, "1000000000");
        std::fs::create_dir_all(lock_dir(&ws)).unwrap();
        std::fs::create_dir_all(out::dir_of(&ws, "k")).unwrap();
        let mut rec = Record {
            base: "b1".into(),
            store: ws.join("store"),
            parts: BTreeMap::new(),
        };
        let sd = rec.dir("k");
        std::fs::create_dir_all(sd.join("upper/riscv64")).unwrap();
        std::fs::create_dir_all(sd.join("work")).unwrap();
        assert_eq!(
            lay_part(&root, &ws, "k", &mut rec, "riscv64").unwrap(),
            None
        );
        assert!(rec.parts.is_empty(), "an arch dir in the upper: no part");
        std::fs::remove_dir_all(sd.join("upper/riscv64")).unwrap();
        std::os::unix::fs::symlink("nowhere", sd.join("upper/riscv64")).unwrap();
        assert_eq!(
            lay_part(&root, &ws, "k", &mut rec, "riscv64").unwrap(),
            None
        );
        assert!(rec.parts.is_empty(), "a link there: no part");
        remove_tree(&ws).unwrap();
    }

    /// `lay_part` records the newest ancestor part and touches the paths that differ from it; the record is written after the touch.
    /// Break: skip the touch -> a.txt keeps its 2001 mtime -> red. b.txt (equal in part and HEAD) keeps 2001 either way.
    #[test]
    fn lay_records_the_part_and_touches_what_differs() {
        let ws = scratch_ws("xtask-delta-lay-touch");
        let root = ws.join("repo");
        let (c1, c2) = two_commit_repo(&root);
        make_part(&ws, "riscv64", &c1, "1000000000");
        std::fs::create_dir_all(lock_dir(&ws)).unwrap();
        for f in ["a.txt", "b.txt"] {
            touch_at(&root.join(f), "981154800");
        }
        let dir = out::dir_of(&ws, "k");
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = Record {
            base: c2.clone(),
            store: ws.join("store"),
            parts: BTreeMap::new(),
        };
        std::fs::create_dir_all(rec.dir("k").join("upper")).unwrap();
        std::fs::create_dir_all(rec.dir("k").join("work")).unwrap();
        let line = lay_part(&root, &ws, "k", &mut rec, "riscv64").unwrap();
        assert_eq!(line, Some(format!("out: ws/k part riscv64@{c1} laid")));
        assert_eq!(rec.parts.get("riscv64"), Some(&c1));
        assert_eq!(record(&dir).unwrap().unwrap().parts, rec.parts);
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(981_154_800);
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        assert!(mtime(&root.join("a.txt")) > old, "a.txt differs: touched");
        assert_eq!(mtime(&root.join("b.txt")), old, "b.txt equal: kept");
        remove_tree(&ws).unwrap();
    }

    /// The overlay [`mount_options`] makes for a laid part, mounted as stage mount does (uid 0 in a user + mount namespace): base and part merge; a delete under the arch dir whites out the part's file; a fresh upper without the arch dir shows the part again.
    /// Break: leave the part out of lowerdir -> `riscv64/rv.o` is missing -> red.
    #[test]
    fn laid_part_merges_under_the_base_and_whiteouts_hide_it() {
        let ws = scratch_ws("xtask-delta-part-mount");
        let base = out::base_of(&ws, "b1");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("base.txt"), "x86").unwrap();
        let part = out::part_root(&ws, "riscv64").join("p1");
        std::fs::create_dir_all(part.join("riscv64")).unwrap();
        std::fs::write(part.join("riscv64/rv.o"), "rv").unwrap();
        std::fs::write(part.join(".part"), "commit=p1\narch=riscv64\n").unwrap();
        let parts = BTreeMap::from([("riscv64".to_owned(), "p1".to_owned())]);
        let store = ws.join("store");
        let merged = out::dir_of(&ws, "k");
        std::fs::create_dir_all(&merged).unwrap();
        let opts = |id: &str| {
            let rec = Record {
                base: "b1".into(),
                store: store.clone(),
                parts: parts.clone(),
            };
            std::fs::create_dir_all(rec.dir(id).join("upper")).unwrap();
            std::fs::create_dir_all(rec.dir(id).join("work")).unwrap();
            mount_options(&ws, id, &rec).unwrap()
        };
        let (o1, o2) = (opts("k"), opts("k2"));
        let m = merged.display();
        let script = format!(
            "set -e\n\
             mount -t overlay overlay -o '{o1}' '{m}'\n\
             test -f '{m}/base.txt'\n\
             test -f '{m}/riscv64/rv.o'\n\
             rm '{m}/riscv64/rv.o'\n\
             test ! -e '{m}/riscv64/rv.o'\n\
             umount '{m}'\n\
             mount -t overlay overlay -o '{o2}' '{m}'\n\
             test -f '{m}/riscv64/rv.o'\n\
             umount '{m}'\n\
             echo merged-ok\n"
        );
        let out = std::process::Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
                "--",
                "sh",
                "-c",
                &script,
            ])
            .output()
            .expect("unshare");
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("merged-ok"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        remove_tree(&ws).unwrap();
    }

    /// Part lines: 0, 1 and 2 parts round-trip; text without part lines (an older xtask's record) parses to none; a line without `@` is refused.
    #[test]
    fn record_parts_round_trip() {
        let old = Record::parse("base=abc\nstore=/s\n").unwrap();
        assert!(old.parts.is_empty());
        let one = Record {
            parts: BTreeMap::from([("riscv64".to_owned(), "c2".to_owned())]),
            ..old.clone()
        };
        assert_eq!(one.text(), "base=abc\nstore=/s\npart=riscv64@c2\n");
        assert_eq!(Record::parse(&one.text()).unwrap(), one);
        let two = Record {
            parts: BTreeMap::from([
                ("arm64".to_owned(), "c3".to_owned()),
                ("riscv64".to_owned(), "c2".to_owned()),
            ]),
            ..old
        };
        assert_eq!(Record::parse(&two.text()).unwrap(), two);
        assert!(Record::parse("base=abc\nstore=/s\npart=riscv64\n").is_err());
    }

    #[test]
    fn record_round_trip_and_keys() {
        let r = Record {
            base: "abc".into(),
            store: "/s".into(),
            parts: BTreeMap::new(),
        };
        assert_eq!(Record::parse(&r.text()).unwrap(), r);
        assert_eq!(r.text(), "base=abc\nstore=/s\n");
        assert_eq!(r.dir("x"), Path::new("/s/x"));
        assert_eq!(Class::from_key("peak_check_gib"), Some(Class::Check));
        assert_eq!(Class::from_key("peak_bogus_gib"), None);
        let cfg = crate::ws_config::parse(
            "[delta]\npeak_check_gib = 4\nratio_default = 3.5\nram_store = \"/z\"\n",
            Path::new("/w/workspace.toml"),
        )
        .unwrap()
        .delta;
        assert_eq!(Class::Check.peak(&cfg), 4 * GIB);
        assert_eq!(cfg.ratio_milli.value, 3500);
        assert_eq!(cfg.ram_store.value.as_deref(), Some(Path::new("/z")));
        // defaults: D3 derivations (ws_config DEFAULT_PEAK_MIB)
        let d = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .unwrap()
            .delta;
        // cross 12 GiB: measured riscv64 upper 10.32 GiB raw + 16 % (manager 2026-11-25)
        assert_eq!(Class::Cross.peak(&d), 12 * GIB);
        // run default = the larger measured WeChat clean install + launch window (ws_config DEFAULT_PEAK_MIB[4]), rounded up
        assert_eq!(
            Class::Run.peak(&d),
            796.5_f64.max(796.6).ceil() as u64 * MIB
        );
    }
}
