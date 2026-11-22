// SPDX-License-Identifier: MIT
//! Run dirs and seeds: regenerable run data (app data roots, bulky one-off outputs) of a checkout, kept apart from its evidence (logs, screenshots: `out/<task>/`, out_tasks.rs) and never cited by a report.
//!
//! Run dir `out/pool/run/<id>/`: one per checkout, keyed by the build dir's id (out.rs `checkout_id`), made on first use by `out show --run` or `out seed use`. Its `.run` record names its tier (`tier=pool`; the ram tier is a later tier of this same dir, chosen when the dir is made and never re-decided while it exists) and every data root a seed gave it (`use=<name> <seed>`). A rebase or refresh leaves it alone: data roots do not depend on the base.
//! Locks: the build dir's (`build_pool`, `ws-<id>.lock`). Making a run dir or copying a seed into it holds the lock shared, as a build does; leave and prune remove the run dir under the exclusive lock they take for the build dir, so a run dir never goes while a build or run of its checkout holds the lock.
//! Seed `out/pool/seed/<package>-<versionCode>/`: an installed data root, made once (`out seed make`, copied, reflinked when the source is on the pool) and read-only by convention (`.seed` record; modes are kept, so a copy of it is writable). `seed use` reflinks it into the run dir: no data bytes written. `out prune` lists seeds no live run dir names that are not their package's newest (highest versionCode), and run dirs whose checkout is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::build_pool;
use crate::out;

/// Run dirs under the pool: `run/<id>`.
const RUN_DIRS: &str = "run";
/// Seeds under the pool: `seed/<package>-<versionCode>`.
const SEED_DIRS: &str = "seed";
/// In a run dir: its tier and the seeds it was given.
pub const RECORD: &str = ".run";
/// In a seed: package, versionCode and the data root it was copied from.
const SEED_RECORD: &str = ".seed";
/// The only run-dir tier so far: on the pool filesystem, same as the seeds (reflink).
const TIER_POOL: &str = "pool";
/// A run dir record's seed-use key.
const USE_KEY: &str = "use=";
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
    /// Data root name -> seed dir name.
    pub uses: BTreeMap<String, String>,
}

impl Record {
    fn new() -> Self {
        Self {
            tier: TIER_POOL.to_owned(),
            uses: BTreeMap::new(),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut tier = None;
        let mut uses = BTreeMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(t) = line.strip_prefix("tier=") {
                ensure!(t == TIER_POOL, "unknown run dir tier {t:?}");
                tier = Some(t.to_owned());
            } else if let Some(u) = line.strip_prefix(USE_KEY) {
                let (name, seed) = u
                    .split_once(' ')
                    .with_context(|| format!("bad use line {line:?}"))?;
                uses.insert(name.to_owned(), seed.to_owned());
            } else {
                bail!("unknown run dir record line {line:?}");
            }
        }
        Ok(Self {
            tier: tier.context("run dir record without tier=")?,
            uses,
        })
    }

    pub fn text(&self) -> String {
        let mut s = format!("tier={}\n", self.tier);
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

/// The run dir of the checkout at `root`, made when missing (through a temp name renamed into place, so a half-made dir never carries a record). Holds the build dir's lock shared while it works; the lock goes with the returned guard.
pub fn ensure(root: &Path) -> Result<(String, PathBuf, build_pool::DirLock)> {
    let ws = Layout::of(root)?.workspace().to_owned();
    let id = out::checkout_id(root)?;
    let held = build_pool::hold_dir(&ws, &id, || {
        eprintln!("out: run/{id} is being pruned or removed, waiting")
    })?;
    let dir = make(&ws, &id)?;
    Ok((id, dir, held))
}

/// [`ensure`]'s work once the lock is held: an existing run dir is returned as it is.
fn make(ws: &Path, id: &str) -> Result<PathBuf> {
    let dir = run_dir_of(ws, id);
    if record(&dir)?.is_some() {
        return Ok(dir);
    }
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
    write_record(&tmp, &Record::new())?;
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => {
            out::count(ws, "run-pool");
            // stderr: `out show --run` prints only the path on stdout
            eprintln!("out: run/{id} made: {} (tier {TIER_POOL})", dir.display());
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
pub fn orphans(ws: &Path, live: &BTreeSet<String>) -> Result<(Dirs, Dirs)> {
    let (gone, kept): (Vec<_>, Vec<_>) = run_dirs(ws)?
        .into_iter()
        .partition(|(id, _)| !live.contains(id));
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
            for (id, path) in run_dirs(&ws)? {
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
    let (id, dir, _held) = ensure(root)?;
    let dst = dir.join(name);
    if out::present(&dst)? {
        ensure!(
            fresh,
            "{} exists: pass --fresh to replace it with a copy of seed {seed}",
            dst.display()
        );
        remove(&dst)?;
    }
    crate::disk_floor::pool_gate(ws, &crate::ws_config::load(ws)?)?;
    let tier = out::copy_tree(&src, &dst, None, &[SEED_RECORD])?;
    let mut rec = record(&dir)?.context("run dir lost its record")?;
    rec.uses.insert(name.to_owned(), seed.clone());
    write_record(&dir, &rec)?;
    out::count(ws, &format!("seed-{}", tier.name()));
    eprintln!("out: run/{id}/{name} from seed {seed} ({})", tier.name());
    Ok(dst)
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
        let mut rec = Record::new();
        rec.uses.insert("wx".into(), "com.tencent.mm-2800".into());
        let text = rec.text();
        assert_eq!(text, "tier=pool\nuse=wx com.tencent.mm-2800\n");
        assert_eq!(Record::parse(&text).unwrap(), rec);
        assert!(Record::parse("tier=ram\n").is_err());
        assert!(Record::parse("use=a b\n").is_err());
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
        let (gone, old) = orphans(&ws, &live).unwrap();
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
        let dir = make(&ws, "t").unwrap();
        assert_eq!(dir, run_dir_of(&ws, "t"));
        assert_eq!(record(&dir).unwrap(), Some(Record::new()));
        let ro = dir.join("root/locked");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("f"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(make(&ws, "t").unwrap(), dir);
        remove(&dir).unwrap();
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&ws);
    }
}
