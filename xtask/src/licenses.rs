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

    fn get(&self) -> Result<&crate::aosp::Index, &str> {
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

/// An AOSP project's default from its `license` modules' kinds: exactly one kind, naming a valid SPDX id. Owner decision 2026-10-31: several kinds do not say which file has which, so those files stay unresolved (a `paths` entry each).
pub fn aosp_default(project: &Path, kinds: &BTreeSet<String>) -> Unlabelled {
    let p = project.display();
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
    /// Licence of the notice-less files of a checkout keyed `key`, checked out at `dir`; `project` = its AOSP tree path (empty for the AOSP tree root and other upstreams).
    /// An AOSP project's is derived from its Android.bp (`aosp_default`) and the AOSP module index its licence references resolve through, never stored; another upstream's is its `unlabelled`.
    pub fn unlabelled(&self, key: &str, project: &Path, dir: &Path) -> Result<Unlabelled> {
        if key == AOSP {
            if project.as_os_str().is_empty() {
                return Ok(Unlabelled::Missing("it is in no AOSP project".to_owned()));
            }
            let index = match self.aosp_index.get() {
                Ok(i) => i,
                Err(why) => return Ok(Unlabelled::Missing(why.to_owned())),
            };
            let kinds = crate::aosp::license_kinds(dir, project, index)
                .with_context(|| format!("reading the Android.bp files of {}", dir.display()))?;
            return Ok(match kinds {
                Ok(kinds) => aosp_default(project, &kinds),
                Err(why) => Unlabelled::Missing(format!("{}: {why}", project.display())),
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
    licenses.aosp_index.file = Some(crate::aosp::index_file(
        root,
        &sources::load(root)?.aosp,
    )?);
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
            let licence =
                parse_licence(unlabelled).with_context(|| format!("[upstream.{name}]"))?;
            ensure!(
                licence.exception.is_none(),
                "[upstream.{name}]: unlabelled takes a plain id"
            );
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
        let text = "[artifact.runtime]\nallow = [\"GPL-3.0-or-later WITH Autoconf-exception-3.0\"]\n";
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
        let text = "[artifact.runtime]\nallow = [\"FTL\", \"GPL-2.0-only WITH Linux-syscall-note\"]\n";
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
    fn aosp_kinds_read_from_the_project_blueprints() {
        let tmp = std::env::temp_dir().join(format!("xtask-licence-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let nested = tmp.join("nested");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        let license = |kind: &str| {
            format!(
                "license {{\n    name: \"l\",\n    license_kinds: [\n        \"{kind}\",\n    ],\n}}\n"
            )
        };
        let mit = "SPDX-license-identifier-MIT";
        std::fs::write(
            tmp.join("Android.bp"),
            format!("build = [\"more.bp\"]\n{}", license(mit)),
        )
        .unwrap();
        std::fs::write(tmp.join("more.bp"), license(mit)).unwrap();
        std::fs::write(
            tmp.join("sub/Android.bp"),
            "cc_library { name: \"x\", srcs: [\"x.c\"] }\n",
        )
        .unwrap();
        // a nested project is its own: its kinds are not this project's
        std::fs::write(
            nested.join("Android.bp"),
            license("SPDX-license-identifier-GPL-2.0"),
        )
        .unwrap();
        let apache = "SPDX-license-identifier-Apache-2.0";
        let mut licenses = parse(RUNTIME_MIT, &upstreams()).unwrap();
        // build/soong/licenses' shared module; a second `l` elsewhere is not the one external/x means
        licenses.aosp_index = AospIndex::of(crate::aosp::Index::of_licenses(&[
            ("Android-Apache-2.0", "build/soong/licenses/Android.bp", &[apache]),
            ("l", "external/x/Android.bp", &[mit]),
            ("l", "external/y/Android.bp", &[apache]),
        ]));
        let x = Path::new("external/x");
        let kinds = |licenses: &Licenses| licenses.unlabelled(AOSP, x, &tmp).unwrap();
        let got = kinds(&licenses);
        // a `build = [...]` file with a second kind makes the project multi-kind
        std::fs::write(tmp.join("more.bp"), license(apache)).unwrap();
        let two = kinds(&licenses);
        // a module's `licenses` naming a shared module adds that module's kinds
        std::fs::write(tmp.join("more.bp"), "").unwrap();
        let sub = "cc_library { name: \"x\", licenses: [\"Android-Apache-2.0\"] }\n";
        std::fs::write(tmp.join("sub/Android.bp"), sub).unwrap();
        let shared = kinds(&licenses);
        // system/server_configurable_flags' shape: no license module, only the shared default
        std::fs::write(tmp.join("sub/Android.bp"), "").unwrap();
        let only_shared = "package {\n    default_applicable_licenses: [\"Android-Apache-2.0\"],\n}\n";
        std::fs::write(tmp.join("Android.bp"), only_shared).unwrap();
        let referenced = kinds(&licenses);
        // `l` from external/x resolves to external/x's, the nearest enclosing definition
        let own = "package {\n    default_applicable_licenses: [\"l\"],\n}\n";
        std::fs::write(tmp.join("sub/Android.bp"), own).unwrap();
        let nearest = kinds(&licenses);
        std::fs::write(tmp.join("sub/Android.bp"), own.replace("\"l\"", "\"gone\"")).unwrap();
        let dangling = kinds(&licenses);
        let root = licenses.unlabelled(AOSP, Path::new(""), &tmp);
        // without an index, references cannot be followed
        let unindexed = parse(RUNTIME_MIT, &upstreams()).unwrap();
        let no_index = kinds(&unindexed);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(got, Unlabelled::Licence("MIT".into()));
        assert!(matches!(two, Unlabelled::Missing(m) if m.contains("several")));
        assert!(matches!(shared, Unlabelled::Missing(m) if m.contains("several")));
        assert_eq!(referenced, Unlabelled::Licence("Apache-2.0".into()));
        assert!(matches!(nearest, Unlabelled::Missing(m) if m.contains("several")));
        assert!(
            matches!(&dangling, Unlabelled::Missing(m) if m.contains("licence gone (from external/x/sub): no license module")),
            "{dangling:?}"
        );
        assert!(matches!(root.unwrap(), Unlabelled::Missing(m) if m.contains("no AOSP project")));
        assert!(matches!(no_index, Unlabelled::Missing(m) if m.contains("no AOSP module index")));
        // other upstreams: their `unlabelled`, or none
        let qemu = format!("{RUNTIME_MIT}[upstream.qemu]\nunlabelled = \"GPL-2.0-or-later\"\n");
        let licenses = parse(&qemu, &upstreams()).unwrap();
        let none = Path::new("");
        assert_eq!(
            licenses.unlabelled("qemu", none, none).unwrap(),
            Unlabelled::Licence("GPL-2.0-or-later".into())
        );
        assert!(matches!(
            licenses.unlabelled("boringssl", none, none).unwrap(),
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
