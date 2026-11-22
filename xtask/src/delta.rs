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
//! Admission: the store's own accounting (zram mm_stat, statvfs) is the persistent term; a build reserves its class's peak growth for its run ([`judge`]).

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
/// In `<store>/<id>/`: the dir was just made; the first mount forgets the base's units reading the land checkout (out::forget_runs_reading), then removes it.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub base: String,
    pub store: PathBuf,
}

impl Record {
    fn text(&self) -> String {
        format!("base={}\nstore={}\n", self.base, self.store.display())
    }

    fn parse(text: &str) -> Result<Record> {
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .with_context(|| format!("{RECORD} has no `{k}=` line: {text:?}"))
        };
        Ok(Record {
            base: field("base")?.to_owned(),
            store: PathBuf::from(field("store")?),
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
    /// Reserve `need_ram` bytes and start.
    Admit { need_ram: u64 },
    /// Room exists once other builds' reservations end.
    Wait(String),
    /// Not now: the store is full by its own accounting.
    Refuse(String),
    /// The class can never fit this store: pool tier.
    NeverFits(String),
    /// Fits only without land's hold ([`Hold`]): a standing reservation, so waiting is pointless.
    Held(String),
}

impl Verdict {
    /// The stated numbers of a non-admitting verdict.
    pub fn reason(&self) -> String {
        match self {
            Verdict::Admit { need_ram } => format!("admitted, {:.2} GiB RAM", gib(*need_ram)),
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
    /// `class`'s peak on `s`: ram = ceil(raw / ratio); class run's ratio is capped at `delta.ratio_run` (app data compresses worse than the build uppers the device ratio mostly measures).
    pub fn of(class: Class, cfg: &DeltaConfig, s: &Stats) -> Need {
        let raw = class.peak(cfg);
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

/// Land first (plan ram-delta S3): room for land's next native build that every other admission leaves free. Zero for land itself, while land's own reservation is live (it stands in), and when land's native class never fits the store (land is pool then).
pub type Hold = Need;

/// The hold an admission by `id` counts on `s`; `land_live` = a live reservation of land exists.
pub fn hold_for(id: &str, cfg: &DeltaConfig, s: &Stats, land_live: bool) -> Hold {
    if id == out::LAND_ID || land_live {
        return Hold::default();
    }
    let native = Need::of(Class::Native, cfg, s);
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
    Verdict::Admit { need_ram }
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

/// The verdict for `class` by `id` on `stats` against the live reservations and land's hold; the caller holds the admit gate. Counts `delta-land-hold` on a [`Verdict::Held`].
fn judge_now(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    stats: &Stats,
) -> Result<Verdict> {
    let (others, land_live) = reserved(ws)?;
    let hold = hold_for(id, cfg, stats, land_live);
    let v = judge(
        stats,
        Need::of(class, cfg, stats),
        others,
        hold,
        cfg.ram_margin_gib.value * GIB,
    );
    if matches!(v, Verdict::Held(_)) {
        out::count(ws, "delta-land-hold");
    }
    Ok(v)
}

/// Land's hold on `stats` as a non-land admission sees it now (`out show`).
pub fn hold_now(ws: &Path, cfg: &DeltaConfig, stats: &Stats) -> Result<Hold> {
    let (_, land_live) = reserved(ws)?;
    Ok(hold_for("", cfg, stats, land_live))
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

/// An admitted reservation: the guard, the store's stats it was judged on, the RAM bytes reserved.
pub type Admitted = (Reservation, Stats, u64);

/// One admission try of `class` on `probed` (a [`probe`] result), never waiting: run dirs take the pool tier when short (out_run.rs), a wait would only stall an app start. Ok(Err(verdict)) = not admitted, Wait included.
pub fn admit_once(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    probed: std::result::Result<Stats, String>,
) -> Result<std::result::Result<Admitted, Verdict>> {
    let stats = match probed {
        Ok(s) => s,
        Err(why) => return Ok(Err(Verdict::NeverFits(why))),
    };
    let gate = lock_dir(ws).join("admit.lock");
    let _gate = Flock::take(open_lock(&gate)?, Mode::Exclusive)?;
    match judge_now(ws, id, cfg, class, &stats)? {
        Verdict::Admit { need_ram } => Ok(Ok((reserve(ws, id, need_ram)?, stats, need_ram))),
        other => Ok(Err(other)),
    }
}

/// Whether a build of `class` by `id` would be admitted on `store` now: one try, its reservation released at once (out::relay_land picks land's tier with it). Ok(the admitted line) or Err(why not).
pub fn fits_now(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    store: &Path,
) -> Result<std::result::Result<String, String>> {
    Ok(match admit_once(ws, id, cfg, class, probe(store, cfg)?)? {
        Ok((_released, stats, need_ram)) => Ok(format!(
            "{} build fits, {:.2} GiB RAM; store {}: {}",
            class.name(),
            gib(need_ram),
            store.display(),
            stats.line()
        )),
        Err(v) => Err(format!("{} build: {}", class.name(), v.reason())),
    })
}

/// Admits a build of `class` on store stats from `probe_now`, waiting while only others' reservations stand in the way. Ok(Ok(reservation)), or Ok(Err(verdict)) for Refuse / NeverFits / Held.
fn admit(
    ws: &Path,
    id: &str,
    cfg: &DeltaConfig,
    class: Class,
    mut probe_now: impl FnMut() -> Result<std::result::Result<Stats, String>>,
) -> Result<std::result::Result<(Reservation, Stats, u64), Verdict>> {
    let mut said = false;
    loop {
        let stats = match probe_now()? {
            Ok(s) => s,
            Err(why) => return Ok(Err(Verdict::NeverFits(why))),
        };
        let gate = lock_dir(ws).join("admit.lock");
        let _gate = Flock::take(open_lock(&gate)?, Mode::Exclusive)?;
        match judge_now(ws, id, cfg, class, &stats)? {
            Verdict::Admit { need_ram } => {
                return Ok(Ok((reserve(ws, id, need_ram)?, stats, need_ram)));
            }
            Verdict::Wait(why) => {
                if !said {
                    println!("out: delta ws/{id}: waiting: {why}");
                    out::count(ws, "delta-wait");
                    said = true;
                }
            }
            other => return Ok(Err(other)),
        }
        drop(_gate);
        std::thread::sleep(build_pool::POLL);
    }
}

/// Makes ram dir `ws/<id>` over the newest base that is an ancestor of HEAD: record (under [`bases_lock`]), empty upper/work with `.base`, the fresh marker (not for land: its units' paths are its own, as out::make), the checkout's mtimes restored from the base when `restore` (out::make's rule). Err when there is no base (a build from zero is the pool tier's).
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
    if id != out::LAND_ID {
        std::fs::write(sd.join(FRESH), "")?;
    }
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

/// The outer side of every build command: decides the tier, admits, and for `ram` runs the rest of the command inside the mounted namespace. Some(code) = ran there; None = continue here (pool tier, or already inside).
pub fn enter(root: &Path, class: Class) -> Result<Option<ExitCode>> {
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
    let mut rec = record(&dir)?;
    let pool = |reason: &str| -> Result<Option<ExitCode>> {
        println!("out: delta pool ({reason})");
        out::count(&ws, "delta-pool");
        Ok(None)
    };
    if rec.is_none() && out::present(&dir)? {
        return pool(&format!("ws/{id} is a pool dir"));
    }
    let store = rec
        .as_ref()
        .map_or_else(|| cfg.delta.ram_store.value.clone(), |r| r.store.clone());
    let admitted = admit(&ws, &id, &cfg.delta, class, || probe(&store, &cfg.delta))?;
    let (reservation, stats, need_ram) = match admitted {
        Ok(a) => a,
        Err(Verdict::Refuse(why)) if rec.is_some() => {
            out::count(&ws, "delta-refused");
            println!("out: delta refused ({why})");
            bail!(
                "ws/{id} is a ram delta and its store is full ({why}): not started. Free room: `cargo xtask out prune --execute` (gone worktrees' deltas), or move this checkout to the pool: `cargo xtask out tier pool --execute`"
            );
        }
        // land first: the hold is standing, so an existing ram dir is refused (waiting is pointless) and a new dir takes the pool
        Err(Verdict::Held(why)) if rec.is_some() => {
            out::count(&ws, "delta-refused");
            println!("out: delta refused ({why})");
            bail!(
                "ws/{id} is a ram delta and the store's free room is held for land ({why}): not started. Move this checkout to the pool: `cargo xtask out tier pool --execute`"
            );
        }
        Err(Verdict::Held(why)) => return pool(&why),
        Err(Verdict::Refuse(why) | Verdict::NeverFits(why) | Verdict::Wait(why)) => {
            if let Some(r) = rec.take() {
                drop_ram(&ws, &id, &r)?;
                println!(
                    "out: ws/{id}: ram delta dropped: {} build {why}",
                    class.name()
                );
            }
            return pool(&format!("{} build: {why}", class.name()));
        }
        Err(Verdict::Admit { .. }) => bail!("delta admission returned Admit as a refusal"),
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
        "out: delta ram ({} build, {:.2} GiB RAM reserved; store {}: {})",
        class.name(),
        gib(need_ram),
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
    let code = run(cmd, "unshare (delta mount namespace)")?;
    // the namespace's processes may still use the overlay (a daemon a build spawned: sccache server, gradle, ...); the mount ends with the command, so they do too
    end_holders(&ws, &id, &lock_path, std::process::id())?;
    drop(reservation);
    Ok(Some(code))
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

/// Why ram dir `rec` cannot be mounted (its upper or base is gone, e.g. a WSL restart emptied the store); None = intact.
fn lost(ws: &Path, id: &str, rec: &Record) -> Result<Option<String>> {
    let sd = rec.dir(id);
    if !sd.join("upper").is_dir() || !sd.join("work").is_dir() {
        return Ok(Some(format!("no upper in {}", sd.display())));
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

/// Stage mount: uid 0 in a fresh user + mount namespace; mounts the overlay at the dir's real path, forgets a fresh dir's land-reading units, then runs the command as `uid`/`gid` again (stage in).
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
        let n = out::forget_runs_reading(&dir, &ws.join(crate::land::LAND_WT))?;
        std::fs::remove_file(&fresh).with_context(|| format!("removing {}", fresh.display()))?;
        println!("out: ws/{id}: {n} test/bin unit(s) reading the land checkout forgotten");
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
                need_ram: 2_642_949_252
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
                need_ram: 4915 * MIB
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

    /// Land first, hand arithmetic: zram limit 10 GiB, used 3, margin 1 -> RAM free 7 GiB = 7_516_192_768 B; ratio 1.95.
    /// native 9216 MiB = 9_663_676_416 B -> ram ceil(9_663_676_416_000 / 1950) = 4_955_731_496 B (4.62 GiB, plan S3 "land hold 4.62"); check 2_642_949_252 B (admission_arithmetic).
    #[test]
    fn land_hold_keeps_room_for_land() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let s = stats(10.0, 3.0, 1950);
        let native = Hold {
            raw: 9 * GIB,
            ram: 4_955_731_496,
        };
        assert_eq!(hold_for("child", &cfg, &s, false), native);
        let check = Need::of(Class::Check, &cfg, &s);
        assert_eq!(check.ram, 2_642_949_252);
        // with the hold: 2.46 + 4.62 + 1 = 8.07 GiB > 7 free -> held (a new dir takes the pool)
        assert!(matches!(judge(&s, check, 0, native, GIB), Verdict::Held(_)));
        // land's own admission ignores the hold: 4.62 + 1 = 5.62 <= 7
        assert_eq!(hold_for(out::LAND_ID, &cfg, &s, false), Hold::default());
        let land = Need::of(Class::Native, &cfg, &s);
        assert_eq!(
            judge(&s, land, 0, Hold::default(), GIB),
            Verdict::Admit {
                need_ram: 4_955_731_496
            }
        );
        // red control, no hold: the child is admitted, and land behind its reservation waits (4.62 + 2.46 + 1 > 7)
        assert_eq!(
            judge(&s, check, 0, Hold::default(), GIB),
            Verdict::Admit {
                need_ram: 2_642_949_252
            }
        );
        assert!(matches!(
            judge(&s, land, check.ram, Hold::default(), GIB),
            Verdict::Wait(_)
        ));
        // land's live reservation stands in for the hold; a store native never fits (limit 4 < 4.62 + 1) holds nothing
        assert_eq!(hold_for("child", &cfg, &s, true), Hold::default());
        assert_eq!(
            hold_for("child", &cfg, &stats(4.0, 0.0, 1950), false),
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
                hold_for("child", &cfg, &roomy, false),
                GIB
            ),
            Verdict::Held(_)
        ));
    }

    /// Class run takes min(device ratio, delta.ratio_run 1.37): 797 MiB = 835_715_072 B -> ceil(835_715_072_000 / 1370) = 610_011_002 B at device 1.95 (not 428_571_833 at 1.95); tmpfs 1.0 stays 1.0.
    #[test]
    fn run_class_reserves_at_the_app_data_ratio() {
        let cfg = crate::ws_config::parse("", Path::new("/w/workspace.toml"))
            .expect("cfg")
            .delta;
        let run = Need::of(Class::Run, &cfg, &stats(10.0, 0.0, 1950));
        assert_eq!((run.raw, run.ram), (835_715_072, 610_011_002));
        let t = stats(10.0, 0.0, 1000);
        assert_eq!(Need::of(Class::Run, &cfg, &t).ram, 835_715_072);
        // build classes keep the device ratio
        assert_eq!(
            Need::of(Class::Check, &cfg, &stats(10.0, 0.0, 1950)).ram,
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
        let got = admit(&ws, "child", &cfg, Class::Check, || {
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

    #[test]
    fn record_round_trip_and_keys() {
        let r = Record {
            base: "abc".into(),
            store: "/s".into(),
        };
        assert_eq!(Record::parse(&r.text()).unwrap(), r);
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
        assert_eq!(cfg.ram_store.value, Path::new("/z"));
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
