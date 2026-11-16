// SPDX-License-Identifier: MIT
//! Per-machine build limits: the optional, untracked `<workspace>/workspace.toml`. The only reader of that file.
//! An absent file or key takes its default; an unknown table or key, or an invalid value, is refused naming the file and the key, never ignored.
//!
//! ```toml
//! [pool]                   # machine-wide admission (build_pool.rs)
//! cpu_slots = 16          # concurrent jobs machine-wide (default: the CPU count)
//! mem_gib = 20            # memory reservable by admitted work
//! [pool.native-full]      # per class: CPU min..max (elastic) and GiB reserved
//! cpu_min = 8
//! cpu_max = 16
//! mem_gib = 7
//! [cache]
//! sccache_size = "5G"  # shared C/C++ compiler cache
//! [fetch]
//! for = ["cts"]        # gates whose sources.toml [aosp.needed_by] projects fetch checks out (`native --for` names, `all-gates`); absent = core only
//! [disk]               # free-space floors in GiB, checked before `native` (disk_floor)
//! host_min_free = 6    # Windows drive holding the WSL vhdx
//! host_compact_below = 30
//! workspace_min_free = 6
//! pool_min_free = 5    # build-dir pool
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

/// `pool.cpu_slots` default: the host's CPU count (`available_parallelism`), 1 when unknown.
fn default_cpu_slots() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// `disk.host_min_free` (GiB): the Windows drive holding the WSL vhdx. A full libandroid_runtime build from cold grows the vhdx by several GiB, and a host drive that fills mid-build fails writes inside the distro: owner's hard stop 6.
pub const DEFAULT_HOST_MIN_FREE: u64 = 6;
/// `disk.host_compact_below` (GiB): under it, a note asks to compact the vhdx (owner: compact when C: < 30 GB). A note, never a refusal.
pub const DEFAULT_HOST_COMPACT_BELOW: u64 = 30;
/// `disk.workspace_min_free` (GiB): the filesystem holding the workspace (src/, cache/, out/ logs; WSL `/`, filled to 0 B twice on 2026-11-15). One build can grow sccache from empty to its cap ([`DEFAULT_SCCACHE_SIZE`] 5 GiB) + 1 GiB for logs, cargo's registry and /tmp.
pub const DEFAULT_WORKSPACE_MIN_FREE: u64 = 6;
/// `disk.pool_min_free` (GiB): the filesystem holding the build-dir pool. One child build that rewrites its whole copy writes ~4.2 GB on disk (measured 2026-11-15: 8.0 GB logical base at zstd:1 ~2x) + margin.
pub const DEFAULT_POOL_MIN_FREE: u64 = 5;

/// `[disk]`: free-space floors in GiB (`disk_floor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFloors {
    pub host_min_free: Setting<u64>,
    pub host_compact_below: Setting<u64>,
    pub workspace_min_free: Setting<u64>,
    pub pool_min_free: Setting<u64>,
}

/// `[pool]`: the machine-wide admission pool (build_pool.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// `pool.cpu_slots`: CPU slots, i.e. concurrent jobs machine-wide.
    pub cpu_slots: Setting<u32>,
    /// `pool.mem_gib`: memory reservable by admitted work.
    pub mem_gib: Setting<u32>,
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
            classes: Class::ALL
                .into_iter()
                .map(|c| (c, c.default_weight()))
                .collect(),
        },
        refresh_min_upstream_files: Setting::default(DEFAULT_REFRESH_MIN_UPSTREAM_FILES),
        sccache_size: Setting::default(DEFAULT_SCCACHE_SIZE.to_owned()),
        fetch_for: Setting::default(Vec::new()),
        disk: DiskFloors {
            host_min_free: Setting::default(DEFAULT_HOST_MIN_FREE),
            host_compact_below: Setting::default(DEFAULT_HOST_COMPACT_BELOW),
            workspace_min_free: Setting::default(DEFAULT_WORKSPACE_MIN_FREE),
            pool_min_free: Setting::default(DEFAULT_POOL_MIN_FREE),
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
    for (name, section) in &table {
        let toml::Value::Table(section) = section else {
            bail!(
                "{shown}: `{name}` must be a table ([build], [cache], [fetch], [disk] or [pool])"
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
                ("pool", k) => pool_key(&mut cfg.pool, k, value, &at)?,
                ("build", "refresh_min_upstream_files") => {
                    cfg.refresh_min_upstream_files = Setting::file(count(value, &at)?);
                }
                ("cache", "sccache_size") => cfg.sccache_size = Setting::file(size(value, &at)?),
                ("fetch", "for") => cfg.fetch_for = Setting::file(gates(value, &at)?),
                ("disk", "host_min_free") => {
                    cfg.disk.host_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "host_compact_below") => {
                    cfg.disk.host_compact_below = Setting::file(gib(value, &at)?);
                }
                ("disk", "workspace_min_free") => {
                    cfg.disk.workspace_min_free = Setting::file(gib(value, &at)?);
                }
                ("disk", "pool_min_free") => {
                    cfg.disk.pool_min_free = Setting::file(gib(value, &at)?);
                }
                _ => bail!(
                    "{at} is not a known key (known: build.refresh_min_upstream_files, cache.sccache_size, fetch.for, disk.host_min_free, disk.host_compact_below, disk.workspace_min_free, disk.pool_min_free, pool.cpu_slots, pool.mem_gib, pool.<class>)"
                ),
            }
        }
    }
    Ok(cfg)
}

/// One `[pool]` entry: `cpu_slots`, `mem_gib`, or a class table (`[pool.<class>]`: `cpu_min`, `cpu_max`, `mem_gib`; absent keys keep the class default).
fn pool_key(pool: &mut PoolConfig, key: &str, value: &toml::Value, at: &str) -> Result<()> {
    match key {
        "cpu_slots" => pool.cpu_slots = Setting::file(count(value, at)?),
        "mem_gib" => pool.mem_gib = Setting::file(whole(value, at)?),
        _ => {
            let Some(class) = Class::from_name(key) else {
                bail!(
                    "{at} is not a known key (known: pool.cpu_slots, pool.mem_gib, pool.<class> with {})",
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
        }
    }
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
            Class::NativeFull.default_weight()
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

    #[test]
    fn file_values_override_and_absent_keys_default() {
        let cfg = parse("[cache]\nsccache_size = \"8G\"\n", path()).expect("valid");
        assert_eq!(cfg.pool.mem_gib, Setting::default(20));
        assert_eq!(cfg.sccache_size, Setting::file("8G".to_owned()));
        let all = parse("[pool]\ncpu_slots = 4\nmem_gib = 12\n", path()).expect("valid");
        assert_eq!(all.pool.cpu_slots, Setting::file(4));
        assert_eq!(all.pool.mem_gib, Setting::file(12));
    }

    /// Class tables override only the keys they name; the others keep the class default.
    #[test]
    fn class_tables_override_per_key() {
        let cfg = parse("[pool.native-full]\ncpu_max = 8\n", path()).expect("valid");
        let d = Class::NativeFull.default_weight();
        assert_eq!(
            cfg.pool.classes[&Class::NativeFull],
            ClassWeight { cpu_max: 8, ..d }
        );
        assert_eq!(
            cfg.pool.classes[&Class::Cargo],
            Class::Cargo.default_weight()
        );
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
        assert_eq!(d.host_compact_below, Setting::default(30));
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
    }
}
