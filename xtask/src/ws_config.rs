// SPDX-License-Identifier: MIT
//! Per-machine build limits: the optional, untracked `<workspace>/workspace.toml`. The only reader of that file.
//! An absent file or key takes its default; an unknown table or key, or an invalid value, is refused naming the file and the key, never ignored.
//!
//! ```toml
//! [pool]                   # machine-wide admission (build_pool.rs)
//! cpu_slots = 16          # concurrent jobs machine-wide (default: the CPU count)
//! mem_gib = 20            # memory reservable by admitted work
//! starve_after_s = 7200   # a ticket waiting this long joins the top tier (build_pool.rs Tier)
//! [pool.native-full]      # per class: CPU min..max (elastic) and GiB reserved; defaults derive from cpu_slots (`Class::default_weight`)
//! cpu_min = 8
//! cpu_max = 8
//! mem_gib = 7
//! [cache]
//! sccache_size = "5G"  # shared C/C++ compiler cache
//! [fetch]
//! for = ["cts"]        # gates whose sources.toml [aosp.needed_by] projects fetch checks out (`native --for` names, `all-gates`); absent = core only
//! [disk]               # free-space floors in GiB, checked before `native` (disk_floor)
//! host_min_free = 6    # Windows drive holding the WSL vhdx
//! host_shrink_below = 30
//! workspace_min_free = 6
//! pool_min_free = 5    # build-dir pool
//! pool_metadata_min_free = 1  # build-dir pool: btrfs metadata headroom (btrfs_space)
//! pool_backing = "/mnt/e/WSL/atl-pool.vhdx"  # file backing the pool's filesystem, as seen in the guest; absent = growth check skipped
//! pool_backing_margin = 2      # host drive under pool_backing must keep this much past the file's worst-case growth
//! [delta]              # RAM tier of build-dir deltas (delta.rs)
//! ram_store = "/mnt/atl-zram"  # filesystem holding children's overlay uppers; used only when RAM-backed (zram or tmpfs)
//! ram_margin_gib = 1   # store RAM kept free past every reservation
//! ratio_default = 1.95 # raw/RAM ratio assumed while the zram device holds < 1 GiB
//! peak_small_gib = 2   # per build class: upper growth reserved before the build starts
//! peak_check_gib = 4.8
//! peak_native_gib = 9
//! peak_cross_gib = 20
//! peak_run_gib = 0.78  # one app data root (clean install + one launch) in a run dir on the store (out_run.rs)
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::build_pool::{Class, ClassWeight};

/// File name under the workspace.
pub const FILE: &str = "workspace.toml";

/// Keys that no longer exist, refused by name with what replaces them: `(table, key, replacement)`.
/// Condition: some workspace.toml still carries one. Delete this list once none does; the unknown-key refusal then covers them.
const RETIRED: [(&str, &str, &str); 4] = [
    (
        "build",
        "slots",
        "each checkout has its own build dir now (`cargo xtask out show` names it)",
    ),
    (
        "build",
        "slow_slots",
        "each checkout has its own build dir now (`cargo xtask out show` names it)",
    ),
    (
        "build",
        "slow_builds",
        "each checkout has its own build dir now (`cargo xtask out show` names it)",
    ),
    (
        "build",
        "parallel_builds",
        "the machine pool replaces the token count: `[pool]` cpu_slots and mem_gib, and the class weights under `[pool.<class>]`",
    ),
];

/// Size limit of the compiler cache (sccache evicts least recently used entries past it), in sccache's `SCCACHE_CACHE_SIZE` syntax.
/// One clean build of the libandroid_runtime closure stores ~80 MB (2026-11-01, zstd entries); 1.7 GB was in use under the old 2 GB limit (2026-11-11), so the owner raised it to 5 GB: ~60 such generations, room for the goal closure to grow.
pub const DEFAULT_SCCACHE_SIZE: &str = "5G";

/// `build.refresh_min_upstream_files`: native refreshes a rebased checkout's build dir (out::refresh) only when at least this many upstream-input files (out::UPSTREAM_INPUTS: patches/, sources.toml) changed between the dir's base and the newer base.
/// Why upstream inputs: refresh keeps the checkout's own cargo roots (cargo units key on the checkout path), so it can only gain on ninja/AOSP outputs, which upstream inputs drive.
/// Measured (2026-11-16): 0 upstream files changed -> keep 149-171 s vs refresh 214-232 s (README-only rebase); 0 upstream + 40 other native files (crates, bp2ninja) -> keep 157-175 s vs refresh 185 s. Refresh never won without an upstream change, so 1 = any upstream change; no rebase with one was measurable yet (no two bases differ there).
pub const DEFAULT_REFRESH_MIN_UPSTREAM_FILES: u32 = 1;

/// `pool.mem_gib` (GiB): memory reservable by admitted work. 20 = the 24 GB WSL cap (owner 2026-11-14) minus a 4 GB floor for the kernel, page cache, WSLg and editors [est, re-measure under load].
pub const DEFAULT_POOL_MEM_GIB: u32 = 20;

/// `pool.starve_after_s`: a ticket waiting this long joins the top tier at its arrival (build_pool.rs `Tier`), so a stream of owner/land tickets cannot hold an agent's back forever.
/// It must exceed the waits FIFO alone produces under load, else every ticket is promoted when the pool is busy and priority vanishes exactly then: the longest measured wait was 2250.8 s (2026-11-18, job s3-ab2, 6 tickets ahead behind two cold natives); 7200 s = ~3.2x that.
pub const DEFAULT_POOL_STARVE_AFTER_S: u32 = 7200;

/// `pool.cpu_slots` default: the host's CPU count (`available_parallelism`), 1 when unknown.
fn default_cpu_slots() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// `disk.host_min_free` (GiB): the Windows drive holding the WSL vhdx. A full libandroid_runtime build from cold grows the vhdx by several GiB, and a host drive that fills mid-build fails writes inside the distro: owner's hard stop 6.
pub const DEFAULT_HOST_MIN_FREE: u64 = 6;
/// `disk.host_shrink_below` (GiB): under it, a note asks to shrink the vhdx (owner: shrink when C: < 30 GB). A note, never a refusal.
pub const DEFAULT_HOST_SHRINK_BELOW: u64 = 30;
/// `disk.workspace_min_free` (GiB): the filesystem holding the workspace (src/, cache/, out/ logs; WSL `/`, filled to 0 B twice on 2026-11-15). One build can grow sccache from empty to its cap ([`DEFAULT_SCCACHE_SIZE`] 5 GiB) + 1 GiB for logs, cargo's registry and /tmp.
pub const DEFAULT_WORKSPACE_MIN_FREE: u64 = 6;
/// `disk.pool_min_free` (GiB): the filesystem holding the build-dir pool. One child build that rewrites its whole copy writes ~4.2 GB on disk (measured 2026-11-15: 8.0 GB logical base at zstd:1 ~2x) + margin.
pub const DEFAULT_POOL_MIN_FREE: u64 = 5;
/// `disk.pool_metadata_min_free` (GiB): btrfs metadata headroom of the build-dir pool (`btrfs_space`: settled metadata free in its chunks, in-flight `may_use` reservations not counted, + unallocated raw space at the profile ratio). statfs cannot see it: 2026-11-16 the pool had 6-7 GB data free while metadata was full with unallocated 0.
/// Derivation (pool measured 2026-11-16): 2.08 GB metadata over 2.16 M paths = ~960 B per path, an upper bound for a reflink copy (a copy shares extent and csum items). A base is 156k paths = ~0.14 GiB. 1 GiB = ~7 base copies: two copies at once plus a build's metadata churn [estimate; re-measure with a real copy's metadata cost].
pub const DEFAULT_POOL_METADATA_MIN_FREE: u64 = 1;
/// `disk.pool_backing_margin` (GiB): what the host drive under `disk.pool_backing` keeps past the backing file's worst-case growth (`disk_floor::backing_row`).
/// Covers the dynamic vhdx's own metadata (BAT + sector bitmaps, well under 1% of a 90 GiB disk = <1 GiB) and other Windows writers to that drive while a build runs (~1 GiB) [est].
pub const DEFAULT_POOL_BACKING_MARGIN: u64 = 2;

/// `delta.ram_store`: the owner's boot unit mounts the zram-backed ext4 there (plan ram-delta D7); absent or not RAM-backed -> every child gets the pool tier, one logged reason per build.
pub const DEFAULT_RAM_STORE: &str = "/mnt/atl-zram";
/// `delta.ram_margin_gib`: zram reports `mem_limit` as an I/O error in ext4, not ENOSPC, so admission stops short of it by this much (~one small build's RAM at lzo-rle, 2 GiB x 0.54).
pub const DEFAULT_RAM_MARGIN_GIB: u64 = 1;
/// `delta.ratio_default` in thousandths: lzo-rle zram orig_data_size / mem_used_total at a full check's peak, 5.04 / 2.59 GiB (ram-delta-prototype B2).
pub const DEFAULT_RATIO_MILLI: u64 = 1950;
/// `delta.peak_*_gib` in MiB: upper growth of one build of the class from an empty upper. small 2 GiB: a Rust change + test (owner estimate 1-2); check 4.8: tmpfs peak of a full check, 4.79 GiB (prototype B); native 9: all-gates / ART rebuild estimate; cross 20: riscv64 cross upper estimate 12-20.
/// run 797: store growth of one app data root = clean install + launch of the largest known app into an empty root (every app run starts so, out_run.rs `fresh_root`).
/// WeChat 8.0 (versionCode 3182) on the zram store, ext4 used from the minimum after the previous root's removal to the run's peak (2 s samples, 2026-11-22): 796.5 and 796.6 MiB in the two windows no other writer touched; peak = end, so no install transient above the final root (du 792.8 MiB) at that resolution. 796.6 -> 797.
/// Small cases: Temple Run root 251 MiB, fossify-math 7 MiB.
pub const DEFAULT_PEAK_MIB: [u64; 5] = [2048, 4915, 9216, 20480, 797];

/// `[delta]`: the RAM tier of build-dir deltas (delta.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaConfig {
    pub ram_store: Setting<PathBuf>,
    pub ram_margin_gib: Setting<u64>,
    /// Thousandths: 1950 = 1.95 raw bytes per RAM byte.
    pub ratio_milli: Setting<u64>,
    /// MiB per `delta::Class`, in `Class::ALL` order.
    pub peak_mib: [Setting<u64>; 5],
}

/// `[disk]`: free-space floors in GiB (`disk_floor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFloors {
    pub host_min_free: Setting<u64>,
    pub host_shrink_below: Setting<u64>,
    pub workspace_min_free: Setting<u64>,
    pub pool_min_free: Setting<u64>,
    pub pool_metadata_min_free: Setting<u64>,
    /// `disk.pool_backing`: the file that backs the build-dir pool's filesystem (a dynamically expanding vhdx), path as the guest sees it (drvfs, e.g. `/mnt/e/WSL/atl-pool.vhdx`).
    /// Machine configuration, not derivable unprivileged: the guest sees only a block device. Absent -> the host growth check is skipped with one logged line (machines whose pool is not a growing file).
    pub pool_backing: Setting<Option<PathBuf>>,
    pub pool_backing_margin: Setting<u64>,
}

/// `[pool]`: the machine-wide admission pool (build_pool.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// `pool.cpu_slots`: CPU slots, i.e. concurrent jobs machine-wide.
    pub cpu_slots: Setting<u32>,
    /// `pool.mem_gib`: memory reservable by admitted work.
    pub mem_gib: Setting<u32>,
    /// `pool.starve_after_s`: the starvation bound of a lower-tier ticket, seconds.
    pub starve_after_s: Setting<u32>,
    /// `pool.<class>`: every class, its default or the file's values.
    pub classes: BTreeMap<Class, ClassWeight>,
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    File,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Setting<T> {
    fn default(value: T) -> Self {
        Self {
            value,
            origin: Origin::Default,
        }
    }

    fn file(value: T) -> Self {
        Self {
            value,
            origin: Origin::File,
        }
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Setting<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = match self.origin {
            Origin::File => "file",
            Origin::Default => "default",
        };
        write!(f, "{} ({from})", self.value)
    }
}

/// The effective limits of one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsConfig {
    /// The file read, or that would be read.
    pub path: PathBuf,
    /// `[pool]`: the admission weights (`build_pool`).
    pub pool: PoolConfig,
    /// `build.refresh_min_upstream_files`: out::refresh's threshold.
    pub refresh_min_upstream_files: Setting<u32>,
    /// `cache.sccache_size`: `SCCACHE_CACHE_SIZE`.
    pub sccache_size: Setting<String>,
    /// `fetch.for`: gates (`gate_goals` names or `all-gates`) whose `needed_by` projects fetch checks out; empty = core only.
    pub fetch_for: Setting<Vec<String>>,
    /// `[disk]` floors.
    pub disk: DiskFloors,
    /// `[delta]`: the RAM tier.
    pub delta: DeltaConfig,
}

impl WsConfig {
    /// `fetch.for` as a report shows it.
    pub fn fetch_for_shown(&self) -> String {
        let from = match self.fetch_for.origin {
            Origin::File => "file",
            Origin::Default => "default",
        };
        if self.fetch_for.value.is_empty() {
            format!("[] = core only ({from})")
        } else {
            format!("[{}] ({from})", self.fetch_for.value.join(", "))
        }
    }
}

/// `fetch --for`: add `gates` to `[fetch] for` in `ws`'s file (created if absent; comments and order kept). Returns the new list; refuses a name that is not a `--for` value.
pub fn add_fetch_for(ws: &Path, gates: &[String]) -> Result<Vec<String>> {
    let cfg = load(ws)?;
    let mut list = cfg.fetch_for.value.clone();
    for g in gates {
        ensure!(
            crate::gate_goals::is_for_value(g),
            "--for {g}: not a gate (gate_goals.rs GATES) or `{}`",
            crate::gate_goals::ALL
        );
        if !list.contains(g) {
            list.push(g.clone());
        }
    }
    let text = match std::fs::read_to_string(&cfg.path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", cfg.path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{}: not valid TOML", cfg.path.display()))?;
    let fetch = doc
        .entry("fetch")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .with_context(|| format!("{}: `fetch` must be a table", cfg.path.display()))?;
    fetch["for"] = toml_edit::value(
        list.iter()
            .map(String::as_str)
            .collect::<toml_edit::Array>(),
    );
    let out = doc.to_string();
    parse(&out, &cfg.path).context("workspace.toml after adding --for")?;
    std::fs::write(&cfg.path, out).with_context(|| format!("writing {}", cfg.path.display()))?;
    Ok(list)
}

/// The config of workspace `ws`: its file's values over the defaults.
pub fn load(ws: &Path) -> Result<WsConfig> {
    let path = ws.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(defaults(path)),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn defaults(path: PathBuf) -> WsConfig {
    WsConfig {
        path,
        pool: PoolConfig {
            cpu_slots: Setting::default(default_cpu_slots()),
            mem_gib: Setting::default(DEFAULT_POOL_MEM_GIB),
            starve_after_s: Setting::default(DEFAULT_POOL_STARVE_AFTER_S),
            classes: class_defaults(default_cpu_slots()),
        },
        refresh_min_upstream_files: Setting::default(DEFAULT_REFRESH_MIN_UPSTREAM_FILES),
        sccache_size: Setting::default(DEFAULT_SCCACHE_SIZE.to_owned()),
        fetch_for: Setting::default(Vec::new()),
        disk: DiskFloors {
            host_min_free: Setting::default(DEFAULT_HOST_MIN_FREE),
            host_shrink_below: Setting::default(DEFAULT_HOST_SHRINK_BELOW),
            workspace_min_free: Setting::default(DEFAULT_WORKSPACE_MIN_FREE),
            pool_min_free: Setting::default(DEFAULT_POOL_MIN_FREE),
            pool_metadata_min_free: Setting::default(DEFAULT_POOL_METADATA_MIN_FREE),
            pool_backing: Setting::default(None),
            pool_backing_margin: Setting::default(DEFAULT_POOL_BACKING_MARGIN),
        },
        delta: DeltaConfig {
            ram_store: Setting::default(PathBuf::from(DEFAULT_RAM_STORE)),
            ram_margin_gib: Setting::default(DEFAULT_RAM_MARGIN_GIB),
            ratio_milli: Setting::default(DEFAULT_RATIO_MILLI),
            peak_mib: DEFAULT_PEAK_MIB.map(Setting::default),
        },
    }
}

/// `text` as the content of `path`; errors name `path` and the key.
pub fn parse(text: &str, path: &Path) -> Result<WsConfig> {
    let shown = path.display();
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("{shown}: not valid TOML"))?;
    let mut cfg = defaults(path.to_owned());
    // class tables apply after the loop: their defaults derive from `pool.cpu_slots`, which may come later in the table
    let mut class_tables = Vec::new();
    for (name, section) in &table {
        let toml::Value::Table(section) = section else {
            bail!(
                "{shown}: `{name}` must be a table ([build], [cache], [fetch], [disk], [delta] or [pool])"
            );
        };
        for (key, value) in section {
            let at = format!("{shown}: `{name}.{key}`");
            if let Some((_, _, why)) = RETIRED
                .iter()
                .find(|(t, k, _)| *t == name.as_str() && *k == key.as_str())
            {
                bail!("{at} is retired: {why}; delete the key from {shown}");
            }
            match (name.as_str(), key.as_str()) {
                ("pool", "cpu_slots") => cfg.pool.cpu_slots = Setting::file(count(value, &at)?),
                ("pool", "mem_gib") => cfg.pool.mem_gib = Setting::file(whole(value, &at)?),
                ("pool", "starve_after_s") => {
                    cfg.pool.starve_after_s = Setting::file(count(value, &at)?);
                }
                ("pool", k) => class_tables.push((k, value, at)),
                ("build", "refresh_min_upstream_files") => {
                    cfg.refresh_min_upstream_files = Setting::file(count(value, &at)?);
                }
                ("cache", "sccache_size") => cfg.sccache_size = Setting::file(size(value, &at)?),
                ("fetch", "for") => cfg.fetch_for = Setting::file(gates(value, &at)?),
                ("disk", "host_min_free") => {
                    cfg.disk.host_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "host_shrink_below") => {
                    cfg.disk.host_shrink_below = Setting::file(gib(value, &at)?);
                }
                ("disk", "workspace_min_free") => {
                    cfg.disk.workspace_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "pool_min_free") => {
                    cfg.disk.pool_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "pool_metadata_min_free") => {
                    cfg.disk.pool_metadata_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "pool_backing") => {
                    let toml::Value::String(p) = value else {
                        bail!("{at} must be an absolute path string, not {value}");
                    };
                    if !Path::new(p).is_absolute() {
                        bail!("{at} must be an absolute path, not {p:?}");
                    }
                    cfg.disk.pool_backing = Setting::file(Some(PathBuf::from(p)));
                }
                ("disk", "pool_backing_margin") => {
                    cfg.disk.pool_backing_margin = Setting::file(gib(value, &at)?);
                }
                ("delta", "ram_store") => {
                    let toml::Value::String(p) = value else {
                        bail!("{at} must be an absolute path string, not {value}");
                    };
                    if !Path::new(p).is_absolute() {
                        bail!("{at} must be an absolute path, not {p:?}");
                    }
                    cfg.delta.ram_store = Setting::file(PathBuf::from(p));
                }
                ("delta", "ram_margin_gib") => {
                    cfg.delta.ram_margin_gib = Setting::file(gib(value, &at)?);
                }
                ("delta", "ratio_default") => {
                    let milli = thousandths(value, &at)?;
                    ensure!(milli >= 1000, "{at} must be at least 1.0, not {value}");
                    cfg.delta.ratio_milli = Setting::file(milli);
                }
                ("delta", k) if crate::delta::Class::from_key(k).is_some() => {
                    let class = crate::delta::Class::from_key(k).context("checked above")?;
                    let mib = thousandths(value, &at)? * 1024 / 1000;
                    ensure!(mib > 0, "{at} must be above 0, not {value}");
                    cfg.delta.peak_mib[class as usize] = Setting::file(mib);
                }
                _ => bail!(
                    "{at} is not a known key (known: build.refresh_min_upstream_files, cache.sccache_size, fetch.for, disk.host_min_free, disk.host_shrink_below, disk.workspace_min_free, disk.pool_min_free, disk.pool_metadata_min_free, disk.pool_backing, disk.pool_backing_margin, delta.ram_store, delta.ram_margin_gib, delta.ratio_default, delta.peak_{{small,check,native,cross,run}}_gib, pool.cpu_slots, pool.mem_gib, pool.starve_after_s, pool.<class>)"
                ),
            }
        }
    }
    cfg.pool.classes = class_defaults(cfg.pool.cpu_slots.value);
    for (key, value, at) in class_tables {
        class_table(&mut cfg.pool, key, value, &at)?;
    }
    Ok(cfg)
}

/// Every class at its default weight on a pool of `cpu_slots` (`Class::default_weight`).
fn class_defaults(cpu_slots: u32) -> BTreeMap<Class, ClassWeight> {
    Class::ALL
        .into_iter()
        .map(|c| (c, c.default_weight(cpu_slots)))
        .collect()
}

/// One `[pool.<class>]` table (`cpu_min`, `cpu_max`, `mem_gib`; absent keys keep the class default).
fn class_table(pool: &mut PoolConfig, key: &str, value: &toml::Value, at: &str) -> Result<()> {
    let Some(class) = Class::from_name(key) else {
        bail!(
            "{at} is not a known key (known: pool.cpu_slots, pool.mem_gib, pool.starve_after_s, pool.<class> with {})",
            Class::ALL.map(Class::name).join(", ")
        );
    };
    let toml::Value::Table(fields) = value else {
        bail!("{at} must be a table with cpu_min, cpu_max and mem_gib");
    };
    let mut w = pool.classes[&class];
    for (field, v) in fields {
        let fat = format!("{}.{field}`", at.trim_end_matches('`'));
        match field.as_str() {
            "cpu_min" => w.cpu_min = count(v, &fat)?,
            "cpu_max" => w.cpu_max = count(v, &fat)?,
            "mem_gib" => w.mem_gib = whole(v, &fat)?,
            _ => bail!("{fat} is not a known key (known: cpu_min, cpu_max, mem_gib)"),
        }
    }
    ensure!(
        w.cpu_min <= w.cpu_max,
        "{at}: cpu_min {} is above cpu_max {}",
        w.cpu_min,
        w.cpu_max
    );
    pool.classes.insert(class, w);
    Ok(())
}

/// A positive integer.
fn count(value: &toml::Value, at: &str) -> Result<u32> {
    let toml::Value::Integer(n) = value else {
        bail!("{at} must be a positive integer, not {value}");
    };
    let n = u32::try_from(*n).ok().filter(|&n| n > 0);
    n.with_context(|| format!("{at} must be a positive integer, not {value}"))
}

/// A whole number, 0 allowed.
fn whole(value: &toml::Value, at: &str) -> Result<u32> {
    let toml::Value::Integer(n) = value else {
        bail!("{at} must be a whole number, not {value}");
    };
    u32::try_from(*n).with_context(|| format!("{at} must be a whole number, not {value}"))
}

/// A whole number of GiB, 0 allowed (no floor).
fn gib(value: &toml::Value, at: &str) -> Result<u64> {
    let toml::Value::Integer(n) = value else {
        bail!("{at} must be a whole number of GiB, not {value}");
    };
    u64::try_from(*n).with_context(|| format!("{at} must be a whole number of GiB, not {value}"))
}

/// A non-negative number (integer or float) in thousandths: 4.8 -> 4800.
fn thousandths(value: &toml::Value, at: &str) -> Result<u64> {
    let x = match value {
        toml::Value::Integer(n) => *n as f64,
        toml::Value::Float(f) => *f,
        _ => bail!("{at} must be a number, not {value}"),
    };
    ensure!(
        x.is_finite() && x >= 0.0,
        "{at} must be a non-negative number, not {value}"
    );
    Ok((x * 1000.0).round() as u64)
}

/// A list of distinct `--for` values (gate names, `all-gates`).
fn gates(value: &toml::Value, at: &str) -> Result<Vec<String>> {
    let toml::Value::Array(items) = value else {
        bail!("{at} must be a list of gate names, not {value}");
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let toml::Value::String(g) = item else {
            bail!("{at}: {item} is not a gate name string");
        };
        ensure!(
            crate::gate_goals::is_for_value(g),
            "{at}: `{g}` is not a gate (gate_goals.rs GATES) or `{}`",
            crate::gate_goals::ALL
        );
        ensure!(!out.contains(g), "{at}: `{g}` is listed twice");
        out.push(g.clone());
    }
    Ok(out)
}

/// A size in sccache's syntax: digits then one of K, M, G, T (sccache `config.rs` `parse_size`: a suffix-less number is bytes, which no cache this size wants, so a unit is required here).
fn size(value: &toml::Value, at: &str) -> Result<String> {
    let toml::Value::String(s) = value else {
        bail!("{at} must be a size string such as \"5G\", not {value}");
    };
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let ok = matches!(unit, "K" | "M" | "G" | "T")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.parse::<u64>().is_ok_and(|n| n > 0);
    ensure!(
        ok,
        "{at} must be a positive whole number with a unit K, M, G or T (e.g. \"5G\"), not {s:?}"
    );
    Ok(s.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("/ws/workspace.toml")
    }

    fn err(text: &str) -> String {
        format!("{:#}", parse(text, path()).expect_err("must refuse"))
    }

    /// Expected defaults are the spec's (owner 2026-11-11 and 2026-11-14): pool.mem_gib 20, sccache_size 5G; cpu_slots is the host's CPU count.
    #[test]
    fn empty_file_and_absent_file_give_the_defaults() {
        let cfg = parse("", path()).expect("empty");
        assert_eq!(cfg.pool.mem_gib, Setting::default(20));
        assert_eq!(cfg.pool.cpu_slots.origin, Origin::Default);
        assert_eq!(
            cfg.pool.classes[&Class::NativeFull],
            Class::NativeFull.default_weight(cfg.pool.cpu_slots.value)
        );
        assert_eq!(cfg.sccache_size, Setting::default("5G".to_owned()));
        let dir = std::env::temp_dir().join(format!("xtask-wscfg-{}-absent", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let absent = load(&dir).expect("absent file");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(absent.pool.mem_gib, Setting::default(20));
        assert_eq!(absent.sccache_size.origin, Origin::Default);
    }

    /// Defaults follow cpu_slots (check-admission D2): native-full = half the slots, both ends; native-incr and cargo = 4 ..= cpu_slots - 1, min clamped to the max on a tiny pool. Values by hand per slot count.
    #[test]
    fn class_defaults_derive_from_cpu_slots() {
        // (cpu_slots, native-full, native-incr, cargo)
        let cases = [
            (16, w(8, 8, 7), w(4, 15, 2), w(4, 15, 6)),
            (6, w(3, 3, 7), w(4, 5, 2), w(4, 5, 6)),
            (3, w(1, 1, 7), w(2, 2, 2), w(2, 2, 6)),
            (1, w(1, 1, 7), w(1, 1, 2), w(1, 1, 6)),
        ];
        for (slots, full, incr, cargo) in cases {
            let cfg = parse(&format!("[pool]\ncpu_slots = {slots}\n"), path()).expect("valid");
            let c = &cfg.pool.classes;
            assert_eq!(
                (
                    c[&Class::NativeFull],
                    c[&Class::NativeIncr],
                    c[&Class::Cargo]
                ),
                (full, incr, cargo),
                "cpu_slots {slots}"
            );
        }
    }

    #[test]
    fn file_values_override_and_absent_keys_default() {
        let cfg = parse("[cache]\nsccache_size = \"8G\"\n", path()).expect("valid");
        assert_eq!(cfg.pool.mem_gib, Setting::default(20));
        assert_eq!(cfg.sccache_size, Setting::file("8G".to_owned()));
        // 7200 s: the derivation on DEFAULT_POOL_STARVE_AFTER_S (3.2x the longest measured wait, 2250.8 s)
        assert_eq!(cfg.pool.starve_after_s, Setting::default(7200));
        let all = parse(
            "[pool]\ncpu_slots = 4\nmem_gib = 12\nstarve_after_s = 60\n",
            path(),
        )
        .expect("valid");
        assert_eq!(all.pool.cpu_slots, Setting::file(4));
        assert_eq!(all.pool.mem_gib, Setting::file(12));
        assert_eq!(all.pool.starve_after_s, Setting::file(60));
    }

    fn w(cpu_min: u32, cpu_max: u32, mem_gib: u32) -> ClassWeight {
        ClassWeight {
            cpu_min,
            cpu_max,
            mem_gib,
        }
    }

    /// Class tables override only the keys they name; the others keep the class default on the file's cpu_slots, whichever order the keys come in.
    /// Expected by hand from check-admission D2 on 16 slots: native-full 8..8 (half), cargo 4..15 (cpu_slots - 1).
    #[test]
    fn class_tables_override_per_key() {
        let text = "[pool.native-full]\nmem_gib = 3\n[pool]\ncpu_slots = 16\n";
        let cfg = parse(text, path()).expect("valid");
        assert_eq!(cfg.pool.classes[&Class::NativeFull], w(8, 8, 3));
        assert_eq!(cfg.pool.classes[&Class::Cargo], w(4, 15, 6));
        let e = err("[pool.native-full]\ncpu_min = 9\ncpu_max = 8\n");
        assert!(
            e.contains("`pool.native-full`: cpu_min 9 is above cpu_max 8"),
            "{e}"
        );
        let e = err("[pool.bogus]\ncpu_min = 1\n");
        assert!(e.contains("`pool.bogus` is not a known key"), "{e}");
        let e = err("[pool.native-full]\ncpu = 8\n");
        assert!(
            e.contains("`pool.native-full.cpu` is not a known key"),
            "{e}"
        );
    }

    /// A workspace.toml from before the pool: each retired key is refused by name, with its replacement.
    #[test]
    fn retired_keys_are_refused_with_their_replacement() {
        for key in ["slots", "slow_slots", "slow_builds"] {
            let m = err(&format!("[build]\n{key} = 1\n"));
            assert!(
                m.contains(&format!("`build.{key}`")) && m.contains("out show"),
                "{m}"
            );
        }
        let m = err("[build]\nparallel_builds = 2\n");
        assert!(
            m.contains("`build.parallel_builds` is retired") && m.contains("`[pool]` cpu_slots"),
            "{m}"
        );
    }

    #[test]
    fn load_reads_the_workspace_file() {
        let dir = std::env::temp_dir().join(format!("xtask-wscfg-{}-file", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::write(dir.join(FILE), "[pool]\nmem_gib = 6\n").expect("file");
        let cfg = load(&dir).expect("load");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(cfg.pool.mem_gib, Setting::file(6));
        assert_eq!(cfg.path, dir.join(FILE));
    }

    #[test]
    fn unknown_keys_and_tables_are_refused_by_name() {
        let m = err("[build]\nparallel = 3\n");
        assert!(
            m.contains("/ws/workspace.toml") && m.contains("`build.parallel`"),
            "{m}"
        );
        let m = err("[cash]\nsccache_size = \"5G\"\n");
        assert!(m.contains("`cash.sccache_size`"), "{m}");
        let m = err("parallel_builds = 3\n");
        assert!(m.contains("`parallel_builds` must be a table"), "{m}");
    }

    #[test]
    fn invalid_values_are_refused_by_name() {
        for (text, key) in [
            ("[pool]\ncpu_slots = 0\n", "`pool.cpu_slots`"),
            ("[pool]\ncpu_slots = -1\n", "`pool.cpu_slots`"),
            ("[pool]\nmem_gib = \"6\"\n", "`pool.mem_gib`"),
            ("[pool.cargo]\ncpu_min = 0\n", "`pool.cargo.cpu_min`"),
            ("[pool.cargo]\nmem_gib = -1\n", "`pool.cargo.mem_gib`"),
            ("[cache]\nsccache_size = \"5\"\n", "`cache.sccache_size`"),
            ("[cache]\nsccache_size = \"0G\"\n", "`cache.sccache_size`"),
            ("[cache]\nsccache_size = \"5GB\"\n", "`cache.sccache_size`"),
            ("[cache]\nsccache_size = \"G\"\n", "`cache.sccache_size`"),
            ("[cache]\nsccache_size = 5\n", "`cache.sccache_size`"),
        ] {
            let m = err(text);
            assert!(
                m.contains("/ws/workspace.toml") && m.contains(key),
                "{text}: {m}"
            );
        }
        assert!(err("[build\n").contains("not valid TOML"));
    }

    // gate names from gate_goals.rs GATES (`cts`, `gl`) and its `--for` value `all-gates`
    #[test]
    fn fetch_for_takes_gate_names_and_refuses_others() {
        assert_eq!(
            parse("", path()).unwrap().fetch_for,
            Setting::default(vec![])
        );
        let cfg = parse("[fetch]\nfor = [\"cts\", \"all-gates\"]\n", path()).expect("valid");
        assert_eq!(
            cfg.fetch_for,
            Setting::file(vec!["cts".to_owned(), "all-gates".to_owned()])
        );
        for (text, why) in [
            ("[fetch]\nfor = [\"nope\"]\n", "`nope` is not a gate"),
            (
                "[fetch]\nfor = [\"cts\", \"cts\"]\n",
                "`cts` is listed twice",
            ),
            ("[fetch]\nfor = \"cts\"\n", "must be a list"),
            ("[fetch]\nfor = [1]\n", "not a gate name string"),
            (
                "[fetch]\nfrom = [\"cts\"]\n",
                "`fetch.from` is not a known key",
            ),
        ] {
            let m = err(text);
            assert!(
                m.contains("/ws/workspace.toml") && m.contains(why),
                "{text}: {m}"
            );
        }
    }

    #[test]
    fn add_fetch_for_keeps_the_file_and_refuses_unknown_gates() {
        let dir = std::env::temp_dir().join(format!("xtask-wscfg-{}-for", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::write(dir.join(FILE), "# mine\n[pool]\nmem_gib = 6\n").expect("file");
        assert_eq!(add_fetch_for(&dir, &["cts".into()]).unwrap(), ["cts"]);
        assert_eq!(
            add_fetch_for(&dir, &["cts".into(), "gl".into()]).unwrap(),
            ["cts", "gl"]
        );
        let e = format!("{:#}", add_fetch_for(&dir, &["nope".into()]).unwrap_err());
        assert!(e.contains("--for nope: not a gate"), "{e}");
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        let cfg = load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert!(text.starts_with("# mine\n[pool]\nmem_gib = 6\n"), "{text}");
        assert_eq!(cfg.fetch_for.value, ["cts", "gl"]);
        assert_eq!(cfg.pool.mem_gib, Setting::file(6));
    }

    /// `[disk]` floors: absent -> the documented defaults (6/30/6/5 GiB, each const's derivation); set -> file; 0 allowed; negative or a string refused by key.
    #[test]
    fn disk_floors() {
        let d = parse("", path()).unwrap().disk;
        assert_eq!(d.host_min_free, Setting::default(6));
        assert_eq!(d.host_shrink_below, Setting::default(30));
        assert_eq!(d.workspace_min_free, Setting::default(6));
        assert_eq!(d.pool_min_free, Setting::default(5));
        let d = parse(
            "[disk]\npool_min_free = 8\nworkspace_min_free = 0\n",
            path(),
        )
        .unwrap()
        .disk;
        assert_eq!(d.pool_min_free, Setting::file(8));
        assert_eq!(d.workspace_min_free, Setting::file(0));
        assert_eq!(d.host_min_free, Setting::default(6));
        let e = format!(
            "{:#}",
            parse("[disk]\npool_min_free = -1\n", path()).unwrap_err()
        );
        assert!(
            e.contains("disk.pool_min_free` must be a whole number"),
            "{e}"
        );
        let e = format!("{:#}", parse("[disk]\npool = 1\n", path()).unwrap_err());
        assert!(e.contains("not a known key"), "{e}");
        let d = parse("", path()).unwrap().disk;
        assert_eq!(d.pool_metadata_min_free, Setting::default(1));
        let d = parse("[disk]\npool_metadata_min_free = 2\n", path())
            .unwrap()
            .disk;
        assert_eq!(d.pool_metadata_min_free, Setting::file(2));
        let d = parse("", path()).unwrap().disk;
        assert_eq!(d.pool_backing, Setting::default(None));
        assert_eq!(d.pool_backing_margin, Setting::default(2));
        let d = parse(
            "[disk]\npool_backing = \"/host/WSL/atl-pool.vhdx\"\npool_backing_margin = 3\n",
            path(),
        )
        .unwrap()
        .disk;
        assert_eq!(
            d.pool_backing,
            Setting::file(Some(PathBuf::from("/host/WSL/atl-pool.vhdx")))
        );
        assert_eq!(d.pool_backing_margin, Setting::file(3));
        let e = format!(
            "{:#}",
            parse("[disk]\npool_backing = \"WSL/x.vhdx\"\n", path()).unwrap_err()
        );
        assert!(e.contains("must be an absolute path"), "{e}");
    }

    /// `workspace.toml.pool` (the pool-format machine file, swapped in at the admission land), copied here so the test needs no local file.
    const POOL_FORMAT: &str = r#"# Per-machine workspace settings (not tracked; xtask/src/ws_config.rs reads it).
# 2026-11-16: [build] parallel_builds retired; the machine pool (xtask/src/build_pool.rs) replaces it: CPU slots and reserved memory, held around each leaf build.
# Since 2026-11-16 ~06:00 the host pagefile is 32 GB (commit limit 95.8 GB) and WSL is capped at 24 GB; raise mem_gib only with a measured memory/commit budget.
[pool]
# concurrent jobs machine-wide: the dev host's 16 CPUs
cpu_slots = 16
# GiB reservable by admitted work: the 24 GB WSL cap minus 4 GB for kernel, page cache, WSLg and editors
mem_gib = 20

# per class: CPU min..max (elastic: a grant takes the free slots up to max, at least min; the count is the tool's job count) and GiB reserved (measured peak RSS, rounded up)
# cpu_max 8 would admit two full natives at once (memory fits two); 16 lets one full native take the whole pool and the second wait for CPU
[pool.native-full]
# ninja on a cold or reconfigured graph: peak 7.0 GB (2026-11-16)
cpu_min = 8
cpu_max = 16
mem_gib = 7

[pool.native-incr]
# ninja on a warm graph: peak 1.8 GB
cpu_min = 4
cpu_max = 16
mem_gib = 2

[pool.cargo]
# one cargo build, test or clippy run: check peak 5.4 GB (whole check)
cpu_min = 4
cpu_max = 16
mem_gib = 6

[pool.harness]
# a runtime gate: not measured yet
cpu_min = 4
cpu_max = 6
mem_gib = 2

[pool.light]
# static checks
cpu_min = 1
cpu_max = 1
mem_gib = 0

[fetch]
# The dev machine verifies every gate: fetch keeps every gate's projects (cts included) and check covers them all.
for = ["all-gates"]
"#;

    #[test]
    fn pool_format_file_parses_to_the_shipped_weights() {
        let cfg = parse(POOL_FORMAT, path()).expect("the pool-format file parses");
        assert_eq!(cfg.pool.cpu_slots, Setting::file(16));
        assert_eq!(cfg.pool.mem_gib, Setting::file(20));
        let weight = |c: Class| cfg.pool.classes[&c];
        assert_eq!(
            weight(Class::NativeFull),
            ClassWeight {
                cpu_min: 8,
                cpu_max: 16,
                mem_gib: 7
            }
        );
        assert_eq!(
            weight(Class::NativeIncr),
            ClassWeight {
                cpu_min: 4,
                cpu_max: 16,
                mem_gib: 2
            }
        );
        assert_eq!(
            weight(Class::Cargo),
            ClassWeight {
                cpu_min: 4,
                cpu_max: 16,
                mem_gib: 6
            }
        );
        assert_eq!(
            weight(Class::Harness),
            ClassWeight {
                cpu_min: 4,
                cpu_max: 6,
                mem_gib: 2
            }
        );
        assert_eq!(
            weight(Class::Light),
            ClassWeight {
                cpu_min: 1,
                cpu_max: 1,
                mem_gib: 0
            }
        );
        assert_eq!(cfg.fetch_for.value, ["all-gates"]);
    }

    /// The main-format file (`[build] parallel_builds`) is refused by name with its replacement; the rest of such a file parses once that key is gone.
    #[test]
    fn main_format_file_is_refused_with_the_replacement() {
        let text = "# Per-machine workspace settings\n[build]\n# full native builds machine-wide\nparallel_builds = 2\n\n[fetch]\nfor = [\"all-gates\"]\n";
        let m = err(text);
        assert!(m.contains("`build.parallel_builds` is retired"), "{m}");
        assert!(m.contains("`[pool]` cpu_slots and mem_gib"), "{m}");
        assert!(m.contains("delete the key from /ws/workspace.toml"), "{m}");
        let rest = parse("[fetch]\nfor = [\"all-gates\"]\n", path()).expect("the rest parses");
        assert_eq!(rest.fetch_for.value, ["all-gates"]);
    }
}
