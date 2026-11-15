// SPDX-License-Identifier: MIT
//! Per-machine build limits: the optional, untracked `<workspace>/workspace.toml`. The only reader of that file.
//! An absent file or key takes its default; an unknown table or key, or an invalid value, is refused naming the file and the key, never ignored.
//!
//! ```toml
//! [build]
//! parallel_builds = 2  # machine-wide full native builds
//! [cache]
//! sccache_size = "5G"  # shared C/C++ compiler cache
//! [fetch]
//! for = ["cts"]        # gates whose sources.toml [aosp.needed_by] projects fetch checks out (`native --for` names, `all-gates`); absent = core only
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// File name under the workspace.
pub const FILE: &str = "workspace.toml";

/// Keys of the fixed slot pool, replaced by per-checkout build dirs (out.rs): accepted and ignored, with a note, so a workspace.toml still read by binaries from before the pool keeps working for them.
/// Condition: some checkout still runs a pre-pool xtask. Delete this list, and the keys from the file, once none does (the slot disks' retirement).
const RETIRED: [&str; 3] = ["slots", "slow_slots", "slow_builds"];

/// Concurrent full native builds. 2 = the owner's limit on the dev machine (20 cores, 31 GB RAM): one ninja `-k 0` build already takes every core, and its peak link steps (ART, libcore) take several GB each, so a third build only adds memory pressure.
pub const DEFAULT_PARALLEL_BUILDS: u32 = 2;

/// Size limit of the compiler cache (sccache evicts least recently used entries past it), in sccache's `SCCACHE_CACHE_SIZE` syntax.
/// One clean build of the libandroid_runtime closure stores ~80 MB (2026-11-01, zstd entries); 1.7 GB was in use under the old 2 GB limit (2026-11-11), so the owner raised it to 5 GB: ~60 such generations, room for the goal closure to grow.
pub const DEFAULT_SCCACHE_SIZE: &str = "5G";

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
    /// `build.parallel_builds`: build tokens (`build_pool`).
    pub parallel_builds: Setting<u32>,
    /// `cache.sccache_size`: `SCCACHE_CACHE_SIZE`.
    pub sccache_size: Setting<String>,
    /// `fetch.for`: gates (`gate_goals` names or `all-gates`) whose `needed_by` projects fetch checks out; empty = core only.
    pub fetch_for: Setting<Vec<String>>,
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
        parallel_builds: Setting::default(DEFAULT_PARALLEL_BUILDS),
        sccache_size: Setting::default(DEFAULT_SCCACHE_SIZE.to_owned()),
        fetch_for: Setting::default(Vec::new()),
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
            bail!("{shown}: `{name}` must be a table ([build] or [cache])");
        };
        for (key, value) in section {
            let at = format!("{shown}: `{name}.{key}`");
            match (name.as_str(), key.as_str()) {
                ("build", "parallel_builds") => {
                    cfg.parallel_builds = Setting::file(count(value, &at)?);
                }
                ("build", k) if RETIRED.contains(&k) => {
                    static NOTED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!(
                            "{at}: retired with the slot pool, ignored (with any other {RETIRED:?}); delete them once no pre-pool checkout runs"
                        );
                    }
                }
                ("cache", "sccache_size") => cfg.sccache_size = Setting::file(size(value, &at)?),
                ("fetch", "for") => cfg.fetch_for = Setting::file(gates(value, &at)?),
                _ => bail!(
                    "{at} is not a known key (known: build.parallel_builds, cache.sccache_size, fetch.for)"
                ),
            }
        }
    }
    Ok(cfg)
}

/// A positive integer.
fn count(value: &toml::Value, at: &str) -> Result<u32> {
    let toml::Value::Integer(n) = value else {
        bail!("{at} must be a positive integer, not {value}");
    };
    let n = u32::try_from(*n).ok().filter(|&n| n > 0);
    n.with_context(|| format!("{at} must be a positive integer, not {value}"))
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

    // expected defaults are the spec's (owner 2026-11-11): parallel_builds 2, sccache_size 5G
    #[test]
    fn empty_file_and_absent_file_give_the_defaults() {
        let cfg = parse("", path()).expect("empty");
        assert_eq!(cfg.parallel_builds, Setting::default(2));
        assert_eq!(cfg.sccache_size, Setting::default("5G".to_owned()));
        let dir = std::env::temp_dir().join(format!("xtask-wscfg-{}-absent", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let absent = load(&dir).expect("absent file");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(absent.parallel_builds, Setting::default(2));
        assert_eq!(absent.sccache_size.origin, Origin::Default);
    }

    #[test]
    fn file_values_override_and_absent_keys_default() {
        let cfg = parse("[cache]\nsccache_size = \"8G\"\n", path()).expect("valid");
        assert_eq!(cfg.parallel_builds, Setting::default(2));
        assert_eq!(cfg.sccache_size, Setting::file("8G".to_owned()));
        let all = parse("[build]\nparallel_builds = 4\n", path()).expect("valid");
        assert_eq!(all.parallel_builds, Setting::file(4));
    }

    /// The live workspace.toml of pre-pool checkouts still carries the slot keys: accepted, whatever their value, and ignored.
    #[test]
    fn retired_slot_keys_are_ignored() {
        let cfg = parse(
            "[build]\nslots = 6\nparallel_builds = 2\nslow_slots = [5, 6]\nslow_builds = 1\n",
            path(),
        )
        .expect("retired keys accepted");
        assert_eq!(cfg.parallel_builds, Setting::file(2));
    }

    #[test]
    fn load_reads_the_workspace_file() {
        let dir = std::env::temp_dir().join(format!("xtask-wscfg-{}-file", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::write(dir.join(FILE), "[build]\nparallel_builds = 1\n").expect("file");
        let cfg = load(&dir).expect("load");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(cfg.parallel_builds, Setting::file(1));
        assert_eq!(cfg.path, dir.join(FILE));
    }

    #[test]
    fn unknown_keys_and_tables_are_refused_by_name() {
        let m = err("[build]\nslot = 3\n");
        assert!(
            m.contains("/ws/workspace.toml") && m.contains("`build.slot`"),
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
            ("[build]\nparallel_builds = 0\n", "`build.parallel_builds`"),
            ("[build]\nparallel_builds = -1\n", "`build.parallel_builds`"),
            (
                "[build]\nparallel_builds = \"3\"\n",
                "`build.parallel_builds`",
            ),
            (
                "[build]\nparallel_builds = 2.5\n",
                "`build.parallel_builds`",
            ),
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
        std::fs::write(dir.join(FILE), "# mine\n[build]\nparallel_builds = 1\n").expect("file");
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
        assert!(
            text.starts_with("# mine\n[build]\nparallel_builds = 1\n"),
            "{text}"
        );
        assert_eq!(cfg.fetch_for.value, ["cts", "gl"]);
        assert_eq!(cfg.parallel_builds, Setting::file(1));
    }
}
