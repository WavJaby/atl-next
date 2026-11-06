// SPDX-License-Identifier: MIT
//! `providers.toml`: Android.bp modules provided by our crates, upstreams built by their own build system, or system libraries instead of AOSP source. Column meanings live in that file's header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::deps::Deps;
use crate::upstream;

pub const FILE: &str = "providers.toml";
/// Provider of a module nothing provides on this host: a dependency on it is dropped.
pub const NONE: &str = "none";

/// Modules whose dependencies the converter drops (`provider = "none"` for the whole module).
pub fn omitted(providers: &Providers) -> Vec<&str> {
    providers
        .iter()
        .filter(|(_, row)| row.is_none() && row.srcs.is_empty())
        .map(|(module, _)| module.as_str())
        .collect()
}

/// `(module, name)` for each row whose provider is `<kind>:<name>`.
fn of_kind<'a>(providers: &'a Providers, kind: &str) -> Vec<(&'a str, &'a str)> {
    providers
        .iter()
        .filter_map(|(module, row)| {
            let (k, name) = row.provider.as_deref()?.split_once(':')?;
            (k == kind).then_some((module.as_str(), name))
        })
        .collect()
}

/// Rows replacing only some sources of a module (`srcs`): (module, row).
pub fn partial(providers: &Providers) -> Vec<(&str, &Provider)> {
    providers
        .iter()
        .filter(|(_, row)| !row.srcs.is_empty())
        .map(|(module, row)| (module.as_str(), row))
        .collect()
}

/// Upstreams the `upstream:<name>` rows name (each built once, whichever modules it provides).
pub fn upstreams(providers: &Providers) -> BTreeSet<&str> {
    of_kind(providers, "upstream")
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

/// Modules our crates provide (`crate:<name>`): (module, crate).
pub fn crates(providers: &Providers) -> Vec<(&str, &str)> {
    of_kind(providers, "crate")
}

/// Modules system libraries provide (`system:<deps.toml row>`): (module, row).
pub fn systems(providers: &Providers) -> Vec<(&str, &str)> {
    of_kind(providers, "system")
}

/// Modules another tree module stands in for (`module:<module>`): (module, stand-in), the converter's `-stand-in` pairs (bp2ninja cc.go node checks the stand-in exists and installs under the module's name).
pub fn stand_ins(providers: &Providers) -> Vec<(&str, &str)> {
    of_kind(providers, "module")
}

/// One module provided outside the tree (`crate:`, `upstream:` and `system:` providers), as `xtask native` hands it to the converter in `<out>/provided.json` (bp2ninja provided.go `Provided`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Provided {
    /// Absolute path of the shared object or archive dependents link.
    pub lib: PathBuf,
    /// Header dirs dependents get as `-isystem`.
    pub include: Vec<PathBuf>,
    /// Header dirs dependents get as `-I` (Soong `export_include_dirs`): the guest libc++ prebuilts' (clangprebuilts.go androidLibcxxHeaderDirs), searched before bionic's `-isystem`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub export_include: Vec<PathBuf>,
    /// `NAME=VALUE` defines the headers depend on, given to dependents as `-D`.
    pub defines: Vec<String>,
}

/// One `srcs` row as `xtask native` hands it to the converter in `<out>/srcs.json` (bp2ninja provided.go `Partial`): the module is built from the tree without these sources, linking the provider's artifact if it has one.
#[derive(Debug, Serialize)]
pub struct Partial {
    /// As the row lists them: relative to the module's directory.
    pub srcs: Vec<String>,
    /// None for `provider = "none"`: nothing replaces the dropped sources.
    pub provider: Option<Provided>,
}

/// Row key = Android.bp module name.
pub type Providers = BTreeMap<String, Provider>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// None on a `rust_os` or `defines` row.
    pub provider: Option<String>,
    pub reason: String,
    /// Empty = the whole module.
    #[serde(default)]
    pub srcs: Vec<String>,
    /// The tree builds the module, as the OS's userspace instead of linux_glibc's (bp2ninja rust.go rustOSSide).
    pub rust_os: Option<RustOs>,
    /// The tree builds the module, its device C/C++ compiles with these macros (`NAME` or `NAME=VALUE`), each read by a patch under patches/ (bp2ninja provided.go parseDefines).
    #[serde(default)]
    pub defines: Vec<String>,
}

/// `module:MACRO` pairs of the `defines` rows, the converter's `-defines`; that each names a cc module with a device variant needs the tree, the converter checks it (bp2ninja provided.go checkDefines).
pub fn defines(providers: &Providers) -> Vec<String> {
    providers
        .iter()
        .flat_map(|(module, row)| row.defines.iter().map(move |d| format!("{module}:{d}")))
        .collect()
}

/// The macro name of a `defines` entry: `NAME` of `NAME` or `NAME=VALUE`.
fn macro_name(define: &str) -> &str {
    define.split_once('=').map_or(define, |(name, _)| name)
}

/// Every `defines` macro is read by a patch: some `+` line of a `.patch` file under `patches_dir` names it. A macro no patch reads switches nothing, so its row is dead.
fn check_defines_read(providers: &Providers, patches_dir: &Path) -> Result<()> {
    let mut added = String::new();
    let mut stack = vec![patches_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries {
            let path = e?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "patch") {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                for l in text
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                {
                    added.push_str(l);
                    added.push('\n');
                }
            }
        }
    }
    for (module, row) in providers {
        for d in &row.defines {
            let name = macro_name(d);
            let read = added.match_indices(name).any(|(i, _)| {
                let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
                !added[..i].ends_with(ident) && !added[i + name.len()..].starts_with(ident)
            });
            ensure!(
                read,
                "[{module}] defines: no patch under {} reads `{name}`",
                patches_dir.display()
            );
        }
    }
    Ok(())
}

/// `rust_os` values: the OS whose Android.bp `target` blocks a device Rust module takes; linux_glibc, every other crate's, is never written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RustOs {
    /// android blocks + `--cfg atl_android_userspace`, for a crate whose Android behaviour this runtime provides.
    Android,
}

/// Modules the converter compiles as Android userspace (`rust_os = "android"`); that each is a device Rust module needs the tree, the converter checks it (bp2ninja rust.go checkRustAndroid).
pub fn rust_android(providers: &Providers) -> Vec<&str> {
    providers
        .iter()
        .filter(|(_, row)| row.rust_os == Some(RustOs::Android))
        .map(|(module, _)| module.as_str())
        .collect()
}

impl Provider {
    /// `provider = "none"`.
    pub fn is_none(&self) -> bool {
        self.provider.as_deref() == Some(NONE)
    }
}

/// What a provider may name: workspace crates, sources.toml git upstreams, deps.toml rows.
pub struct Targets<'a> {
    pub crates: &'a BTreeSet<String>,
    pub upstreams: BTreeSet<&'a str>,
    pub deps: &'a Deps,
}

impl<'a> Targets<'a> {
    pub fn new(
        crates: &'a BTreeSet<String>,
        sources: &'a crate::sources::Sources,
        deps: &'a Deps,
    ) -> Self {
        let upstreams = sources.git.keys().map(String::as_str).collect();
        Self {
            crates,
            upstreams,
            deps,
        }
    }
}

pub fn load(root: &Path, targets: &Targets) -> Result<Providers> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let providers = parse(&text, targets).with_context(|| format!("in {FILE}"))?;
    check_defines_read(&providers, &root.join("patches")).with_context(|| format!("in {FILE}"))?;
    Ok(providers)
}

fn parse(text: &str, targets: &Targets) -> Result<Providers> {
    let providers: Providers = toml::from_str(text)?;
    for (module, row) in &providers {
        validate(module, row, targets).with_context(|| format!("[{module}]"))?;
    }
    Ok(providers)
}

fn validate(module: &str, row: &Provider, targets: &Targets) -> Result<()> {
    ensure!(is_module_name(module), "not an Android.bp module name");
    ensure!(!row.reason.trim().is_empty(), "empty reason");
    if row.rust_os.is_some() {
        ensure!(
            row.provider.is_none() && row.srcs.is_empty() && row.defines.is_empty(),
            "rust_os: the tree builds the module, so the row has no provider, srcs or defines"
        );
        return Ok(());
    }
    if !row.defines.is_empty() {
        ensure!(
            row.provider.is_none() && row.srcs.is_empty(),
            "defines: the tree builds the module, so the row has no provider or srcs"
        );
        let mut seen = BTreeSet::new();
        for d in &row.defines {
            let name = macro_name(d);
            ensure!(
                name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !d.contains(','),
                "defines: `{d}` is not NAME or NAME=VALUE (a C identifier, no comma)"
            );
            ensure!(seen.insert(name), "defines: `{name}` listed twice");
        }
        return Ok(());
    }
    // that each names a source of the module at the pin needs the tree: the converter checks it (bp2ninja provided.go checkPartial)
    let mut seen = BTreeSet::new();
    for src in &row.srcs {
        ensure!(
            is_src_path(src),
            "srcs: `{src}` is not a plain path relative to the module's directory"
        );
        ensure!(seen.insert(src), "srcs: `{src}` listed twice");
    }
    if row.is_none() {
        return Ok(());
    }
    let Some(p) = &row.provider else {
        bail!("no provider (a row without one needs rust_os or defines)")
    };
    // another tree module is the whole library: no sources of the replaced module left
    ensure!(
        !(p.starts_with("module:") && !row.srcs.is_empty()),
        "a module: provider takes no srcs"
    );
    validate_one(module, p, targets)?;
    Ok(())
}

fn validate_one(module: &str, provider: &str, targets: &Targets) -> Result<()> {
    match provider.split_once(':') {
        Some(("crate", name)) => {
            ensure!(targets.crates.contains(name), "no workspace crate `{name}`")
        }
        Some(("upstream", name)) => {
            ensure!(
                targets.upstreams.contains(name),
                "no sources.toml upstream `{name}`"
            );
            let Some(recipe) = upstream::recipe(name) else {
                bail!("xtask has no build recipe for upstream `{name}` (upstream.rs RECIPES)")
            };
            ensure!(
                recipe.modules.iter().any(|(m, _)| *m == module),
                "upstream `{name}` installs no module `{module}` (its recipe: {})",
                recipe
                    .modules
                    .iter()
                    .map(|(m, _)| *m)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Some(("system", name)) => {
            let Some(dep) = targets.deps.get(name) else {
                bail!("no deps.toml row `{name}`")
            };
            ensure!(
                dep.pkgconfig.is_some(),
                "deps.toml row `{name}` has no pkgconfig"
            );
            ensure!(
                dep.used_by.iter().any(|u| u == "upstream:aosp"),
                "deps.toml row `{name}` lacks `upstream:aosp` in used_by"
            );
        }
        // that the module exists and installs under `module`'s name needs the tree: the converter checks it
        Some(("module", name)) => {
            ensure!(
                is_module_name(name) && name != module,
                "module:`{name}` is not another Android.bp module name"
            )
        }
        _ => bail!(
            "provider `{provider}` is not crate:<name>, upstream:<name>, system:<name>, module:<name> or {NONE}"
        ),
    }
    Ok(())
}

/// Soong module names: letters, digits and `_ . + -` (e.g. `libc++`).
fn is_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// A `srcs` entry: a file path under the module's directory, as Android.bp `srcs` write one; no glob (the check is per file), no module reference, nothing leaving the directory.
fn is_src_path(s: &str) -> bool {
    // an empty component also rejects "", a leading or trailing `/`, and `//`
    !s.starts_with(':')
        && !s.contains(['*', '?', '[', '\\'])
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses with workspace crate `atl-nativeloader`, sources.toml git upstreams `boringssl` (has a recipe) and `qemu` (has none), deps.toml rows `egl` (an AOSP library), `git` (no pkgconfig) and `zlib` (not used by upstream:aosp).
    fn parsed(text: &str) -> Result<Providers> {
        let crates = ["atl-nativeloader".to_owned()].into();
        let deps: Deps = toml::from_str(
            "[egl]\ndebian = \"libegl-dev\"\npkgconfig = \"egl\"\nscope = [\"build\"]\n\
             used_by = [\"upstream:aosp\"]\n\
             [git]\ndebian = \"git\"\nscope = [\"test\"]\nused_by = [\"xtask:check\"]\n\
             [zlib]\ndebian = \"zlib1g-dev\"\npkgconfig = \"zlib\"\nscope = [\"build\"]\n\
             used_by = [\"xtask:check\"]\n",
        )
        .unwrap();
        let targets = Targets {
            crates: &crates,
            upstreams: ["boringssl", "qemu"].into(),
            deps: &deps,
        };
        parse(text, &targets)
    }

    fn err(text: &str) -> String {
        format!("{:#}", parsed(text).expect_err("should be rejected"))
    }

    #[test]
    fn crate_providers() {
        let text = "[libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(
            super::crates(&providers),
            [("libnativeloader", "atl-nativeloader")]
        );
    }

    #[test]
    fn upstream_providers() {
        let text = "[libcrypto]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n\
                    [libssl]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(upstreams(&providers), BTreeSet::from(["boringssl"]));
        assert!(
            err("[x]\nprovider = \"upstream:gone\"\nreason = \"r\"\n")
                .contains("no sources.toml upstream")
        );
        assert!(
            err("[x]\nprovider = \"upstream:qemu\"\nreason = \"r\"\n").contains("no build recipe")
        );
        // the recipe installs libcrypto, libcrypto_static and libssl only
        assert!(
            err("[libdecrepit]\nprovider = \"upstream:boringssl\"\nreason = \"r\"\n")
                .contains("installs no module `libdecrepit`")
        );
    }

    #[test]
    fn system_providers() {
        let text = "[libEGL]\nprovider = \"system:egl\"\nreason = \"r\"\n\
                    [libGLESv2]\nprovider = \"system:egl\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(
            systems(&providers),
            [("libEGL", "egl"), ("libGLESv2", "egl")]
        );
        assert!(
            err("[x]\nprovider = \"system:gone\"\nreason = \"r\"\n").contains("no deps.toml row")
        );
        assert!(
            err("[x]\nprovider = \"system:git\"\nreason = \"r\"\n").contains("has no pkgconfig")
        );
        assert!(
            err("[x]\nprovider = \"system:zlib\"\nreason = \"r\"\n")
                .contains("lacks `upstream:aosp`")
        );
    }

    #[test]
    fn module_providers() {
        let text =
            "[libartpalette-system]\nprovider = \"module:libartpalette_fake\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(
            stand_ins(&providers),
            [("libartpalette-system", "libartpalette_fake")]
        );
        assert!(err("[x]\nprovider = \"module:x\"\nreason = \"r\"\n").contains("not another"));
        assert!(
            err("[x]\nprovider = \"module:y\"\nsrcs = [\"a.cc\"]\nreason = \"r\"\n")
                .contains("takes no srcs")
        );
    }

    #[test]
    fn one_provider_per_row() {
        // a list is not a provider: the multi-provider form is gone
        assert!(!err("[x]\nprovider = [\"system:egl\"]\nreason = \"r\"\n").is_empty());
        assert!(err("[x]\nreason = \"r\"\n").contains("no provider"));
    }

    #[test]
    fn missing_targets() {
        assert!(
            err("[x]\nprovider = \"crate:gone\"\nreason = \"r\"\n").contains("no workspace crate")
        );
    }

    #[test]
    fn none_drops_the_dependency() {
        let text = "[libdl_android]\nprovider = \"none\"\nreason = \"bionic linker API\"\n\
                    [libnativeloader]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(super::omitted(&providers), ["libdl_android"]);
        assert!(err("[x]\nprovider = \"none\"\nreason = \"\"\n").contains("empty reason"));
    }

    #[test]
    fn unknown_kind_and_empty_reason() {
        assert!(err("[x]\nprovider = \"aosp:x\"\nreason = \"r\"\n").contains("is not crate:"));
        assert!(
            err("[x]\nprovider = \"crate:atl-nativeloader\"\nreason = \" \"\n")
                .contains("empty reason")
        );
    }

    #[test]
    fn rust_os_rows() {
        let text = "[librustutils]\nrust_os = \"android\"\nreason = \"r\"\n\
                    [libdl]\nprovider = \"none\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        assert_eq!(rust_android(&providers), ["librustutils"]);
        // built from the tree: provided by nothing, dropped by nothing
        assert_eq!(super::omitted(&providers), ["libdl"]);
        assert!(super::partial(&providers).is_empty());
        assert!(super::crates(&providers).is_empty());
        for bad in [
            "provider = \"none\"\n",
            "provider = \"crate:atl-nativeloader\"\n",
            "srcs = [\"a.rs\"]\n",
            "defines = [\"A\"]\n",
        ] {
            assert!(
                err(&format!(
                    "[x]\nrust_os = \"android\"\nreason = \"r\"\n{bad}"
                ))
                .contains("no provider, srcs or defines"),
                "{bad}"
            );
        }
        assert!(err("[x]\nrust_os = \"android\"\nreason = \" \"\n").contains("empty reason"));
        // linux_glibc is every other crate's OS, never written
        assert!(
            err("[x]\nrust_os = \"linux_glibc\"\nreason = \"r\"\n").contains("unknown variant")
        );
        assert!(err("[x]\nreason = \"r\"\n").contains("needs rust_os"));
    }

    /// `defines` rows: handed to the converter as module:MACRO; refused with a provider or srcs, a macro that is no C identifier or holds a comma, or one listed twice.
    #[test]
    fn defines_rows() {
        let providers =
            parsed("[servicemanager]\ndefines = [\"NO_X\", \"LEVEL=2\"]\nreason = \"r\"\n")
                .unwrap();
        assert_eq!(
            defines(&providers),
            ["servicemanager:NO_X", "servicemanager:LEVEL=2"]
        );
        assert!(super::omitted(&providers).is_empty() && super::partial(&providers).is_empty());
        for bad in ["provider = \"none\"\n", "srcs = [\"a.cpp\"]\n"] {
            assert!(
                err(&format!("[x]\ndefines = [\"A\"]\nreason = \"r\"\n{bad}"))
                    .contains("no provider or srcs"),
                "{bad}"
            );
        }
        for bad in ["1A", "A-B", "", "=1", "A=1,2"] {
            assert!(
                err(&format!("[x]\ndefines = [\"{bad}\"]\nreason = \"r\"\n"))
                    .contains("not NAME or NAME=VALUE"),
                "{bad}"
            );
        }
        assert!(err("[x]\ndefines = [\"A\", \"A=1\"]\nreason = \"r\"\n").contains("listed twice"));
    }

    /// A macro counts as read only where a patch's added line names it as a whole identifier; a context or removed line, a longer identifier or no patch at all leave the row dead.
    #[test]
    fn defines_read_by_a_patch() {
        let dir =
            std::env::temp_dir().join(format!("atl-providers-defines-{}", std::process::id()));
        let sub = dir.join("aosp/frameworks/native");
        std::fs::create_dir_all(&sub).unwrap();
        let rows =
            |d: &str| parsed(&format!("[sm]\ndefines = [\"{d}\"]\nreason = \"r\"\n")).unwrap();
        let patch = "--- a/Access.cpp\n+++ b/Access.cpp\n NO_CONTEXT\n-#ifdef NO_REMOVED\n+#if !defined(NO_X)\n+#ifdef NO_XY_LONGER\n";
        std::fs::write(sub.join("0001-x.patch"), patch).unwrap();
        // a non-patch file is not read
        std::fs::write(sub.join("LICENSE"), "+NO_LICENSE\n").unwrap();
        assert!(check_defines_read(&rows("NO_X=1"), &dir).is_ok());
        for dead in ["NO_CONTEXT", "NO_REMOVED", "NO_XY", "NO_LICENSE", "Access"] {
            let e = check_defines_read(&rows(dead), &dir).unwrap_err();
            assert!(format!("{e:#}").contains("no patch"), "{dead}");
        }
        assert!(check_defines_read(&rows("NO_X"), &dir.join("absent")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn srcs_rows() {
        let text = "[libskia]\nprovider = \"none\"\nreason = \"r\"\nsrcs = [\"src/codec/SkRawCodec.cpp\"]\n\
                    [libandroid_runtime]\nprovider = \"crate:atl-nativeloader\"\nreason = \"r\"\nsrcs = [\"a.cpp\"]\n\
                    [libdl]\nprovider = \"none\"\nreason = \"r\"\n";
        let providers = parsed(text).unwrap();
        // a srcs row keeps its module: only the whole-module `none` row drops dependencies
        assert_eq!(super::omitted(&providers), ["libdl"]);
        let partial: Vec<&str> = super::partial(&providers).iter().map(|(m, _)| *m).collect();
        assert_eq!(partial, ["libandroid_runtime", "libskia"]);
        // its crate is still built
        assert_eq!(
            super::crates(&providers),
            [("libandroid_runtime", "atl-nativeloader")]
        );
        let row = |srcs: &str| format!("[x]\nprovider = \"none\"\nreason = \"r\"\nsrcs = {srcs}\n");
        for bad in [
            "[\"\"]",
            "[\"/abs.cpp\"]",
            "[\"../up.cpp\"]",
            "[\"a//b.cpp\"]",
            "[\"./a.cpp\"]",
            "[\"*.cpp\"]",
            "[\":filegroup\"]",
        ] {
            assert!(err(&row(bad)).contains("not a plain path"), "{bad}");
        }
        assert!(err(&row("[\"a.cpp\", \"a.cpp\"]")).contains("listed twice"));
        // the reason stays required on a srcs row
        assert!(
            err("[x]\nprovider = \"none\"\nreason = \"\"\nsrcs = [\"a.cpp\"]\n")
                .contains("empty reason")
        );
    }

    #[test]
    fn module_names() {
        assert!(is_module_name("libc++"));
        assert!(is_module_name("libnativehelper_jvm"));
        assert!(!is_module_name("lib c"));
        assert!(!is_module_name("//art:libart"));
    }
}
