// SPDX-License-Identifier: MIT
//! `deps.toml` (what must be installed outside the repo) and `xtask deps`. Column meanings live in that
//! file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use serde::Deserialize;

use crate::arches;
use crate::sources::{self, Sources};
use crate::tree;
use crate::unit_use::{self, Echo, Units};
use crate::workspace::{self, Workspace};

pub const FILE: &str = "deps.toml";

/// The archive the sysroot pins come from. Every sysroot row is pinned to trixie main (deps.toml), so one suite.
/// The mirror is transport only: apt verifies the index signature with `DEBIAN_KEYRING`.
const DEBIAN_ARCHIVE: &str = "https://deb.debian.org/debian";
const DEBIAN_SUITE: &str = "trixie";
/// The keyring of `debian-archive-keyring`, the package the host's own Debian sources verify with.
const DEBIAN_KEYRING: &str = "/usr/share/keyrings/debian-archive-keyring.gpg";

pub type Deps = BTreeMap<String, Dep>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub debian: Option<String>,
    pub arch: Option<String>,
    pub pkgconfig: Option<String>,
    pub min: Option<String>,
    pub min_reason: Option<String>,
    pub cargo: Option<String>,
    pub version: Option<String>,
    pub rustup_target: Option<String>,
    pub aosp: Option<String>,
    pub sysroot_debs: Option<Vec<String>>,
    pub binary: Option<String>,
    pub version_arg: Option<String>,
    pub scope: Vec<Scope>,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Build,
    Runtime,
    Test,
}

/// Everything a `used_by` reference may name, collected from the repo itself.
pub struct Consumers {
    pub crates: BTreeSet<String>,
    pub upstreams: BTreeSet<String>,
    pub xtask: BTreeSet<String>,
    /// `[aosp.projects]` paths of sources.toml: what an `aosp` row may name.
    pub projects: BTreeSet<String>,
    /// Cross hosts (arches.toml `[<arch>.host]` with a `sysroot`): arch -> the rows it names. What `host:<arch>` may name.
    pub hosts: BTreeMap<String, HostRows>,
}

/// The rows of a cross host: its sysroot (`sysroot`), the rust std of its target (`rust`, a `rustup_target` row) and its toolchain rows (`cc`, `cxx`, `lld`).
pub struct HostRows {
    pub sysroot: String,
    pub rust: String,
    pub toolchain: Vec<String>,
}

impl Consumers {
    pub fn new(workspace: &Workspace, sources: &Sources, arches: &arches::Arches) -> Self {
        Self {
            crates: workspace.crates.clone(),
            upstreams: sources.upstream_names(),
            xtask: crate::cli::subcommands(),
            projects: sources.aosp.projects.keys().cloned().collect(),
            hosts: arches
                .iter()
                .filter_map(|(arch, a)| {
                    let host = a.host.as_ref()?;
                    let sysroot = host.sysroot.clone()?;
                    let rust = host.rust.clone();
                    let toolchain = [&host.cc, &host.cxx, &host.lld]
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect();
                    Some((
                        arch.clone(),
                        HostRows {
                            sysroot,
                            rust,
                            toolchain,
                        },
                    ))
                })
                .collect(),
        }
    }

    fn resolve(&self, reference: &str) -> Result<()> {
        let Some((kind, name)) = reference.split_once(':') else {
            bail!("`{reference}` is not <kind>:<name>");
        };
        let known = match kind {
            "crate" => &self.crates,
            "upstream" => &self.upstreams,
            "xtask" => &self.xtask,
            _ => bail!("`{reference}`: unknown kind `{kind}` (crate, upstream, xtask)"),
        };
        ensure!(known.contains(name), "`{reference}`: no such {kind}");
        Ok(())
    }
}

pub fn load(root: &Path, consumers: &Consumers) -> Result<Deps> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text, consumers).with_context(|| format!("in {FILE}"))
}

/// One row, unvalidated: xtask code reads the `binary` or `aosp` value it needs. `xtask check` (tables)
/// validates the whole file.
pub fn row(root: &Path, name: &str) -> Result<Dep> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let mut deps: Deps = toml::from_str(&text).with_context(|| format!("in {FILE}"))?;
    deps.remove(name)
        .with_context(|| format!("{FILE}: no row `{name}`"))
}

/// The names of every `sysroot_debs` row: Debian package trees, the platform's files for the licence gate (memory M41 F1).
pub fn sysroot_rows(root: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let deps: Deps = toml::from_str(&text).with_context(|| format!("in {FILE}"))?;
    Ok(deps
        .into_iter()
        .filter(|(_, d)| d.sysroot_debs.is_some())
        .map(|(n, _)| n)
        .collect())
}

/// The executable row `name` declares (`binary`): the name xtask runs.
pub fn binary(root: &Path, name: &str) -> Result<String> {
    row(root, name)?
        .binary
        .with_context(|| format!("{FILE}: row `{name}` names no binary"))
}

/// The AOSP tree path row `name` declares (`aosp`).
pub fn aosp_project(root: &Path, name: &str) -> Result<String> {
    row(root, name)?
        .aosp
        .with_context(|| format!("{FILE}: row `{name}` is not an aosp row"))
}

fn parse(text: &str, consumers: &Consumers) -> Result<Deps> {
    let deps: Deps = toml::from_str(text)?;
    for (name, dep) in &deps {
        validate(name, dep, consumers).with_context(|| format!("[{name}]"))?;
        ensure!(
            sources::is_name(name),
            "[{name}]: name must be [a-z0-9-], starting alphanumeric"
        );
    }
    Ok(deps)
}

fn validate(name: &str, dep: &Dep, consumers: &Consumers) -> Result<()> {
    let distro = dep.debian.is_some() || dep.arch.is_some();
    let cargo = dep.cargo.is_some() || dep.version.is_some();
    let rustup = dep.rustup_target.is_some();
    let aosp = dep.aosp.is_some();
    let sysroot = dep.sysroot_debs.is_some();
    ensure!(
        [distro, cargo, rustup, aosp, sysroot]
            .iter()
            .filter(|s| **s)
            .count()
            == 1,
        "exactly one source: debian/arch, cargo + version, rustup_target, aosp, or sysroot_debs"
    );
    if let Some(pins) = &dep.sysroot_debs {
        validate_sysroot(dep, pins)?;
    }
    if let Some(project) = &dep.aosp {
        ensure!(
            consumers.projects.contains(project),
            "aosp `{project}`: not a [aosp.projects] path in sources.toml"
        );
        ensure!(
            dep.pkgconfig.is_none() && dep.min.is_none() && dep.version_arg.is_none(),
            "aosp rows take no pkgconfig, min or version_arg: the pin is the version"
        );
    }
    if rustup {
        ensure!(
            dep.pkgconfig.is_none() && dep.min.is_none(),
            "pkgconfig and min apply to distro rows only"
        );
    }
    if cargo {
        ensure!(
            dep.cargo.is_some() && dep.version.is_some(),
            "cargo and version go together"
        );
        ensure!(
            dep.pkgconfig.is_none() && dep.min.is_none(),
            "pkgconfig and min apply to distro rows only"
        );
        let version = dep.version.as_deref().unwrap_or_default();
        ensure!(
            is_exact_version(version),
            "version `{version}` is not an exact x.y.z"
        );
    }
    match &dep.binary {
        None => ensure!(dep.version_arg.is_none(), "version_arg needs binary"),
        Some(binary) => {
            ensure!(
                distro || aosp,
                "binary applies to distro and aosp rows only"
            );
            if distro {
                ensure!(
                    is_bin_name(binary),
                    "binary `{binary}` is not a bare executable name"
                );
                ensure!(
                    dep.min.is_some(),
                    "binary needs min: the version its binary reports is checked against it"
                );
                ensure!(
                    dep.pkgconfig.is_none(),
                    "binary and pkgconfig are exclusive"
                );
            } else {
                ensure!(
                    sources::is_tree_path(binary),
                    "binary `{binary}` is not a path inside the project"
                );
            }
        }
    }
    ensure!(
        dep.min.is_some() == dep.min_reason.is_some(),
        "min and min_reason go together"
    );
    if let Some(min) = &dep.min {
        ensure!(
            parse_version(min).is_some(),
            "min `{min}` is not a dotted number"
        );
    }
    ensure!(!dep.scope.is_empty(), "empty scope");
    ensure!(
        dep.scope.iter().collect::<BTreeSet<_>>().len() == dep.scope.len(),
        "repeated scope"
    );
    ensure!(!dep.used_by.is_empty(), "empty used_by");
    for reference in &dep.used_by {
        match reference.strip_prefix(HOST_REF) {
            // the native build of cross host `arch` uses this row only if its arches.toml [<arch>.host] names it as sysroot or toolchain row, or its rust target's std
            Some(arch) => ensure!(
                consumers
                    .hosts
                    .get(arch)
                    .is_some_and(|host| host.sysroot == name
                        || host.toolchain.iter().any(|t| t == name)
                        || dep.rustup_target.as_deref() == Some(host.rust.as_str())),
                "`{reference}`: arches.toml [{arch}.host] does not name this row as its sysroot, cc, cxx, lld or rust target's std"
            ),
            // an optional consumer runs without this row; its target is checked like any other reference
            None => consumers.resolve(reference.strip_prefix(OPTIONAL_REF).unwrap_or(reference))?,
        }
    }
    Ok(())
}

/// A `sysroot_debs` row: non-empty, unique `name=version` pins, and no field of the other sources.
fn validate_sysroot(dep: &Dep, pins: &[String]) -> Result<()> {
    ensure!(!pins.is_empty(), "sysroot_debs is empty");
    ensure!(
        dep.binary.is_none()
            && dep.min.is_none()
            && dep.pkgconfig.is_none()
            && dep.version_arg.is_none(),
        "sysroot_debs rows take no binary, min, pkgconfig or version_arg"
    );
    for pin in pins {
        ensure!(is_pin(pin), "`{pin}` is not name=version");
    }
    let unique: BTreeSet<&str> = pins.iter().map(String::as_str).collect();
    ensure!(
        unique.len() == pins.len(),
        "repeated package in sysroot_debs"
    );
    Ok(())
}

/// `name=version`: a Debian package name (`a-z 0-9 + - .`) and a non-empty version without spaces or `=`.
fn is_pin(s: &str) -> bool {
    s.split_once('=').is_some_and(|(name, version)| {
        !name.is_empty()
            && name.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.')
            })
            && !version.is_empty()
            && !version
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b == b'=')
    })
}

/// A bare executable name: `ld.lld`, `clang++-19`, `pkg-config`.
fn is_bin_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn is_exact_version(s: &str) -> bool {
    parse_version(s).is_some_and(|parts| parts.len() == 3)
}

/// `4.18.6` -> [4, 18, 6]; None unless every component is a number.
fn parse_version(s: &str) -> Option<Vec<u64>> {
    s.split('.').map(|c| c.parse().ok()).collect()
}

/// Dotted-number comparison; a missing component counts as 0 (`4.18` == `4.18.0`).
fn version_at_least(have: &[u64], min: &[u64]) -> bool {
    let len = have.len().max(min.len());
    let pad = |v: &[u64]| {
        (0..len)
            .map(|i| v.get(i).copied().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    pad(have) >= pad(min)
}

/// The first dotted number in a version line: `go version go1.24.4 linux/amd64` -> `1.24.4`,
/// `Debian clang version 19.1.7 (3+b1)` -> `19.1.7`. A bare integer (`2025`) is not a version.
fn first_version(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let starts =
            b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.'));
        if !starts {
            i += 1;
            continue;
        }
        let end = i + b[i..]
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == b'.')
            .count();
        let run = text[i..end].trim_end_matches('.');
        if run.contains('.') && parse_version(run).is_some() {
            return Some(run);
        }
        i = end;
    }
    None
}

/// Upstream part of a distro package version: epoch (`1:`) and revision / pkgrel (last `-…`) dropped,
/// then the leading dotted number (`4.18.6+ds-2` -> `4.18.6`).
fn upstream_version(package_version: &str) -> &str {
    let v = package_version
        .split_once(':')
        .map_or(package_version, |(_, v)| v);
    let v = v.rsplit_once('-').map_or(v, |(v, _)| v);
    let end = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    v[..end].trim_end_matches('.')
}

// ---- xtask deps --check -------------------------------------------------------------------------

/// Check this host against deps.toml (read-only). `--fetch NAME` downloads and unpacks sysroot row NAME: a dry run unless `--execute`.
#[derive(clap::Args)]
pub struct Args {
    /// Verify every row is installed at its required version (sysroot rows: fetched at the indexed SHA256).
    #[arg(long, conflicts_with = "fetch", required_unless_present = "fetch")]
    check: bool,
    /// Download and unpack the `sysroot_debs` row NAME into cache/sysroot/NAME/root, without root. `--execute` refreshes the row's private apt index (cache/apt/<arch>) first.
    #[arg(long, value_name = "NAME", conflicts_with = "check")]
    fetch: Option<String>,
    /// With `--fetch`: does it. Without: prints the packages it would fetch.
    #[arg(long, requires = "fetch")]
    execute: bool,
    /// With `--check`: also checks the cross rows of arch ARCH (its `[<arch>.host]` sysroot and rust std), which plain `--check` skips.
    #[arg(
        long,
        value_name = "ARCH",
        requires = "check",
        conflicts_with = "fetch"
    )]
    host: Option<String>,
}

#[derive(Clone, Copy)]
enum Distro {
    Debian,
    Arch,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if let Some(name) = &args.fetch {
        return fetch_sysroot(root, name, args.execute);
    }
    let report = host_problems(root, args.host.as_deref())?;
    for p in &report.problems {
        println!("deps: {p}");
    }
    if report.problems.is_empty() {
        println!("deps: {}", report.ok_line());
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "deps: {} of {} rows not satisfied",
            report.problems.len(),
            report.rows
        );
        Ok(ExitCode::FAILURE)
    }
}

/// The `used_by` prefix of a cross host's native build: `host:<arch>` names the sysroot or the rust std of its `[<arch>.host]` row.
const HOST_REF: &str = "host:";
/// The `used_by` reference of a cross build (`xtask cross --arch`).
const CROSS_TASK: &str = "xtask:cross";

/// A row only cross builds need: every consumer is `xtask:cross` or `host:<arch>`. Plain `check` and `deps --check` skip it.
pub fn cross_only(dep: &Dep) -> bool {
    dep.used_by
        .iter()
        .all(|r| r == CROSS_TASK || r.starts_with(HOST_REF))
}

/// The `used_by` prefix of a consumer that runs without the row (a fallback): `optional:xtask:fetch`.
const OPTIONAL_REF: &str = "optional:";

/// A row only optional consumers use: each one runs without it. Plain `check` and `deps --check` skip it and name it.
pub fn optional_only(dep: &Dep) -> bool {
    dep.used_by.iter().all(|r| r.starts_with(OPTIONAL_REF))
}

/// A cross row of host `arch` (rust target `rust`): its sysroot (`host:<arch>`) or the rust std of its target.
fn is_host_row(dep: &Dep, arch: &str, rust: &str) -> bool {
    dep.used_by
        .iter()
        .any(|r| r.strip_prefix(HOST_REF) == Some(arch))
        || dep.rustup_target.as_deref() == Some(rust)
}

/// The rust target triple of cross host `arch`: its `[<arch>.host]` row's `rust`.
fn host_rust(arches: &arches::Arches, arch: &str) -> Result<String> {
    arches
        .get(arch)
        .and_then(|a| a.host.as_ref())
        .map(|h| h.rust.clone())
        .with_context(|| format!("{}: no [{arch}.host] row", arches::FILE))
}

/// What a deps check found: rows checked, one `name: problem` per unmet row, and the rows left out (`name (cross-only)` or `name (optional)`).
#[derive(Default)]
pub struct Report {
    pub rows: usize,
    pub problems: Vec<String>,
    pub skipped: Vec<String>,
}

impl Report {
    /// The `deps: ok` line, naming each skipped row so a machine that skips one can see it.
    pub fn ok_line(&self) -> String {
        if self.skipped.is_empty() {
            return format!("ok ({} rows)", self.rows);
        }
        format!(
            "ok ({} rows, {} skipped: {})",
            self.rows,
            self.skipped.len(),
            self.skipped.join(", ")
        )
    }
}

/// Every deps.toml row against this host, read-only: required rows and the build machine's host rows (its sysroot: every probe reads it) always, other cross-only rows only when `host`
/// names their arch, optional rows never (their consumer runs without them). `deps --check` and `xtask check`'s deps step both run it.
pub fn host_problems(root: &Path, host: Option<&str>) -> Result<Report> {
    let arches = arches::load(root)?;
    let (build, _) = arches::build_machine(root, &arches)?;
    let mut targets = vec![(build.to_owned(), host_rust(&arches, build)?)];
    if let Some(arch) = host {
        targets.push((arch.to_owned(), host_rust(&arches, arch)?));
    }
    check_rows(root, &arches, |_, dep| {
        !optional_only(dep)
            && (!cross_only(dep)
                || targets
                    .iter()
                    .any(|(arch, rust)| is_host_row(dep, arch, rust)))
    })
}

/// The cross rows of host `arch` unmet on this host. `cross --arch` runs it before cargo, so the fetch or
/// `rustup target add` command comes first.
pub fn host_rows_problems(root: &Path, arch: &str) -> Result<Vec<String>> {
    let arches = arches::load(root)?;
    let rust = host_rust(&arches, arch)?;
    Ok(check_rows(root, &arches, |_, dep| is_host_row(dep, arch, &rust))?.problems)
}

/// The rows `select` picks, checked against this host; every other row is listed as skipped.
fn check_rows(
    root: &Path,
    arches: &arches::Arches,
    select: impl Fn(&str, &Dep) -> bool,
) -> Result<Report> {
    let sources = sources::load(root)?;
    let deps = load(
        root,
        &Consumers::new(&workspace::load(root)?, &sources, arches),
    )?;
    let distro = host_distro()?;
    let cargo_installed = cargo_installed()?;
    let rustup_targets = rustup_targets(root)?;
    let tree = tree::shared_tree(&Layout::of(root)?);
    let mut report = Report::default();
    for (name, dep) in &deps {
        if !select(name.as_str(), dep) {
            let why = if optional_only(dep) {
                "optional"
            } else {
                "cross-only"
            };
            report.skipped.push(format!("{name} ({why})"));
            continue;
        }
        report.rows += 1;
        let problem = match &dep.sysroot_debs {
            Some(pins) => sysroot_problem(root, name, pins)?,
            None => check_row(dep, distro, &cargo_installed, &rustup_targets, &tree)?,
        };
        if let Some(problem) = problem {
            report.problems.push(format!("{name}: {problem}"));
        }
    }
    Ok(report)
}

fn check_row(
    dep: &Dep,
    distro: Distro,
    cargo_installed: &BTreeMap<String, String>,
    rustup_targets: &BTreeSet<String>,
    tree: &Path,
) -> Result<Option<String>> {
    if let Some(project) = &dep.aosp {
        return Ok(aosp_problem(tree, project, dep.binary.as_deref()));
    }
    if let Some(target) = &dep.rustup_target {
        return Ok((!rustup_targets.contains(target))
            .then(|| format!("missing: rustup target add {target}")));
    }
    if let (Some(krate), Some(version)) = (&dep.cargo, &dep.version) {
        return Ok(match cargo_installed.get(krate) {
            None => Some(format!(
                "missing: cargo install --locked {krate} --version {version}"
            )),
            Some(have) if have != version => Some(format!("has {have}, needs exactly {version}")),
            Some(_) => None,
        });
    }
    let (package, distro_name) = match distro {
        Distro::Debian => (&dep.debian, "Debian"),
        Distro::Arch => (&dep.arch, "Arch"),
    };
    let Some(package) = package else {
        return Ok(Some(format!("no {distro_name} package declared")));
    };
    let Some(installed) = package_version(distro, package)? else {
        return Ok(Some(format!("package {package} not installed")));
    };
    if let Some(binary) = &dep.binary {
        let min = dep
            .min
            .as_deref()
            .context("binary row without min (tables check)")?;
        let arg = dep.version_arg.as_deref().unwrap_or("--version");
        return binary_problem(binary, arg, min);
    }
    let Some(min) = &dep.min else { return Ok(None) };
    if let Some(module) = &dep.pkgconfig {
        let ok = Command::new("pkg-config")
            .arg(format!("--atleast-version={min}"))
            .arg(module)
            .status()
            .context("running pkg-config")?
            .success();
        return Ok((!ok).then(|| format!("pkg-config {module} older than {min}")));
    }
    let have = upstream_version(&installed);
    let ok = matches!(
        (parse_version(have), parse_version(min)),
        (Some(h), Some(m)) if version_at_least(&h, &m)
    );
    Ok((!ok).then(|| format!("{package} {installed} older than {min}")))
}

/// An `aosp` row: its project is in the pinned tree (`fetch` puts it at the pin), and so is its binary.
fn aosp_problem(tree: &Path, project: &str, binary: Option<&str>) -> Option<String> {
    let dir = tree.join(project);
    if !dir.is_dir() {
        return Some(format!(
            "{} missing: cargo xtask fetch --execute",
            dir.display()
        ));
    }
    let path = dir.join(binary?);
    (!path.exists()).then(|| format!("{} missing from the pinned tree", path.display()))
}

/// A distro row's binary: runs `<binary> <version_arg>` and compares the first dotted number it prints
/// with `min`. The binary's own report, not the package version, is what xtask gets.
fn binary_problem(binary: &str, version_arg: &str, min: &str) -> Result<Option<String>> {
    let out = match Command::new(binary).arg(version_arg).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(format!("`{binary}` not on PATH")));
        }
        Err(e) => return Err(e).with_context(|| format!("running `{binary} {version_arg}`")),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let Some(have) = first_version(&text) else {
        return Ok(Some(format!("`{binary} {version_arg}` prints no version")));
    };
    let ok = matches!(
        (parse_version(have), parse_version(min)),
        (Some(h), Some(m)) if version_at_least(&h, &m)
    );
    Ok((!ok).then(|| format!("`{binary}` is {have}, needs at least {min}")))
}

fn host_distro() -> Result<Distro> {
    let text = std::fs::read_to_string("/etc/os-release").context("reading /etc/os-release")?;
    let ids: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("ID=").or_else(|| l.strip_prefix("ID_LIKE=")))
        .flat_map(|v| v.trim_matches('"').split_whitespace())
        .collect();
    if ids.contains(&"debian") {
        Ok(Distro::Debian)
    } else if ids.contains(&"arch") {
        Ok(Distro::Arch)
    } else {
        bail!("unsupported distro (os-release ids: {ids:?}); deps.toml covers Debian and Arch")
    }
}

/// Installed package version, None if not installed.
fn package_version(distro: Distro, package: &str) -> Result<Option<String>> {
    let out = match distro {
        Distro::Debian => Command::new("dpkg-query")
            .args(["-W", "-f=${db:Status-Abbrev}|${Version}", package])
            .output(),
        Distro::Arch => Command::new("pacman").args(["-Q", package]).output(),
    }
    .context("querying the package manager")?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(match distro {
        // `ii ` = installed; `rc` etc. leave only config files behind
        Distro::Debian => text
            .split_once('|')
            .filter(|(s, _)| s.starts_with("ii"))
            .map(|(_, v)| v),
        Distro::Arch => text.split_whitespace().nth(1),
    }
    .map(str::to_owned))
}

fn cargo_installed() -> Result<BTreeMap<String, String>> {
    let mut cmd = crate::cargo_command();
    cmd.args(["install", "--list"]);
    let out = unit_use::run(&mut cmd, Units::Plain, Echo::Nothing)
        .context("running cargo install --list")?;
    ensure!(out.status.success(), "cargo install --list: {}", out.stderr);
    Ok(parse_install_list(&out.stdout))
}

/// The std targets installed for the pinned toolchain: rustup run in `root` reads rust-toolchain.toml.
fn rustup_targets(root: &Path) -> Result<BTreeSet<String>> {
    let out = Command::new("rustup")
        .current_dir(root)
        .args(["target", "list", "--installed"])
        .output()
        .context("running rustup target list --installed")?;
    ensure!(
        out.status.success(),
        "rustup target list --installed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

/// `cargo install --list`: one `<crate> v<version>[+<build>][ (<source>)]:` line per crate, binaries
/// indented. Build metadata does not identify a SemVer version and is dropped.
fn parse_install_list(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with(char::is_whitespace))
        .filter_map(|l| {
            let mut words = l.trim_end_matches(':').split_whitespace();
            let name = words.next()?;
            let version = words.next()?.strip_prefix('v')?.trim_end_matches(':');
            let version = version.split_once('+').map_or(version, |(v, _)| v);
            Some((name.to_owned(), version.to_owned()))
        })
        .collect()
}

// ---- sysroots (`sysroot_debs`) ------------------------------------------------------------------

/// `cache/sysroot/<name>/` (Layout::sysroot): `debs/` (the downloads), `root/` (the unpacked tree), `stamp.txt` (written last).
fn sysroot_dir(root: &Path, name: &str) -> Result<PathBuf> {
    Ok(Layout::of(root)?.sysroot(name))
}

/// The unpacked tree of sysroot row `name`, once every pin is fetched at its indexed SHA256. `xtask cross` links against it.
pub fn sysroot_root(root: &Path, name: &str) -> Result<PathBuf> {
    let pins = row(root, name)?
        .sysroot_debs
        .with_context(|| format!("{FILE}: row `{name}` is not a sysroot_debs row"))?;
    if let Some(problem) = sysroot_problem(root, name, &pins)? {
        bail!("sysroot {name}: {problem}");
    }
    Ok(sysroot_dir(root, name)?.join("root"))
}

/// apt state private to one Debian architecture: `cache/apt/<arch>/`, refreshed by `apt-get update` without root.
/// The host's lists are amd64 only and its dpkg status is the host's, so every apt call names this state with
/// `-o` options. `/etc/apt` still applies: the host's proxy settings reach the mirror.
struct PrivateApt {
    arch: String,
    dir: PathBuf,
}

impl PrivateApt {
    /// The state of sysroot row `name`: the arch of the arches.toml `[<arch>.host]` row whose `sysroot` names it.
    fn of_sysroot(root: &Path, name: &str) -> Result<Self> {
        let arches = arches::load(root)?;
        let mut hosts = arches
            .values()
            .filter_map(|arch| arch.host.as_ref())
            .filter(|host| host.sysroot.as_deref() == Some(name));
        let host = hosts
            .next()
            .with_context(|| format!("no [<arch>.host] row in arches.toml names sysroot {name}"))?;
        ensure!(
            hosts.next().is_none(),
            "more than one [<arch>.host] row in arches.toml names sysroot {name}"
        );
        let arch = host.debian.clone();
        let dir = Layout::of(root)?
            .workspace()
            .join("cache")
            .join("apt")
            .join(&arch);
        Ok(Self { arch, dir })
    }

    /// `tool` (`apt-get`, `apt-cache`) with this state's options first.
    fn command(&self, tool: &str) -> Command {
        let mut cmd = Command::new(tool);
        cmd.args(self.options());
        cmd
    }

    fn options(&self) -> Vec<String> {
        let state = self.dir.join("state");
        [
            format!("Dir::State={}", state.display()),
            format!("Dir::State::status={}", state.join("status").display()),
            format!("Dir::Cache={}", self.dir.join("cache").display()),
            format!(
                "Dir::Etc::sourcelist={}",
                self.dir.join("sources.list").display()
            ),
            // `-`: no sources.list.d, the host's sources name other suites
            "Dir::Etc::sourceparts=-".to_owned(),
            format!("APT::Architecture={}", self.arch),
            format!("APT::Architectures::={}", self.arch),
            // no translation indexes: nothing here reads them
            "Acquire::Languages=none".to_owned(),
        ]
        .into_iter()
        .flat_map(|option| ["-o".to_owned(), option])
        .collect()
    }

    /// The only source: `DEBIAN_SUITE` main for this arch, signed for `DEBIAN_KEYRING`.
    fn sources_text(&self) -> String {
        format!(
            "deb [arch={} signed-by={DEBIAN_KEYRING}] {DEBIAN_ARCHIVE} {DEBIAN_SUITE} main\n",
            self.arch
        )
    }

    /// Rewrites the source and an empty dpkg status (nothing installed), then runs `apt-get update` into the state.
    fn update(&self) -> Result<()> {
        ensure!(
            Path::new(DEBIAN_KEYRING).is_file(),
            "{DEBIAN_KEYRING} is missing: apt cannot verify {DEBIAN_SUITE} without it"
        );
        let state = self.dir.join("state");
        for dir in [
            state.join("lists").join("partial"),
            self.dir.join("cache").join("archives").join("partial"),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(state.join("status"), "").context("writing the empty dpkg status")?;
        std::fs::write(self.dir.join("sources.list"), self.sources_text())
            .context("writing sources.list")?;
        let status = self
            .command("apt-get")
            .arg("update")
            .status()
            .context("running apt-get update")?;
        ensure!(
            status.success(),
            "apt-get update into {}: {status}",
            self.dir.display()
        );
        Ok(())
    }
}

/// The SHA256 the private index gives the exact `name=version`; None when the index has no such version.
fn index_sha256(apt: &PrivateApt, pin: &str) -> Result<Option<String>> {
    let out = apt
        .command("apt-cache")
        .args(["show", pin])
        .output()
        .context("running apt-cache show")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("SHA256: "))
        .map(str::to_owned))
}

/// `stamp.txt`: one `name=version sha256` line per fetched pin.
fn parse_stamp(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(pin, sha)| (pin.to_owned(), sha.to_owned()))
        .collect()
}

/// Why sysroot row `name` is not usable: no tree, a pin the apt index lacks, or a pin not fetched at the indexed bytes.
fn sysroot_problem(root: &Path, name: &str, pins: &[String]) -> Result<Option<String>> {
    let dir = sysroot_dir(root, name)?;
    let fetch = format!("not fetched: cargo xtask deps --fetch {name} --execute");
    if !dir.join("root").is_dir() {
        return Ok(Some(fetch));
    }
    let stamp = parse_stamp(&std::fs::read_to_string(dir.join("stamp.txt")).unwrap_or_default());
    let apt = PrivateApt::of_sysroot(root, name)?;
    for pin in pins {
        let Some(sha) = index_sha256(&apt, pin)? else {
            return Ok(Some(format!(
                "{pin} is not in the private apt index {} (cargo xtask deps --fetch {name} --execute refreshes it, or fix the pin)",
                apt.dir.display()
            )));
        };
        if stamp.get(pin) != Some(&sha) {
            return Ok(Some(fetch));
        }
    }
    Ok(None)
}

/// `deps --fetch NAME`: refreshes the private index, then for each pin `apt-get download` (no root), its SHA256 checked against that index, `dpkg -x` into `root/`; `stamp.txt` last. Dry run unless `execute`.
fn fetch_sysroot(root: &Path, name: &str, execute: bool) -> Result<ExitCode> {
    let pins = row(root, name)?
        .sysroot_debs
        .with_context(|| format!("{FILE}: row `{name}` is not a sysroot_debs row"))?;
    let dir = sysroot_dir(root, name)?;
    let apt = PrivateApt::of_sysroot(root, name)?;
    if !execute {
        println!(
            "deps --fetch {name}: {} packages into {} (--execute fetches):",
            pins.len(),
            dir.display()
        );
        for pin in &pins {
            println!("  {pin}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    apt.update()?;
    // A half-unpacked tree is not a sysroot: start from nothing.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    let debs = dir.join("debs");
    let tree = dir.join("root");
    for d in [&debs, &tree] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let mut stamp = Vec::new();
    for pin in &pins {
        let expected = index_sha256(&apt, pin)?.with_context(|| {
            format!(
                "{pin} is not in {DEBIAN_SUITE} main for {} (fix the pin)",
                apt.arch
            )
        })?;
        let deb = download_deb(&apt, &debs, pin)?;
        let got = sha256_file(&deb)?;
        ensure!(
            got == expected,
            "{pin}: {} has sha256 {got}, the apt index says {expected}",
            deb.display()
        );
        let status = Command::new("dpkg")
            .arg("-x")
            .arg(&deb)
            .arg(&tree)
            .status()
            .context("running dpkg -x")?;
        ensure!(status.success(), "dpkg -x {}: {status}", deb.display());
        stamp.push(format!("{pin} {got}"));
    }
    std::fs::write(dir.join("stamp.txt"), stamp.join("\n") + "\n").context("writing stamp.txt")?;
    println!(
        "deps --fetch {name}: {} packages unpacked into {}",
        pins.len(),
        tree.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `apt-get download <pin>` in `debs` from the private index: the one file it creates there.
fn download_deb(apt: &PrivateApt, debs: &Path, pin: &str) -> Result<PathBuf> {
    let before = listing(debs)?;
    let status = apt
        .command("apt-get")
        .current_dir(debs)
        .args(["download", pin])
        .status()
        .context("running apt-get download")?;
    ensure!(status.success(), "apt-get download {pin}: {status}");
    let created: Vec<PathBuf> = listing(debs)?.difference(&before).cloned().collect();
    let [deb] = created.as_slice() else {
        bail!(
            "apt-get download {pin} created {} files, expected one",
            created.len()
        );
    };
    Ok(deb.clone())
}

fn listing(dir: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        paths.insert(entry?.path());
    }
    Ok(paths)
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(crate::test_apks::sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consumers() -> Consumers {
        let set = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        Consumers {
            crates: set(&["xtask"]),
            upstreams: set(&["aosp"]),
            xtask: set(&["check", "cross"]),
            projects: set(&["prebuilts/jdk/jdk21", "prebuilts/r8"]),
            hosts: BTreeMap::from([(
                "riscv64".to_owned(),
                HostRows {
                    sysroot: "riscv64-sysroot".to_owned(),
                    rust: "riscv64gc-unknown-linux-gnu".to_owned(),
                    toolchain: vec!["clang-21".to_owned()],
                },
            )]),
        }
    }

    fn err(text: &str) -> String {
        format!(
            "{:#}",
            parse(text, &consumers()).expect_err("should be rejected")
        )
    }

    const USED: &str = "scope = [\"test\"]\nused_by = [\"xtask:check\"]\n";

    #[test]
    fn host_ref_names_the_sysroot_of_its_arch() {
        let pins = "sysroot_debs = [\"a=1\"]\nscope = [\"build\"]\n";
        let ok =
            format!("[riscv64-sysroot]\n{pins}used_by = [\"xtask:cross\", \"host:riscv64\"]\n");
        assert!(parse(&ok, &consumers()).is_ok());
        let other = format!("[other]\n{pins}used_by = [\"host:riscv64\"]\n");
        assert!(err(&other).contains("does not name this row as its sysroot"));
        let unknown = format!("[riscv64-sysroot]\n{pins}used_by = [\"host:x86_64\"]\n");
        assert!(err(&unknown).contains("does not name this row as its sysroot"));
        // a cross host's rust std row is the other row its native build uses
        let std = "[rust-std-riscv64]\nrustup_target = \"riscv64gc-unknown-linux-gnu\"\nscope = [\"build\"]\n";
        assert!(
            parse(
                &format!("{std}used_by = [\"xtask:cross\", \"host:riscv64\"]\n"),
                &consumers()
            )
            .is_ok()
        );
        let other_std = "[rust-std-x86-64]\nrustup_target = \"x86_64-unknown-linux-gnu\"\nscope = [\"build\"]\n";
        assert!(
            err(&format!("{other_std}used_by = [\"host:riscv64\"]\n"))
                .contains("does not name this row as its sysroot")
        );
        // a toolchain row the host names (cc / cxx / lld), and one it does not
        let tool = "debian = \"clang-21\"\nbinary = \"clang-21\"\nmin = \"21.1.8\"\nmin_reason = \"r\"\nscope = [\"build\"]\nused_by = [\"host:riscv64\"]\n";
        parse(&format!("[clang-21]\n{tool}"), &consumers()).expect("a named toolchain row");
        assert!(err(&format!("[clang-22]\n{tool}")).contains("cc, cxx, lld"));
    }

    /// A cross host's rust std row against the installed std targets: the named `rustup target add` problem that cross::cargo_target stops on before any build, and none once installed.
    #[test]
    fn missing_rust_std_is_named_before_any_build() {
        let text = "[rust-std-riscv64]\nrustup_target = \"riscv64gc-unknown-linux-gnu\"\nscope = [\"build\"]\nused_by = [\"xtask:cross\", \"host:riscv64\"]\n";
        let deps = parse(text, &consumers()).unwrap();
        let dep = &deps["rust-std-riscv64"];
        let none = BTreeSet::new();
        assert_eq!(
            check_row(dep, Distro::Debian, &BTreeMap::new(), &none, Path::new("/")).unwrap(),
            Some("missing: rustup target add riscv64gc-unknown-linux-gnu".to_owned())
        );
        let installed = BTreeSet::from(["riscv64gc-unknown-linux-gnu".to_owned()]);
        assert_eq!(
            check_row(
                dep,
                Distro::Debian,
                &BTreeMap::new(),
                &installed,
                Path::new("/")
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn cross_only_means_every_consumer_is_a_cross_build() {
        let row = |used: &str| -> Dep {
            toml::from_str(&format!("scope = [\"build\"]\nused_by = {used}\n")).unwrap()
        };
        assert!(cross_only(&row("[\"xtask:cross\"]")));
        assert!(cross_only(&row("[\"xtask:cross\", \"host:riscv64\"]")));
        assert!(!cross_only(&row("[\"xtask:cross\", \"xtask:native\"]")));
        assert!(!cross_only(&row("[\"xtask:native\"]")));
    }

    #[test]
    fn optional_refs_name_a_checked_consumer() {
        let pins = "debian = \"libsecret-tools\"\nscope = [\"test\"]\n";
        let ok = format!("[secret-tool]\n{pins}used_by = [\"optional:xtask:check\"]\n");
        assert!(parse(&ok, &consumers()).is_ok());
        let unknown = format!("[secret-tool]\n{pins}used_by = [\"optional:xtask:nope\"]\n");
        assert!(err(&unknown).contains("no such xtask"));
    }

    #[test]
    fn optional_means_every_consumer_is_optional() {
        let row = |used: &str| -> Dep {
            toml::from_str(&format!("scope = [\"test\"]\nused_by = {used}\n")).unwrap()
        };
        assert!(optional_only(&row("[\"optional:xtask:check\"]")));
        assert!(!optional_only(&row(
            "[\"optional:xtask:check\", \"xtask:cross\"]"
        )));
        assert!(!optional_only(&row("[\"xtask:check\"]")));
    }

    #[test]
    fn skipped_rows_are_named_in_the_ok_line() {
        let report = Report {
            rows: 3,
            problems: Vec::new(),
            skipped: vec!["a".into(), "b".into()],
        };
        assert_eq!(report.ok_line(), "ok (3 rows, 2 skipped: a, b)");
        assert_eq!(
            Report {
                rows: 3,
                ..Report::default()
            }
            .ok_line(),
            "ok (3 rows)"
        );
    }

    #[test]
    fn distro_and_cargo_rows() {
        let text = format!(
            "[git]\ndebian = \"git\"\n{USED}[cargo-deny]\ncargo = \"cargo-deny\"\nversion = \"0.20.2\"\n{USED}"
        );
        let deps = parse(&text, &consumers()).unwrap();
        assert_eq!(deps["git"].debian.as_deref(), Some("git"));
        assert_eq!(deps["cargo-deny"].version.as_deref(), Some("0.20.2"));
    }

    #[test]
    fn two_sources_or_none() {
        let both = format!("[x]\ndebian = \"x\"\ncargo = \"x\"\nversion = \"1.0.0\"\n{USED}");
        assert!(err(&both).contains("exactly one source"));
        assert!(err(&format!("[x]\n{USED}")).contains("exactly one source"));
    }

    #[test]
    fn sysroot_rows_are_exact_pins() {
        let text = format!("[s]\nsysroot_debs = [\"libc6-riscv64-cross=2.41-11cross1\"]\n{USED}");
        let deps = parse(&text, &consumers()).unwrap();
        assert_eq!(deps["s"].sysroot_debs.as_ref().map(Vec::len), Some(1));
        let row = |pins: &str| err(&format!("[s]\nsysroot_debs = {pins}\n{USED}"));
        assert!(row("[\"libc6-riscv64-cross\"]").contains("is not name=version"));
        assert!(row("[\"Libc=1\"]").contains("is not name=version"));
        assert!(row("[\"a=\"]").contains("is not name=version"));
        assert!(row("[]").contains("sysroot_debs is empty"));
        assert!(row("[\"a=1\", \"a=1\"]").contains("repeated package"));
        assert!(row("[\"a=1\"]\nbinary = \"x\"").contains("take no binary"));
        assert!(
            err(&format!(
                "[s]\ndebian = \"x\"\nsysroot_debs = [\"a=1\"]\n{USED}"
            ))
            .contains("exactly one source")
        );
    }

    #[test]
    fn pins_and_stamp_lines() {
        assert!(is_pin("linux-libc-dev-riscv64-cross=6.12.38-1cross1"));
        assert!(!is_pin("a=1 b"));
        let stamp = parse_stamp("libc6=2.41-11cross1 abc123\nlibgcc=14.2.0-19cross1 def456\n");
        assert_eq!(
            stamp.get("libc6=2.41-11cross1").map(String::as_str),
            Some("abc123")
        );
        assert_eq!(stamp.len(), 2);
    }

    #[test]
    fn private_apt_names_only_its_state() {
        let apt = PrivateApt {
            arch: "riscv64".to_owned(),
            dir: PathBuf::from("/c/apt/riscv64"),
        };
        let options = apt.options();
        for want in [
            "Dir::State=/c/apt/riscv64/state",
            "Dir::State::status=/c/apt/riscv64/state/status",
            "Dir::Cache=/c/apt/riscv64/cache",
            "Dir::Etc::sourcelist=/c/apt/riscv64/sources.list",
            "Dir::Etc::sourceparts=-",
            "APT::Architecture=riscv64",
            "APT::Architectures::=riscv64",
        ] {
            assert!(options.iter().any(|o| o == want), "{want}");
        }
        assert!(options.chunks(2).all(|pair| pair[0] == "-o"));
        assert_eq!(
            apt.sources_text(),
            "deb [arch=riscv64 signed-by=/usr/share/keyrings/debian-archive-keyring.gpg] https://deb.debian.org/debian trixie main\n"
        );
    }

    #[test]
    fn sysroot_arch_comes_from_arches() {
        let root = crate::test_root();
        let apt = PrivateApt::of_sysroot(&root, "riscv64-sysroot").unwrap();
        assert_eq!(apt.arch, "riscv64");
        assert!(apt.dir.ends_with("cache/apt/riscv64"));
        assert!(PrivateApt::of_sysroot(&root, "no-such-sysroot").is_err());
    }

    #[test]
    fn min_needs_reason() {
        let text = format!("[x]\ndebian = \"x\"\nmin = \"1.2\"\n{USED}");
        assert!(err(&text).contains("min and min_reason go together"));
    }

    #[test]
    fn cargo_version_must_be_exact() {
        let text = format!("[x]\ncargo = \"x\"\nversion = \"0.20\"\n{USED}");
        assert!(err(&text).contains("not an exact x.y.z"));
    }

    #[test]
    fn references_must_exist() {
        let text = "[x]\ndebian = \"x\"\nscope = [\"build\"]\nused_by = [\"crate:gone\"]\n";
        assert!(err(text).contains("`crate:gone`: no such crate"));
        let text = "[x]\ndebian = \"x\"\nscope = [\"build\"]\nused_by = [\"tool:x\"]\n";
        assert!(err(text).contains("unknown kind `tool`"));
        let text = "[x]\ndebian = \"x\"\nscope = [\"build\"]\nused_by = [\"upstream:aosp\"]\n";
        assert!(parse(text, &consumers()).is_ok());
    }

    #[test]
    fn unknown_scope() {
        let text = "[x]\ndebian = \"x\"\nscope = [\"dev\"]\nused_by = [\"xtask:check\"]\n";
        assert!(err(text).contains("unknown variant `dev`"));
    }

    #[test]
    fn binary_rows() {
        let no_min = format!("[x]\ndebian = \"x\"\nbinary = \"x\"\n{USED}");
        assert!(err(&no_min).contains("binary needs min"));
        let path = format!(
            "[x]\ndebian = \"x\"\nbinary = \"bin/x\"\nmin = \"1\"\nmin_reason = \"r\"\n{USED}"
        );
        assert!(err(&path).contains("not a bare executable name"));
        let cargo = format!("[x]\ncargo = \"x\"\nversion = \"1.0.0\"\nbinary = \"x\"\n{USED}");
        assert!(err(&cargo).contains("binary applies to distro and aosp rows only"));
        let args = format!("[x]\ndebian = \"x\"\nversion_arg = \"version\"\n{USED}");
        assert!(err(&args).contains("version_arg needs binary"));
    }

    #[test]
    fn aosp_rows() {
        let ok = format!(
            "[jdk]\naosp = \"prebuilts/jdk/jdk21\"\nbinary = \"linux-x86/bin/javac\"\n{USED}"
        );
        assert!(parse(&ok, &consumers()).is_ok());
        let unknown = format!("[jdk]\naosp = \"prebuilts/gone\"\n{USED}");
        assert!(err(&unknown).contains("not a [aosp.projects] path"));
        let pinned = format!(
            "[jdk]\naosp = \"prebuilts/jdk/jdk21\"\nmin = \"21.0.4\"\nmin_reason = \"r\"\n{USED}"
        );
        assert!(err(&pinned).contains("the pin is the version"));
        let escaping =
            format!("[jdk]\naosp = \"prebuilts/jdk/jdk21\"\nbinary = \"../javac\"\n{USED}");
        assert!(err(&escaping).contains("not a path inside the project"));
    }

    #[test]
    fn upstream_version_strips_epoch_revision_suffix() {
        // Debian: [epoch:]upstream[-revision]; Arch: upstream-pkgrel
        assert_eq!(upstream_version("1:2.47.3-0+deb13u1"), "2.47.3");
        assert_eq!(upstream_version("4.18.6+ds-2"), "4.18.6");
        assert_eq!(upstream_version("2.47.1-1"), "2.47.1");
        assert_eq!(upstream_version("1.12.1"), "1.12.1");
        // glibc row: Debian libc6-dev; Arch glibc carries a git snapshot suffix
        assert_eq!(upstream_version("2.41-12+deb13u4"), "2.41");
        assert_eq!(upstream_version("2.41+r48+g5cb575ca9a3d-1"), "2.41");
    }

    #[test]
    fn version_order() {
        let v = |s| parse_version(s).unwrap();
        assert!(version_at_least(&v("4.18.6"), &v("4.18")));
        assert!(version_at_least(&v("4.18"), &v("4.18.0")));
        assert!(!version_at_least(&v("4.9"), &v("4.18")));
        assert!(parse_version("4.18-rc1").is_none());
    }

    #[test]
    fn first_version_of_each_tools_output() {
        // first line of each tool's version output on the build host, 2026-11-07
        for (line, want) in [
            ("Debian clang version 19.1.7 (3+b1)", "19.1.7"),
            ("Debian LLD 19.1.7 (compatible with GNU linkers)", "19.1.7"),
            ("go version go1.24.4 linux/amd64", "1.24.4"),
            ("cmake version 3.31.6", "3.31.6"),
            ("Python 3.13.5", "3.13.5"),
            ("bison (GNU Bison) 3.8.2", "3.8.2"),
            ("bubblewrap 0.12.0", "0.12.0"),
            ("unshare from util-linux 2.41.5", "2.41.5"),
            ("sccache 0.10.0", "0.10.0"),
        ] {
            assert_eq!(first_version(line), Some(want), "{line}");
        }
        assert_eq!(first_version("no version in 2026-11-07"), None);
    }

    #[test]
    fn binary_problem_names_the_row() {
        let missing = binary_problem("atl-no-such-binary", "--version", "1.0")
            .unwrap()
            .unwrap();
        assert_eq!(missing, "`atl-no-such-binary` not on PATH");
        // git is on every build host (the repo needs it); no real git meets 999.0
        let low = binary_problem("git", "--version", "999.0")
            .unwrap()
            .unwrap();
        assert!(
            low.starts_with("`git` is ") && low.ends_with("needs at least 999.0"),
            "{low}"
        );
        assert_eq!(binary_problem("git", "--version", "0.1").unwrap(), None);
    }

    #[test]
    fn aosp_problem_checks_project_then_binary() {
        let tree = std::env::temp_dir().join(format!("xtask-deps-aosp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        let problem = aosp_problem(&tree, "prebuilts/r8", Some("d8")).unwrap();
        assert!(problem.contains("fetch --execute"), "{problem}");
        std::fs::create_dir_all(tree.join("prebuilts/r8")).unwrap();
        let problem = aosp_problem(&tree, "prebuilts/r8", Some("d8")).unwrap();
        assert!(
            problem.contains("missing from the pinned tree"),
            "{problem}"
        );
        std::fs::write(tree.join("prebuilts/r8/d8"), "").unwrap();
        assert_eq!(aosp_problem(&tree, "prebuilts/r8", Some("d8")), None);
        assert_eq!(aosp_problem(&tree, "prebuilts/r8", None), None);
        std::fs::remove_dir_all(&tree).unwrap();
    }

    #[test]
    fn install_list() {
        let text = "cargo-deny v0.20.2:\n    cargo-deny\nfoo v1.0.0 (/src/foo):\n    foo\ncargo-c v0.10.26+cargo-0.100.0:\n    cargo-cbuild\n";
        let list = parse_install_list(text);
        assert_eq!(list["cargo-deny"], "0.20.2");
        assert_eq!(list["foo"], "1.0.0");
        // SemVer build metadata is not part of the version (cargo-c names its cargo with it)
        assert_eq!(list["cargo-c"], "0.10.26");
    }
}
