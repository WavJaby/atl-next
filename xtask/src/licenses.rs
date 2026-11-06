// SPDX-License-Identifier: MIT
//! `licenses.toml`: the licence policy per shipped artifact. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::sources;

pub const FILE: &str = "licenses.toml";
pub const RUNTIME: &str = "runtime";
/// The licence key every AOSP project shares.
pub const AOSP: &str = "aosp";
/// The GPL-3.0-or-later QEMU bridge; its allow list = licences combinable with GPL-3.
const BRIDGE: &str = "qemu-bridge";
/// The exceptions under which GPL-2.0-only code may enter a process: each lets code of any licence use it.
/// Classpath: linking with independent modules (libcore ojluni, libjdwp; owner 2026-10-29). Linux-syscall-note: programs using the kernel through system calls, i.e. the kernel uapi headers (owner 2026-10-31).
const GPL2_LINKING_EXCEPTIONS: &[&str] = &["Classpath-exception-2.0", "Linux-syscall-note"];
/// The exceptions under which GPL-3 code may enter the runtime: each frees the works using it from copyleft (libgcc_s, Bison parsers).
const GPL3_FREEING_EXCEPTIONS: &[&str] = &["GCC-exception-3.1", "Bison-exception-2.2"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Licenses {
    pub artifact: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub upstream: BTreeMap<String, Upstream>,
    /// Android.bp test modules never shipped (module name -> row): their device artifacts are outside every `[artifact.*]`.
    #[serde(default)]
    pub test_only: BTreeMap<String, TestOnly>,
    #[serde(skip)]
    pub(crate) aosp_index: AospIndex,
}

/// The AOSP module index an AOSP project's licence references resolve through; read on first use (~10 MB, and most commands never need it).
#[derive(Debug, Default)]
pub(crate) struct AospIndex {
    file: Option<PathBuf>,
    loaded: OnceLock<Result<crate::aosp::Index, String>>,
}

impl AospIndex {
    #[cfg(test)]
    pub(crate) fn of(index: crate::aosp::Index) -> Self {
        AospIndex {
            file: None,
            loaded: OnceLock::from(Ok(index)),
        }
    }

    pub(crate) fn get(&self) -> Result<&crate::aosp::Index, &str> {
        self.loaded
            .get_or_init(|| {
                let file = self.file.as_ref().ok_or("no AOSP module index is known")?;
                crate::aosp::Index::read(file).map_err(|e| {
                    format!("the AOSP module index its licence references resolve through: {e:#}")
                })
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestOnly {
    /// What the program tests, and why it never ships.
    pub reason: String,
}

/// Soong module types of test programs: what a `[test_only]` row may name (a library could be linked into a shipped artifact).
const TEST_MODULE_TYPES: &[&str] = &["cc_test", "cc_benchmark"];

/// Each `[test_only]` module's definitions in the AOSP module index: at least one, all of a test program type.
pub fn check_test_only(licenses: &Licenses) -> Result<()> {
    if licenses.test_only.is_empty() {
        return Ok(());
    }
    let index = licenses.aosp_index.get().map_err(|e| anyhow::anyhow!("{e}"))?;
    for module in licenses.test_only.keys() {
        let defs = index.definitions_of(module);
        ensure!(
            !defs.is_empty(),
            "[test_only.{module}]: no such module in the AOSP tree"
        );
        for (ty, file) in defs {
            ensure!(
                TEST_MODULE_TYPES.contains(&ty),
                "[test_only.{module}]: {file} defines it as {ty}, not a test program ({TEST_MODULE_TYPES:?})"
            );
        }
    }
    Ok(())
}

/// The device artifacts of `[test_only]` modules in the converter build `out` (its graph.json `artifacts."device/<module>".out`); modules this build does not make are absent.
pub fn test_only_outputs(licenses: &Licenses, out: &Path) -> Result<BTreeSet<PathBuf>> {
    let file = out.join("graph.json");
    let graph: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?,
    )
    .with_context(|| file.display().to_string())?;
    Ok(licenses
        .test_only
        .keys()
        .filter_map(|m| graph["artifacts"][format!("device/{m}")]["out"].as_str())
        .map(PathBuf::from)
        .collect())
}

/// Refuses host paths bound into a runtime root (`staged`: files, library dirs) that are, lie in, or contain the build dir of a test-only artifact: test programs never ship.
pub fn refuse_test_only(staged: &[&Path], test_only: &BTreeSet<PathBuf>) -> Result<()> {
    for s in staged {
        for t in test_only {
            let dir = t.parent().unwrap_or(t);
            ensure!(
                !s.starts_with(dir) && !dir.starts_with(s),
                "{}: a test-only output ([test_only] in {FILE}) staged into a runtime root",
                s.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Licence of files with no licence notice at all.
    pub unlabelled: Option<String>,
    /// Path prefix (relative to the upstream root) -> licence, for notices the rules cannot read.
    #[serde(default)]
    pub paths: BTreeMap<String, PathLicence>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathLicence {
    /// SPDX expression.
    pub licence: String,
    pub reason: String,
}

/// Licence of a checkout's files that carry no notice, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Unlabelled {
    Licence(String),
    Missing(String),
}

/// Prefix of the AOSP `license_kind` modules (build/soong/licenses/Android.bp) that name an SPDX id.
const SPDX_KIND: &str = "SPDX-license-identifier-";

/// An AOSP package's default from its licence kinds (`aosp::package_kinds`): exactly one kind, naming a valid SPDX id. Owner decision 2026-10-31: several kinds do not say which file has which, so those files stay unresolved (a `paths` entry each).
pub fn aosp_default(package: &Path, kinds: &BTreeSet<String>) -> Unlabelled {
    let p = package.display();
    let mut each = kinds.iter();
    let (Some(kind), None) = (each.next(), each.next()) else {
        return Unlabelled::Missing(if kinds.is_empty() {
            format!("{p}'s Android.bp declares no licence kind")
        } else {
            format!(
                "{p}'s Android.bp declares several licence kinds ({}), not which file has which",
                kinds.len()
            )
        });
    };
    match kind
        .strip_prefix(SPDX_KIND)
        .and_then(|id| crate::notice::normalize_tag(id).ok())
    {
        Some(id) => Unlabelled::Licence(id),
        None => Unlabelled::Missing(format!(
            "{p}'s Android.bp licence kind {kind} is no SPDX id"
        )),
    }
}

impl Licenses {
    /// Licence of the notice-less files in directory `at` of a checkout keyed `key`, checked out at `dir`; `project` = its AOSP tree path (empty for the AOSP tree root and other upstreams), `at` relative to `dir`.
    /// An AOSP file's is derived from the Android.bp of its Soong package (`aosp::package_kinds`, `aosp_default`) and the AOSP module index its licence references resolve through, never stored; another upstream's is its `unlabelled`, whatever `at`.
    pub fn unlabelled(
        &self,
        key: &str,
        project: &Path,
        dir: &Path,
        at: &Path,
    ) -> Result<Unlabelled> {
        if key == AOSP {
            if project.as_os_str().is_empty() {
                return Ok(Unlabelled::Missing("it is in no AOSP project".to_owned()));
            }
            let index = match self.aosp_index.get() {
                Ok(i) => i,
                Err(why) => return Ok(Unlabelled::Missing(why.to_owned())),
            };
            let kinds = crate::aosp::package_kinds(dir, project, at, index)
                .with_context(|| format!("reading the Android.bp files of {}", dir.display()))?;
            return Ok(match kinds {
                Ok((package, kinds)) => aosp_default(&package, &kinds),
                Err(why) => Unlabelled::Missing(why),
            });
        }
        Ok(
            match self.upstream.get(key).and_then(|u| u.unlabelled.clone()) {
                Some(l) => Unlabelled::Licence(l),
                None => {
                    Unlabelled::Missing(format!("[upstream.{key}] has no `unlabelled` licence"))
                }
            },
        )
    }

    /// Whether a file under SPDX expression `expr` may be compiled into `artifact`: `OR` needs one allowed side, `AND` both, and each licence (`<id>` or `<id> WITH <exception>`) must be an allow entry as written.
    pub fn allows(&self, artifact: &str, expr: &str) -> Result<bool> {
        let artifact = self
            .artifact
            .get(artifact)
            .with_context(|| format!("no [artifact.{artifact}]"))?;
        let expr = ::spdx::Expression::parse(expr)
            .with_context(|| format!("`{expr}` is not an SPDX expression"))?;
        Ok(expr.evaluate(|req| artifact.allow.iter().any(|a| *a == req.to_string())))
    }
}

/// One allow entry: an SPDX licence, optionally `WITH` an exception.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Licence<'a> {
    id: &'a str,
    exception: Option<&'a str>,
}

fn parse_licence(entry: &str) -> Result<Licence<'_>> {
    let (id, exception) = match entry.split_once(" WITH ") {
        Some((id, exc)) => (id.trim(), Some(exc.trim())),
        None => (entry.trim(), None),
    };
    if let Some(name) = id.strip_prefix("LicenseRef-") {
        // SPDX spec annex: `LicenseRef-` idstring, letters, digits, `.` and `-`; for a licence the SPDX list lacks
        ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "`{id}` is not a valid LicenseRef"
        );
    } else {
        let Some(license) = spdx::license_id(id) else {
            bail!("`{id}` is not an SPDX licence id")
        };
        ensure!(!license.is_deprecated(), "`{id}` is a deprecated SPDX id");
    }
    if let Some(exc) = exception {
        ensure!(
            spdx::exception_id(exc).is_some(),
            "`{exc}` is not an SPDX exception id"
        );
    }
    Ok(Licence { id, exception })
}

pub fn load(root: &Path, upstreams: &BTreeSet<String>) -> Result<Licenses> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let mut licenses = parse(&text, upstreams).with_context(|| format!("in {FILE}"))?;
    let deny = std::fs::read_to_string(root.join("deny.toml")).context("reading deny.toml")?;
    check_deny(&licenses, &deny).context("deny.toml against licenses.toml")?;
    licenses.aosp_index.file = Some(crate::aosp::index_file(root, &sources::load(root)?.aosp)?);
    Ok(licenses)
}

pub(crate) fn parse(text: &str, upstreams: &BTreeSet<String>) -> Result<Licenses> {
    let licenses: Licenses = toml::from_str(text)?;
    ensure!(
        licenses.artifact.contains_key(RUNTIME),
        "no [artifact.{RUNTIME}]"
    );
    for (name, artifact) in &licenses.artifact {
        ensure!(
            sources::is_name(name),
            "[artifact.{name}]: name must be [a-z0-9-]"
        );
        let mut seen = BTreeSet::new();
        for entry in &artifact.allow {
            let licence = parse_licence(entry).with_context(|| format!("[artifact.{name}]"))?;
            // CLAUDE.md licence rules: GPL-2.0-only only with a linking exception; GPL-3 in the
            // runtime only with an exception that frees its users
            if licence.id == "GPL-2.0-only" {
                ensure!(
                    licence
                        .exception
                        .is_some_and(|e| GPL2_LINKING_EXCEPTIONS.contains(&e)),
                    "[artifact.{name}]: `{entry}`; GPL-2.0-only only `WITH` one of {GPL2_LINKING_EXCEPTIONS:?}"
                );
            }
            let gpl3 = licence.id.starts_with("GPL-3.0") || licence.id.starts_with("AGPL-3.0");
            ensure!(
                !(name == RUNTIME
                    && gpl3
                    && !licence
                        .exception
                        .is_some_and(|e| GPL3_FREEING_EXCEPTIONS.contains(&e))),
                "[artifact.{RUNTIME}]: `{entry}`; GPL-3 only `WITH` one of {GPL3_FREEING_EXCEPTIONS:?}"
            );
            ensure!(
                seen.insert(licence),
                "[artifact.{name}]: `{entry}` listed twice"
            );
        }
    }
    for (name, upstream) in &licenses.upstream {
        ensure!(
            upstreams.contains(name),
            "[upstream.{name}]: not a sources.toml upstream"
        );
        if let Some(unlabelled) = &upstream.unlabelled {
            ensure!(
                name != AOSP,
                "[upstream.{AOSP}]: no `unlabelled`; each project's comes from its Android.bp license modules"
            );
            // one licence, `<id>` or `<id> WITH <exception>` (llvm-project's LICENSE.TXT is Apache-2.0 WITH LLVM-exception)
            parse_licence(unlabelled).with_context(|| format!("[upstream.{name}]"))?;
        }
        for (prefix, entry) in &upstream.paths {
            let at = format!("[upstream.{name}.paths.\"{prefix}\"]");
            ensure!(
                !prefix.is_empty()
                    && !prefix.starts_with('/')
                    && !prefix.split('/').any(|c| c == ".."),
                "{at}: not a relative path prefix"
            );
            ::spdx::Expression::parse(&entry.licence)
                .with_context(|| format!("{at}: bad SPDX expression `{}`", entry.licence))?;
            ensure!(!entry.reason.trim().is_empty(), "{at}: empty reason");
        }
    }
    for (module, row) in &licenses.test_only {
        ensure!(
            !row.reason.trim().is_empty(),
            "[test_only.{module}]: empty reason"
        );
    }
    check_bridge_superset(&licenses)?;
    Ok(licenses)
}

/// Runtime code shares a process with the GPL-3 bridge, so every runtime licence must also be one
/// the bridge may be combined with. Active once the bridge artifact exists.
fn check_bridge_superset(licenses: &Licenses) -> Result<()> {
    let Some(bridge) = licenses.artifact.get(BRIDGE) else {
        return Ok(());
    };
    let bridge: BTreeSet<Licence> = bridge
        .allow
        .iter()
        .map(|e| parse_licence(e))
        .collect::<Result<_>>()?;
    for entry in &licenses.artifact[RUNTIME].allow {
        ensure!(
            bridge.contains(&parse_licence(entry)?),
            "[artifact.{RUNTIME}] `{entry}` is not in [artifact.{BRIDGE}]: everything in a \
             bridge-loaded process must be combinable with GPL-3"
        );
    }
    Ok(())
}

/// Crates link into the runtime, so cargo-deny may allow nothing the runtime does not.
fn check_deny(licenses: &Licenses, deny_text: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Deny {
        licenses: DenyLicenses,
    }
    #[derive(Deserialize)]
    struct DenyLicenses {
        allow: Vec<String>,
    }
    let deny: Deny = toml::from_str(deny_text)?;
    let runtime: BTreeSet<Licence> = licenses.artifact[RUNTIME]
        .allow
        .iter()
        .map(|e| parse_licence(e))
        .collect::<Result<_>>()?;
    for entry in &deny.licenses.allow {
        ensure!(
            runtime.contains(&parse_licence(entry)?),
            "crate licence `{entry}` is not allowed in [artifact.{RUNTIME}]"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstreams() -> BTreeSet<String> {
        ["aosp".to_owned(), "qemu".to_owned()].into()
    }

    fn err(text: &str) -> String {
        format!(
            "{:#}",
            parse(text, &upstreams()).expect_err("should be rejected")
        )
    }

    const RUNTIME_MIT: &str = "[artifact.runtime]\nallow = [\"MIT\"]\n";

    /// A row must name a test program the tree defines; a library (could be linked into a shipped artifact) or an unknown name fails.
    #[test]
    fn test_only_rows_checked_against_the_index() {
        let with = |module: &str| {
            let mut l = parse(
                &format!("{RUNTIME_MIT}[test_only.{module}]\nreason = \"conformance runner\"\n"),
                &upstreams(),
            )
            .unwrap();
            l.aosp_index = AospIndex::of(crate::aosp::Index::of_modules(&[
                ("deqp-binary", "cc_test", "external/deqp/Android.bp"),
                ("libdeqp", "cc_library_static", "external/deqp/Android.bp"),
            ]));
            check_test_only(&l).map_err(|e| format!("{e:#}"))
        };
        assert_eq!(with("deqp-binary"), Ok(()));
        assert!(with("libdeqp").unwrap_err().contains("not a test program"));
        assert!(with("nosuch").unwrap_err().contains("no such module"));
        assert!(
            err(&format!("{RUNTIME_MIT}[test_only.x]\nreason = \" \"\n")).contains("empty reason")
        );
    }

    /// The staging guard fires on the artifact, a file beside it and a dir holding it; an unrelated path passes.
    #[test]
    fn test_only_outputs_refused_in_a_runtime_root() {
        let t: BTreeSet<PathBuf> = [PathBuf::from("/o/device/deqp-binary/deqp-binary64")].into();
        for bad in [
            "/o/device/deqp-binary/deqp-binary64",
            "/o/device/deqp-binary/data/x",
            "/o/device",
        ] {
            let e = refuse_test_only(&[Path::new(bad)], &t).unwrap_err();
            assert!(format!("{e}").contains("test-only output"), "{bad}");
        }
        assert!(refuse_test_only(&[Path::new("/o/device/libui/libui.so")], &t).is_ok());
    }

    #[test]
    fn runtime_bridge_and_unlabelled() {
        let text = "[artifact.runtime]\nallow = [\"MIT\", \"GPL-2.0-only WITH Classpath-exception-2.0\"]\n\
                    [artifact.qemu-bridge]\nallow = [\"MIT\", \"GPL-2.0-only WITH Classpath-exception-2.0\", \
                    \"GPL-2.0-or-later\", \"GPL-3.0-or-later\"]\n\
                    [upstream.qemu]\nunlabelled = \"GPL-2.0-or-later\"\n";
        let licenses = parse(text, &upstreams()).unwrap();
        assert_eq!(
            licenses.upstream["qemu"].unlabelled.as_deref(),
            Some("GPL-2.0-or-later")
        );
    }

    #[test]
    fn runtime_required() {
        assert!(
            err("[artifact.qemu-bridge]\nallow = [\"MIT\"]\n").contains("no [artifact.runtime]")
        );
    }

    #[test]
    fn ids_checked_against_spdx_list() {
        assert!(err("[artifact.runtime]\nallow = [\"MITT\"]\n").contains("not an SPDX licence id"));
        // `GPL-2.0` is the deprecated spelling of GPL-2.0-only
        assert!(err("[artifact.runtime]\nallow = [\"GPL-2.0\"]\n").contains("deprecated"));
        let text = "[artifact.runtime]\nallow = [\"MIT WITH No-Such-exception\"]\n";
        assert!(err(text).contains("not an SPDX exception id"));
        // a licence the SPDX list lacks is a LicenseRef, by the spec's idstring rule
        let text = "[artifact.runtime]\nallow = [\"LicenseRef-Public-Domain\"]\n";
        assert!(parse(text, &upstreams()).is_ok());
        let text = "[artifact.runtime]\nallow = [\"LicenseRef-Public Domain\"]\n";
        assert!(err(text).contains("not a valid LicenseRef"));
        assert!(err("[artifact.runtime]\nallow = [\"LicenseRef-\"]\n").contains("not a valid"));
    }

    #[test]
    fn licence_rules() {
        let bare = "[artifact.qemu-bridge]\nallow = [\"GPL-2.0-only\"]\n";
        assert!(err(&format!("{RUNTIME_MIT}{bare}")).contains("GPL-2.0-only only `WITH`"));
        // the kernel uapi headers' and libcore's forms
        let text = "[artifact.runtime]\nallow = [\"GPL-2.0-only WITH Linux-syscall-note\", \
                    \"GPL-2.0-only WITH Classpath-exception-2.0\"]\n";
        assert!(parse(text, &upstreams()).is_ok());
        // a real SPDX exception that is no linking exception: it only frees configure scripts
        let text = "[artifact.runtime]\nallow = [\"GPL-2.0-only WITH Autoconf-exception-2.0\"]\n";
        assert!(err(text).contains("GPL-2.0-only only `WITH`"));
        let gpl3 = "GPL-3 only `WITH`";
        assert!(err("[artifact.runtime]\nallow = [\"GPL-3.0-or-later\"]\n").contains(gpl3));
        assert!(err("[artifact.runtime]\nallow = [\"AGPL-3.0-only\"]\n").contains(gpl3));
        let text =
            "[artifact.runtime]\nallow = [\"GPL-3.0-or-later WITH Autoconf-exception-3.0\"]\n";
        assert!(err(text).contains(gpl3));
        // libgcc_s and Bison parsers: GPL-3 with an exception that frees their users
        let text = "[artifact.runtime]\nallow = [\"GPL-3.0-or-later WITH GCC-exception-3.1\", \
                    \"GPL-3.0-or-later WITH Bison-exception-2.2\"]\n";
        assert!(parse(text, &upstreams()).is_ok());
    }

    #[test]
    fn runtime_within_bridge() {
        let text = "[artifact.runtime]\nallow = [\"MIT\", \"Apache-2.0\"]\n\
                    [artifact.qemu-bridge]\nallow = [\"MIT\", \"GPL-3.0-or-later\"]\n";
        assert!(err(text).contains("`Apache-2.0` is not in [artifact.qemu-bridge]"));
    }

    #[test]
    fn duplicates_and_unknown_upstream() {
        assert!(err("[artifact.runtime]\nallow = [\"MIT\", \"MIT\"]\n").contains("listed twice"));
        let text = format!("{RUNTIME_MIT}[upstream.gone]\nunlabelled = \"MIT\"\n");
        assert!(err(&text).contains("not a sources.toml upstream"));
    }

    #[test]
    fn path_entries() {
        let ok = format!(
            "{RUNTIME_MIT}[upstream.qemu.paths.\"linux-user/\"]\nlicence = \"GPL-2.0-or-later\"\nreason = \"r\"\n"
        );
        assert!(parse(&ok, &upstreams()).is_ok());
        assert!(err(&ok.replace("linux-user/", "../x")).contains("not a relative path prefix"));
        assert!(err(&ok.replace("reason = \"r\"", "reason = \" \"")).contains("empty reason"));
        assert!(err(&ok.replace("GPL-2.0-or-later", "GPL-2.0-or-later AND")).contains("bad SPDX"));
    }

    #[test]
    fn allowed_expressions() {
        let text = "[artifact.runtime]\nallow = [\"MIT\", \"GPL-2.0-only WITH Classpath-exception-2.0\", \"GPL-2.0-or-later\"]\n";
        let licenses = parse(text, &upstreams()).unwrap();
        let allows = |e: &str| licenses.allows(RUNTIME, e).unwrap();
        assert!(allows("MIT"));
        assert!(allows("GPL-2.0-or-later"));
        assert!(allows("GPL-2.0-only WITH Classpath-exception-2.0"));
        // the exception is what makes GPL-2.0-only allowed
        assert!(!allows("GPL-2.0-only"));
        assert!(!allows("Apache-2.0"));
        assert!(allows("Apache-2.0 OR MIT"));
        assert!(!allows("Apache-2.0 AND MIT"));
        // a licence the SPDX list lacks (a `paths` entry's LicenseRef) is denied unless listed
        assert!(!allows("LicenseRef-Adobe-DNG-SDK"));
        assert!(licenses.allows("qemu-bridge", "MIT").is_err());
        // FreeType's dual licence passes through its FTL arm alone; GPL-2.0-or-later is never needed
        let text =
            "[artifact.runtime]\nallow = [\"FTL\", \"GPL-2.0-only WITH Linux-syscall-note\"]\n";
        let licenses = parse(text, &upstreams()).unwrap();
        let allows = |e: &str| licenses.allows(RUNTIME, e).unwrap();
        assert!(allows("FTL OR GPL-2.0-or-later"));
        assert!(!allows("GPL-2.0-or-later"));
        assert!(allows("GPL-2.0-only WITH Linux-syscall-note"));
        assert!(!allows("GPL-2.0-only"));
    }

    #[test]
    fn aosp_unlabelled_is_derived_not_stored() {
        let text = format!("{RUNTIME_MIT}[upstream.aosp]\nunlabelled = \"Apache-2.0\"\n");
        assert!(err(&text).contains("comes from its Android.bp"));
    }

    #[test]
    fn aosp_default_needs_one_spdx_kind() {
        let p = Path::new("external/x");
        let d = |kinds: &[&str]| aosp_default(p, &kinds.iter().map(|k| (*k).to_owned()).collect());
        let missing = |kinds: &[&str], why: &str| match d(kinds) {
            Unlabelled::Missing(m) => assert!(m.contains(why), "{m}"),
            l => panic!("{kinds:?} gave {l:?}"),
        };
        assert_eq!(
            d(&["SPDX-license-identifier-Apache-2.0"]),
            Unlabelled::Licence("Apache-2.0".into())
        );
        // AOSP's kind uses the deprecated id; GPL-2.0 is GPL-2.0-only in the SPDX list
        assert_eq!(
            d(&["SPDX-license-identifier-GPL-2.0"]),
            Unlabelled::Licence("GPL-2.0-only".into())
        );
        missing(&[], "no licence kind");
        // external/skia's kinds (abridged): which file has which is not stated
        missing(
            &[
                "SPDX-license-identifier-Apache-2.0",
                "SPDX-license-identifier-MIT",
            ],
            "several licence kinds",
        );
        missing(&["legacy_notice"], "is no SPDX id");
        // AOSP kinds exist for unversioned families; the SPDX list has no such id
        missing(&["SPDX-license-identifier-BSD"], "is no SPDX id");
    }

    #[test]
    fn aosp_default_is_the_package_one() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::create_dir_all(tmp.join("crate/src")).unwrap();
        let license = |name: &str, kind: &str| {
            format!(
                "license {{\n    name: \"{name}\",\n    license_kinds: [\n        \"{kind}\",\n    ],\n}}\n"
            )
        };
        let mit = "SPDX-license-identifier-MIT";
        let apache = "SPDX-license-identifier-Apache-2.0";
        let write = |file: &str, text: &str| std::fs::write(tmp.join(file), text).unwrap();
        write(
            "Android.bp",
            &format!("build = [\"more.bp\"]\n{}", license("l", mit)),
        );
        write("more.bp", &license("l", mit));
        // a package declaring no licence: its files are the enclosing package's
        write(
            "sub/Android.bp",
            "cc_library { name: \"x\", srcs: [\"x.c\"] }\n",
        );
        // android-crates-io's shape: each crate a package with its own default
        let crate_bp = format!(
            "package {{\n    default_applicable_licenses: [\"crate_l\"],\n}}\n{}",
            license("crate_l", apache)
        );
        write("crate/Android.bp", &crate_bp);
        let mut licenses = parse(RUNTIME_MIT, &upstreams()).unwrap();
        // build/soong/licenses' shared module; a second `l` elsewhere is not the one external/x means
        licenses.aosp_index = AospIndex::of(crate::aosp::Index::of_licenses(&[
            (
                "Android-Apache-2.0",
                "build/soong/licenses/Android.bp",
                &[apache],
            ),
            ("l", "external/x/Android.bp", &[mit]),
            ("l", "external/y/Android.bp", &[apache]),
            ("crate_l", "external/x/crate/Android.bp", &[apache]),
        ]));
        let x = Path::new("external/x");
        let at = |licenses: &Licenses, dir: &str| {
            licenses.unlabelled(AOSP, x, &tmp, Path::new(dir)).unwrap()
        };
        let root = at(&licenses, "");
        let fallback = at(&licenses, "sub");
        let in_crate = at(&licenses, "crate/src");
        // a `build = [...]` file with a second kind makes the package multi-kind
        write("more.bp", &license("l2", apache));
        let two = at(&licenses, "");
        // a module's `licenses` naming a shared module: that module's kinds
        write("more.bp", "");
        write(
            "sub/Android.bp",
            "cc_library { name: \"x\", licenses: [\"Android-Apache-2.0\"] }\n",
        );
        let shared = at(&licenses, "sub");
        // system/server_configurable_flags' shape: no license module, only the shared default
        let only_shared =
            "package {\n    default_applicable_licenses: [\"Android-Apache-2.0\"],\n}\n";
        write("Android.bp", only_shared);
        let referenced = at(&licenses, "");
        // `l` from external/x/sub resolves to external/x's, the nearest enclosing definition
        let own = "package {\n    default_applicable_licenses: [\"l\"],\n}\n";
        write("sub/Android.bp", own);
        let nearest = at(&licenses, "sub");
        write("sub/Android.bp", &own.replace("\"l\"", "\"gone\""));
        let dangling = at(&licenses, "sub");
        let no_project = licenses.unlabelled(AOSP, Path::new(""), &tmp, Path::new(""));
        // without an index, references cannot be followed
        let unindexed = parse(RUNTIME_MIT, &upstreams()).unwrap();
        let no_index = at(&unindexed, "");
        // no Android.bp up to the project root declares a licence
        write("Android.bp", "");
        write("sub/Android.bp", "");
        let none = at(&licenses, "sub");
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(root, Unlabelled::Licence("MIT".into()));
        assert_eq!(fallback, Unlabelled::Licence("MIT".into()));
        assert_eq!(in_crate, Unlabelled::Licence("Apache-2.0".into()));
        assert!(matches!(two, Unlabelled::Missing(m) if m.contains("several")));
        assert_eq!(shared, Unlabelled::Licence("Apache-2.0".into()));
        assert_eq!(referenced, Unlabelled::Licence("Apache-2.0".into()));
        assert_eq!(nearest, Unlabelled::Licence("MIT".into()));
        assert!(
            matches!(&dangling, Unlabelled::Missing(m) if m.contains("licence gone (from external/x/sub): no license module")),
            "{dangling:?}"
        );
        assert!(
            matches!(no_project.unwrap(), Unlabelled::Missing(m) if m.contains("no AOSP project"))
        );
        assert!(matches!(no_index, Unlabelled::Missing(m) if m.contains("no AOSP module index")));
        assert!(
            matches!(&none, Unlabelled::Missing(m) if m.contains("no Android.bp from external/x/sub up to external/x")),
            "{none:?}"
        );
        // other upstreams: their `unlabelled` wherever the file is, or none
        let qemu = format!("{RUNTIME_MIT}[upstream.qemu]\nunlabelled = \"GPL-2.0-or-later\"\n");
        let licenses = parse(&qemu, &upstreams()).unwrap();
        let none = Path::new("");
        assert_eq!(
            licenses
                .unlabelled("qemu", none, none, Path::new("linux-user"))
                .unwrap(),
            Unlabelled::Licence("GPL-2.0-or-later".into())
        );
        assert!(matches!(
            licenses.unlabelled("boringssl", none, none, none).unwrap(),
            Unlabelled::Missing(m) if m.contains("no `unlabelled`")
        ));
    }

    #[test]
    fn deny_within_runtime() {
        let licenses = parse(RUNTIME_MIT, &upstreams()).unwrap();
        assert!(check_deny(&licenses, "[licenses]\nallow = [\"MIT\"]\n").is_ok());
        let e = format!(
            "{:#}",
            check_deny(&licenses, "[licenses]\nallow = [\"MIT\", \"ISC\"]\n").unwrap_err()
        );
        assert!(e.contains("`ISC` is not allowed"));
    }
}
