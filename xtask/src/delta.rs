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

use std::collections::BTreeMap;
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

/// The class of a build command (cli.rs).
pub fn class_for_native(root: &Path, host: &str) -> Result<Class> {
    let arches = crate::arches::load(root)?;
    let (build, _) = crate::arches::build_machine(root, &arches)?;
    Ok(if host == build {
        Class::Native
    } else {
        Class::Cross
    })
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
    let (others, land) = land_now(ws)?;
    let hold = hold_for(id, cfg, stats, land);
    Ok(judge(
        stats,
        Need::of(class, cfg, stats, own),
        others,
        hold,
        cfg.ram_margin_gib.value * GIB,
    ))
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
    admit_keeping(ws, id, cfg, class, own, false, None, probe_now)
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

/// Admits a build of `class` on store stats from `probe_now`, its upper holding `own` bytes of its part (fixed across the wait: the upper changes only under this checkout's own commands). Ok(Ok(reservation)), or Ok(Err(verdict)) for a verdict that does not wait.
/// Waiting: one FIFO [`crate::ticket`] per waiter in [`queue_dir`], judged only at the head ([`may_bypass`] excepted, counter `delta-bypass`). Wait always queues; Refuse and Held queue only for an existing ram dir (`keep`; a new dir takes the pool). NeverFits never waits.
/// A process inside land's queue ([`inside_land_queue`]) and the land checkout never join the FIFO: the land queue waits for them, and a FIFO head waiting on land's hold would wait for them in turn. They poll a Wait (others' reservations end without them); inside the queue Refuse and Held return at once.
/// Nothing is held while waiting (admit gate released between polls, no reservation, no mount lock), so land's own admission proceeds. No timeout: an endless wait shows in `atl-job status`. A line per state change (`out: delta ws/<id>: waiting ...`, naming the store's holders from `store`), counter `delta-wait` once per wait.
#[allow(clippy::too_many_arguments)]
fn admit_keeping(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    own: u64,
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
            let v = judge_quiet(ws, id, cfg, class, own, &stats)?;
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
                    return Ok(Ok((r, stats, need)));
                }
                Verdict::NeverFits(_) => return Ok(Err(v)),
                Verdict::Refuse(_) | Verdict::Held(_) if inside || !keep => return Ok(Err(v)),
                _ => (state_of(&v), v.reason()),
            }
        } else {
            ("queued", format!("{ahead} ahead in the store queue"))
        };
        if !unqueued {
            let payload = format!("{} {state}", class.name());
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

/// The store's deltas other than `me`, largest first: `<id> <GiB> GiB running|idle <min> min` (running = its mount lock is held; idle since its `used` stamp, [`USED`]). What the PM acts on while a build waits.
fn held_by(ws: &Path, store: &Path, me: &str) -> String {
    let Ok(rd) = std::fs::read_dir(store) else {
        return format!("{} unreadable", store.display());
    };
    let mut rows: Vec<(u64, String)> = Vec::new();
    for e in rd.flatten() {
        let Some(id) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let upper = e.path().join("upper");
        if id == me || !upper.is_dir() {
            continue;
        }
        // whole upper, every host dir: what the delta holds on the store
        let size = own_bytes(&upper, Class::Small, None, &[]).unwrap_or(0);
        let running = try_mount_lock(ws, &id).ok().flatten().is_none();
        let state = if running {
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
/// Cross = `upper/<cross_host>` only. Other build classes = all of `upper` but the cross host dirs `cross_dirs`: a cross host's outputs are not credited across arches (rhs2-rv-20: 14.4 GiB of x86 upper, its first riscv64 build still grew 10.32).
/// Run dirs grow from an empty root: 0. A cross command without a host dir (`cross`: cargo writes `target/<triple>`): 0, so its growth is reserved whole and not sampled.
pub fn own_bytes(
    upper: &Path,
    class: Class,
    cross_host: Option<&str>,
    cross_dirs: &[String],
) -> Result<u64> {
    let (top, skip): (PathBuf, Vec<PathBuf>) = match (class, cross_host) {
        (Class::Run, _) | (Class::Cross, None) => return Ok(0),
        (Class::Cross, Some(host)) => (upper.join(host), Vec::new()),
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

/// The outer side of every build command: decides the tier, admits, and for `ram` runs the rest of the command inside the mounted namespace. Some(code) = ran there; None = continue here (pool tier, or already inside).
/// `cross_host`: the host a cross build writes (`upper/<host>`, [`own_bytes`]); None for every other command.
pub fn enter(root: &Path, class: Class, cross_host: Option<&str>) -> Result<Option<ExitCode>> {
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
    let own = if rec.is_some() {
        own_bytes(&upper, class, cross_host, &dirs)?
    } else {
        0
    };
    let admitted = admit_keeping(
        &ws,
        &id,
        &cfg.delta,
        class,
        own,
        rec.is_some(),
        Some(&store),
        || probe(&store, &cfg.delta),
    )?;
    let (reservation, stats, need) = match admitted {
        Ok(a) => a,
        Err(Verdict::Admit { .. }) => bail!("delta admission returned Admit as a refusal"),
        Err(v) => return fall_to_pool(&ws, &id, class, rec.as_ref(), v),
    };
    match rec {
        Some(r) => {
            if let Some(why) = lost(&ws, &id, &r)? {
                out::count(&ws, "delta-lost");
                println!("out: ws/{id} ram delta lost ({why}): remade");
                drop_ram(&ws, &id, &r)?;
                make_ram(root, &ws, &id, &store, true)?;
            }
        }
        None => {
            if let Err(e) = make_ram(root, &ws, &id, &store, true) {
                return pool(&format!("{e:#}"));
            }
        }
    }
    println!(
        "out: delta ram ({} build, {:.2} GiB RAM reserved (own {:.2} GiB counted); store {}: {})",
        class.name(),
        gib(need.ram),
        gib(own),
        store.display(),
        stats.line()
    );
    out::count(&ws, "delta-ram");
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
    // the growth sample: own part of the upper before and after, the reservation still held
    let before = own_bytes(&upper, class, cross_host, &dirs)?;
    let code = run(cmd, "unshare (delta mount namespace)")?;
    // the namespace's processes may still use the overlay (a daemon a build spawned: sccache server, gradle, ...); the mount ends with the command, so they do too
    end_holders(&ws, &id, &lock_path, std::process::id())?;
    let after = own_bytes(&upper, class, cross_host, &dirs)?;
    // idle-since stamp for other checkouts' wait lines; a lost stamp only shows the upper's mtime
    let _ = std::fs::write(store.join(&id).join(USED), "");
    let (line, over) = growth(&id, before, after, need.raw);
    println!("{line}");
    if over {
        out::count(&ws, "delta-over");
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

/// Why ram dir `rec` cannot be mounted (its upper or base is gone, e.g. a WSL restart emptied the store); None = intact.
fn lost(ws: &Path, id: &str, rec: &Record) -> Result<Option<String>> {
    if let Some(why) = lost_upper(id, rec) {
        return Ok(Some(why));
    }
    if !out::base_of(ws, &rec.base).is_dir() {
        return Ok(Some(format!("base/{} is gone", rec.base)));
    }
    Ok(None)
}

/// The overlay mount options of ram dir `rec` of checkout `id`. Overlay splits options at `,` and lowerdir at `:`, which a path may not hold.
pub fn mount_options(ws: &Path, id: &str, rec: &Record) -> Result<String> {
    let sd = rec.dir(id);
    let lower = out::base_of(ws, &rec.base);
    let (upper, work) = (sd.join("upper"), sd.join("work"));
    for p in [&lower, &upper, &work] {
        let s = p.to_string_lossy();
        ensure!(
            !s.contains(',') && !s.contains(':'),
            "{s}: an overlay path may not contain ',' or ':'"
        );
    }
    Ok(format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        lower.display(),
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

    /// A `sleep 300` whose fd 9 is open on `path`, flocked exclusive when `lock`; returns once the fd is set up (the child says so on stdout).
    /// The child opens and locks the file itself: an inheritable fd in this test process would leak into children other tests spawn meanwhile.
    fn sleeper_on(path: &Path, lock: bool) -> std::process::Child {
        let script = if lock {
            r#"exec 9<>"$1"; flock -nx 9 || exit 1; echo ready; exec sleep 300"#
        } else {
            r#"exec 9<>"$1"; echo ready; exec sleep 300"#
        };
        let mut c = Command::new("sh")
            .args(["-c", script, "sh"])
            .arg(path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
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
    fn lingering_holder(ws: &Path, id: &str) -> (std::process::Child, std::process::Child) {
        let p = mount_lock_path(ws, id);
        drop(open_lock(&p).unwrap());
        let bystander = sleeper_on(&p, false);
        (sleeper_on(&p, true), bystander)
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
        let Err(v) = admit_keeping(&ws, "c", &cfg, Class::Check, 0, true, None, probe).unwrap()
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
        let verdict = admit_keeping(&ws, "c", &cfg, Class::Check, 0, true, None, || {
            Ok(Ok(stats(10.0, 3.0, 1950)))
        })
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
        let got = admit_keeping(&ws, "c", &cfg, Class::Check, 0, true, None, || {
            Ok(Ok(stats(10.0, 3.0, 1950)))
        });
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

    /// A waiter thread: `admit_keeping` of an existing ram dir `id` on `store`; the result goes to the receiver, the reservation dropped.
    fn waiter(
        ws: &Path,
        id: &'static str,
        class: Class,
        store: &Store,
    ) -> std::sync::mpsc::Receiver<std::result::Result<Need, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ws, store) = (ws.to_owned(), store.clone());
        std::thread::spawn(move || {
            let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
                .unwrap()
                .delta;
            let got = admit_keeping(&ws, id, &cfg, class, 0, true, None, || {
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
            admit_keeping(&worker_ws, "c", &cfg, Class::Check, 0, true, None, || {
                Ok(Ok(stats(10.0, 3.0, 1950)))
            })
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
        assert_eq!(Class::Cross.peak(&d), 20 * GIB);
        // run default = the larger measured WeChat clean install + launch window (ws_config DEFAULT_PEAK_MIB[4]), rounded up
        assert_eq!(
            Class::Run.peak(&d),
            796.5_f64.max(796.6).ceil() as u64 * MIB
        );
    }
}
