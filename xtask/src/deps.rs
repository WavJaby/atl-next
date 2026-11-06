// SPDX-License-Identifier: MIT
//! `deps.toml` (what must be installed outside the repo) and `xtask deps`. Column meanings live in that
//! file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use serde::Deserialize;

use crate::sources::{self, Sources};
use crate::tree;
use crate::workspace::{self, Workspace};

pub const FILE: &str = "deps.toml";

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
}

impl Consumers {
    pub fn new(workspace: &Workspace, sources: &Sources) -> Self {
        Self {
            crates: workspace.crates.clone(),
            upstreams: sources.upstream_names(),
            xtask: crate::subcommands(),
            projects: sources.aosp.projects.keys().cloned().collect(),
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
        validate(dep, consumers).with_context(|| format!("[{name}]"))?;
        ensure!(
            sources::is_name(name),
            "[{name}]: name must be [a-z0-9-], starting alphanumeric"
        );
    }
    Ok(deps)
}

fn validate(dep: &Dep, consumers: &Consumers) -> Result<()> {
    let distro = dep.debian.is_some() || dep.arch.is_some();
    let cargo = dep.cargo.is_some() || dep.version.is_some();
    let rustup = dep.rustup_target.is_some();
    let aosp = dep.aosp.is_some();
    ensure!(
        [distro, cargo, rustup, aosp].iter().filter(|s| **s).count() == 1,
        "exactly one source: debian/arch, cargo + version, rustup_target, or aosp"
    );
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
        consumers.resolve(reference)?;
    }
    Ok(())
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

/// Check this host against deps.toml. Read-only.
#[derive(clap::Args)]
pub struct Args {
    /// Verify every row is installed at its required version.
    #[arg(long, required = true)]
    check: bool,
}

#[derive(Clone, Copy)]
enum Distro {
    Debian,
    Arch,
}

pub fn run(root: &Path, _args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let deps = load(root, &Consumers::new(&workspace::load(root)?, &sources))?;
    let distro = host_distro()?;
    let cargo_installed = cargo_installed()?;
    let rustup_targets = rustup_targets(root)?;
    let tree = tree::shared_tree(&Layout::of(root)?);
    let mut problems = Vec::new();
    for (name, dep) in &deps {
        if let Some(problem) = check_row(dep, distro, &cargo_installed, &rustup_targets, &tree)? {
            problems.push(format!("{name}: {problem}"));
        }
    }
    for p in &problems {
        println!("deps: {p}");
    }
    if problems.is_empty() {
        println!("deps: ok ({} rows)", deps.len());
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "deps: {} of {} rows not satisfied",
            problems.len(),
            deps.len()
        );
        Ok(ExitCode::FAILURE)
    }
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
    let out = crate::cargo_command()
        .args(["install", "--list"])
        .output()
        .context("running cargo install --list")?;
    ensure!(
        out.status.success(),
        "cargo install --list: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(parse_install_list(&String::from_utf8_lossy(&out.stdout)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn consumers() -> Consumers {
        let set = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        Consumers {
            crates: set(&["xtask"]),
            upstreams: set(&["aosp"]),
            xtask: set(&["check"]),
            projects: set(&["prebuilts/jdk/jdk21", "prebuilts/r8"]),
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
