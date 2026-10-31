// SPDX-License-Identifier: MIT
//! `deps.toml` (what must be installed outside the repo) and `xtask deps`. Column meanings live in that
//! file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::sources::{self, Sources};
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
}

impl Consumers {
    pub fn new(workspace: &Workspace, sources: &Sources) -> Self {
        Self {
            crates: workspace.crates.clone(),
            upstreams: sources.upstream_names(),
            xtask: crate::subcommands(),
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
    ensure!(
        distro != cargo,
        "exactly one source: debian/arch, or cargo + version"
    );
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
    let mut problems = Vec::new();
    for (name, dep) in &deps {
        if let Some(problem) = check_row(dep, distro, &cargo_installed)? {
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
) -> Result<Option<String>> {
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
    fn install_list() {
        let text = "cargo-deny v0.20.2:\n    cargo-deny\nfoo v1.0.0 (/src/foo):\n    foo\ncargo-c v0.10.26+cargo-0.100.0:\n    cargo-cbuild\n";
        let list = parse_install_list(text);
        assert_eq!(list["cargo-deny"], "0.20.2");
        assert_eq!(list["foo"], "1.0.0");
        // SemVer build metadata is not part of the version (cargo-c names its cargo with it)
        assert_eq!(list["cargo-c"], "0.10.26");
    }
}
