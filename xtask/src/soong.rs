// SPDX-License-Identifier: MIT
//! `xtask soong`: AOSP modules built by upstream Soong in the pinned subset tree `<workspace>/src/aosp`
//! (phase-1 route; the converter takes over host builds later).
//!
//! The product is generated from arches.toml, never hand-written: ATL's host arch is AOSP's device arch,
//! the guest arch is AOSP's native-bridge arch. `--fetch-missing` grows the tree: a failed build's
//! output names missing modules (-> project via the `xtask aosp` index) and missing tree paths (->
//! project by path), those get pinned and fetched, and the build runs again. It stops when a round
//! finds nothing new, so it ends after at most one round per manifest project.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, ensure};

use crate::aosp::{self, Index, Manifest};
use crate::arches::{self, Arch, Arches};
use crate::{fetch, sources, workspace};

#[derive(clap::Args)]
pub struct Args {
    /// Build goals (Make module names), e.g. `libnative_bridge_guest_libc.native_bridge`.
    #[arg(required = true)]
    goals: Vec<String>,
    /// Arch ATL runs on = AOSP's device arch: an arches.toml row with `[host]` and `[guest]`.
    #[arg(long)]
    host: String,
    /// Arch built as AOSP's native-bridge target: an arches.toml row with `[guest]`.
    #[arg(long)]
    guest: String,
    /// After a failed build, pin and fetch the projects defining what it reports missing and build
    /// again, until a build passes or a round finds nothing new to fetch.
    #[arg(long)]
    fetch_missing: bool,
}

/// Our generated products live here; no manifest project may own it (checked before writing).
const PRODUCT_DIR: &str = "device/atl";
/// `eng`: debuggable, assertions on -- the spike needs inspection, not release behaviour.
const BUILD_VARIANT: &str = "eng";
/// Error lines echoed per failed round; the log keeps all of them.
const ERRORS_SHOWN: usize = 20;

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let ws = workspace::root_dir(root)?;
    let tree = ws.join("src").join("aosp");
    let product = product(&arches::load(root)?, &args.host, &args.guest)?;
    let out = ws.join("out").join("aosp").join(&product.name);
    let logs = ws.join("out").join("aosp").join("logs");
    std::fs::create_dir_all(&logs)?;
    let mut round = 0;
    loop {
        round += 1;
        let sources = sources::load(root)?;
        let manifest = aosp::load(root, &sources.aosp)?
            .context("AOSP manifest not fetched: xtask fetch --execute")?;
        write_product(&tree, &manifest, &product)?;
        let release = release(&tree)?;
        let log = logs.join(format!("{}-{round}.log", product.name));
        println!(
            "soong: round {round}: {} TARGET_RELEASE={release} -> {}",
            product.name,
            log.display()
        );
        let started = Instant::now();
        let ok = build(
            &tree,
            &out,
            &product.name,
            &release,
            &args.goals,
            true,
            &log,
        )?;
        let text = String::from_utf8_lossy(&std::fs::read(&log)?).into_owned();
        println!(
            "soong: {} after {:.0?}",
            if ok { "built" } else { "failed" },
            started.elapsed()
        );
        if ok {
            return Ok(ExitCode::SUCCESS);
        }
        let mut errors: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("error") || l.starts_with("FAILED:"))
            .collect();
        if errors.is_empty() {
            // failed before any tool reported an error (e.g. bootstrap): the end of the log
            let lines: Vec<&str> = text.lines().collect();
            errors = lines[lines.len().saturating_sub(ERRORS_SHOWN)..].to_vec();
        }
        for e in errors.iter().take(ERRORS_SHOWN) {
            println!("soong:   {e}");
        }
        if errors.len() > ERRORS_SHOWN {
            println!(
                "soong:   ... {} more error lines in the log",
                errors.len() - ERRORS_SHOWN
            );
        }
        if !args.fetch_missing {
            return Ok(ExitCode::FAILURE);
        }
        let index = Index::load(root, &sources.aosp)?;
        let pinned: BTreeSet<&str> = sources.aosp.projects.keys().map(String::as_str).collect();
        let tree_prefix = format!("{}/", tree.display());
        let mut found = missing(&text, &tree_prefix);
        let failing = failing_modules(&text);
        if found.is_empty() && !failing.is_empty() {
            // Allowed-missing deps are silent until a goal builds them, yet a missing `defaults` module
            // already changes a module's analysis (service-art lost its scopes). A strict analysis names
            // every undefined dependency; only those of the failing modules count.
            let strict = logs.join(format!("{}-{round}-strict.log", product.name));
            println!(
                "soong: strict analysis for {} failing modules -> {}",
                failing.len(),
                strict.display()
            );
            build(
                &tree,
                &out,
                &product.name,
                &release,
                &args.goals,
                false,
                &strict,
            )?;
            let strict_text = String::from_utf8_lossy(&std::fs::read(&strict)?).into_owned();
            found = missing(&strict_text, &tree_prefix);
            // a failing module's defaults are part of it (service-art <- service-art-defaults)
            let scope = index.with_defaults(&failing);
            found
                .modules
                .retain(|_, line| depender(line).is_some_and(|d| scope.contains(d)));
        }
        let (want, unresolved) = resolve(&found, &manifest, &index, &pinned);
        for u in &unresolved {
            println!("soong: unresolved {u}");
        }
        if want.is_empty() {
            println!("soong: nothing new to fetch; stopping");
            return Ok(ExitCode::FAILURE);
        }
        for (project, why) in &want {
            println!("soong: + {project:<40} {why}");
        }
        fetch::add_and_fetch(root, &want.into_keys().collect::<Vec<_>>())?;
    }
}

// ---- product ---------------------------------------------------------------------------------------

struct Product {
    name: String,
    /// File name in the product directory -> content.
    files: Vec<(String, String)>,
}

fn product(arches: &Arches, host: &str, guest: &str) -> Result<Product> {
    let arch = |name: &str| {
        arches
            .get(name)
            .with_context(|| format!("{name}: not in {}", arches::FILE))
    };
    let abis = |name: &str, a: &Arch| {
        a.guest
            .as_ref()
            .map(|g| g.android_abis.clone())
            .with_context(|| format!("{name}: no [{name}.guest] row (its Android ABIs)"))
    };
    let (h, g) = (arch(host)?, arch(guest)?);
    ensure!(h.host.is_some(), "{host}: no [{host}.host] row");
    ensure!(
        h.pointer_bits == 64 && g.pointer_bits == 64,
        "{host} + {guest}: only 64-bit pairs are generated; a 32-bit guest is AOSP's secondary \
         native-bridge arch, which needs a secondary device arch"
    );
    let host_abi = abis(host, h)?.into_iter().next().context("no ABI")?;
    let guest_abis = abis(guest, g)?.join(" ");
    let name = format!("atl_{host}_{guest}");
    let header = "# Generated by `cargo xtask soong` from arches.toml; rewritten on every build.\n";
    let files = vec![
        (
            "AndroidProducts.mk".to_owned(),
            format!("{header}PRODUCT_MAKEFILES := $(LOCAL_DIR)/{name}.mk\n"),
        ),
        (
            format!("{name}.mk"),
            format!(
                "{header}$(call inherit-product, $(SRC_TARGET_DIR)/product/core_64_bit_only.mk)\n\
                 PRODUCT_NAME := {name}\nPRODUCT_DEVICE := {name}\nPRODUCT_BRAND := ATL\n\
                 PRODUCT_MODEL := {name}\n\
                 # ATL builds libraries, never device images; the ramdisk is the one image on by default\n\
                 PRODUCT_BUILD_RAMDISK_IMAGE := false\n"
            ),
        ),
        (
            "BoardConfig.mk".to_owned(),
            format!(
                "{header}TARGET_ARCH := {host}\nTARGET_ARCH_VARIANT := {}\nTARGET_CPU_ABI := {host_abi}\n\
                 TARGET_NATIVE_BRIDGE_ARCH := {guest}\nTARGET_NATIVE_BRIDGE_ARCH_VARIANT := {}\n\
                 TARGET_NATIVE_BRIDGE_ABI := {guest_abis}\n",
                h.soong_arch_variant.as_deref().unwrap_or_default(),
                g.soong_arch_variant.as_deref().unwrap_or_default(),
            ),
        ),
    ];
    Ok(Product { name, files })
}

/// Write the product into the tree; a file is only rewritten when its content changed (Kati reruns on
/// a newer timestamp).
fn write_product(tree: &Path, manifest: &Manifest, product: &Product) -> Result<()> {
    let prefix = format!("{PRODUCT_DIR}/");
    ensure!(
        manifest.project_of(PRODUCT_DIR).is_none()
            && !manifest.projects.keys().any(|p| p.starts_with(&prefix)),
        "{PRODUCT_DIR} belongs to a manifest project"
    );
    let dir = tree.join(PRODUCT_DIR).join(&product.name);
    std::fs::create_dir_all(&dir)?;
    for (file, text) in &product.files {
        let path = dir.join(file);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text) {
            std::fs::write(&path, text).with_context(|| path.display().to_string())?;
        }
    }
    Ok(())
}

/// TARGET_RELEASE of the pinned tag: AOSP release builds use the release config named by the build
/// ID's first field (`BP2A.250605.031.A2` -> `bp2a`). Kati rejects a name build/release lacks.
fn release(tree: &Path) -> Result<String> {
    let file = tree.join("build/make/core/build_id.mk");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("BUILD_ID="))
        .with_context(|| format!("no BUILD_ID= in {}", file.display()))?;
    let field = id.split('.').next().unwrap_or(id);
    Ok(field.to_lowercase())
}

/// `allow_missing`: ALLOW_MISSING_DEPENDENCIES, the subset tree's normal mode; off = a strict analysis
/// that fails naming every undefined dependency.
fn build(
    tree: &Path,
    out: &Path,
    product: &str,
    release: &str,
    goals: &[String],
    allow_missing: bool,
    log: &Path,
) -> Result<bool> {
    let file = std::fs::File::create(log).with_context(|| log.display().to_string())?;
    let status = Command::new("build/soong/soong_ui.bash")
        .current_dir(tree)
        // -k: keep going, so one round reports every missing piece
        .args(["--make-mode", "-k"])
        .args(goals)
        .env("TARGET_PRODUCT", product)
        .env("TARGET_RELEASE", release)
        .env("TARGET_BUILD_VARIANT", BUILD_VARIANT)
        // soong_config.mk passes `$(filter true,…)`: "false" is off
        .env("ALLOW_MISSING_DEPENDENCIES", allow_missing.to_string())
        .env("OUT_DIR", out)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .with_context(|| format!("running soong_ui.bash in {}", tree.display()))?;
    Ok(status.success())
}

// ---- what a failed build is missing ----------------------------------------------------------------

/// What a failed build's output names as missing, each with the first line that said so.
#[derive(Default, Debug, PartialEq)]
struct Missing {
    /// Module references as Soong prints them.
    modules: BTreeMap<String, String>,
    /// Module types no soong_build plugin in the tree registers.
    module_types: BTreeMap<String, String>,
    /// Tree-relative paths.
    paths: BTreeMap<String, String>,
}

impl Missing {
    fn is_empty(&self) -> bool {
        self.modules.is_empty() && self.module_types.is_empty() && self.paths.is_empty()
    }
}

/// Phrases of a line reporting a missing file (lowercase); other lines may name paths in prose
/// ("a usage example in prebuilts/sdk") and are not evidence.
const MISSING_FILE: [&str; 5] = [
    "no such file",
    "not found",
    "does not exist",
    "cannot find",
    "no rule to make target",
];

/// Modules a Soong analysis error is about: `error: … module "X" variant "…": …`, or a panic under
/// missing dependencies: `internal error: panic in … for module "X" …`.
fn failing_modules(log: &str) -> BTreeSet<String> {
    log.lines()
        .filter(|l| l.starts_with("error: ") || l.starts_with("internal error: panic"))
        .filter_map(|l| l.split_once("module \"")?.1.split_once('"'))
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// The depending module of a `"X" depends on undefined module "Y"` line.
fn depender(line: &str) -> Option<&str> {
    let (head, _) = line.split_once(" depends on undefined module")?;
    head.strip_suffix('"')?.rsplit_once('"').map(|(_, d)| d)
}

/// Soong analysis error for a dependency no Android.bp defines.
const UNDEFINED: &str = "depends on undefined module \"";
/// Blueprint parse error for a module type no plugin registers.
const UNKNOWN_TYPE: &str = "unrecognized module type \"";
/// Soong's error rule under ALLOW_MISSING_DEPENDENCIES: entries are module references, tree paths or
/// whole messages (`android/paths.go`).
const MISSING_DEPS: &str = "missing dependencies: ";

/// `tree_prefix` = the tree's absolute path with a trailing `/`, stripped from absolute paths.
fn missing(log: &str, tree_prefix: &str) -> Missing {
    let mut out = Missing::default();
    let note = |map: &mut BTreeMap<String, String>, key: &str, line: &str| {
        map.entry(key.to_owned())
            .or_insert_with(|| line.trim().to_owned());
    };
    for line in log.lines() {
        if let Some((_, rest)) = line.split_once(UNDEFINED)
            && let Some((name, _)) = rest.split_once('"')
        {
            note(&mut out.modules, name, line);
        } else if let Some((_, rest)) = line.split_once(UNKNOWN_TYPE)
            && let Some((module_type, _)) = rest.split_once('"')
        {
            note(&mut out.module_types, module_type, line);
        } else if let Some((_, rest)) = line.split_once(MISSING_DEPS) {
            for entry in rest.split(", ").map(str::trim).filter(|e| !e.is_empty()) {
                let is_path = entry.contains('/') && !entry.starts_with("//");
                if entry.contains(char::is_whitespace) || is_path {
                    for p in path_tokens(entry, tree_prefix) {
                        note(&mut out.paths, &p, line);
                    }
                } else {
                    note(&mut out.modules, entry, line);
                }
            }
        } else if MISSING_FILE.iter().any(|p| line.to_lowercase().contains(p)) {
            for p in path_tokens(line, tree_prefix) {
                note(&mut out.paths, &p, line);
            }
        }
    }
    out
}

/// Relative path-like tokens (`a/b`), absolute ones under the tree made relative.
fn path_tokens(text: &str, tree_prefix: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "'\"`,:;()[]{}=<>|".contains(c))
        .map(|t| t.strip_prefix(tree_prefix).unwrap_or(t))
        .map(|t| t.trim_start_matches("./").trim_end_matches('.'))
        .filter(|t| t.contains('/') && !t.starts_with('/') && !t.split('/').any(|c| c == ".."))
        .map(str::to_owned)
        .collect()
}

/// Unpinned project -> why it is wanted; plus what could not be mapped to a project to fetch.
fn resolve(
    found: &Missing,
    manifest: &Manifest,
    index: &Index,
    pinned: &BTreeSet<&str>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut want = BTreeMap::new();
    let mut unresolved = Vec::new();
    // a path in an unpinned project is missing by construction; any other path is not our concern
    for (path, line) in &found.paths {
        if let Some(p) = manifest.project_of(path)
            && !pinned.contains(p)
        {
            want.entry(p.to_owned())
                .or_insert_with(|| format!("path {path}: {line}"));
        }
    }
    for (module, line) in &found.modules {
        match index.project_for(module) {
            Ok(p) if !pinned.contains(p) => {
                want.entry(p.to_owned())
                    .or_insert_with(|| format!("module {module}: {line}"));
            }
            Ok(p) => unresolved.push(format!("{module}: defined in pinned {p}, yet missing")),
            Err(e) => unresolved.push(format!("{module}: {e}")),
        }
    }
    for (module_type, line) in &found.module_types {
        match index.project_for_type(module_type) {
            Ok(p) if !pinned.contains(p) => {
                want.entry(p.to_owned())
                    .or_insert_with(|| format!("module type {module_type}: {line}"));
            }
            Ok(p) => unresolved.push(format!(
                "module type {module_type}: registered in pinned {p}, yet unknown"
            )),
            Err(e) => unresolved.push(format!("module type {module_type}: {e}")),
        }
    }
    (want, unresolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_modules_and_paths() {
        let log = "\
error: art/Android.bp:1:1: \"libart\" depends on undefined module \"libz\".
FAILED: out/x.so
module libfoo missing dependencies: libbar, //ns/dir:libbaz, external/qux/a.c, :gen{.h}
bash: /t/src/aosp/prebuilts/go/linux-x86/bin/go: No such file or directory
art/runtime/x.cc:3:10: fatal error: 'foo.h' file not found
error: art/artd/binder/Android.bp:25:1: unrecognized module type \"aidl_interface\"
Please see the documentation of the prebuilt_apis module type (and a usage example in prebuilts/sdk)
";
        let m = missing(log, "/t/src/aosp/");
        let keys = |b: &BTreeMap<String, String>| b.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            keys(&m.modules),
            ["//ns/dir:libbaz", ":gen{.h}", "libbar", "libz"]
        );
        // only lines reporting a missing file: not `FAILED:` outputs, not prose
        assert_eq!(
            keys(&m.paths),
            [
                "art/runtime/x.cc",
                "external/qux/a.c",
                "prebuilts/go/linux-x86/bin/go"
            ]
        );
        assert!(m.modules["libz"].contains("depends on undefined"));
        assert_eq!(keys(&m.module_types), ["aidl_interface"]);
    }

    #[test]
    fn strict_pass_filters() {
        let log = "\
error: art/libartservice/service/Android.bp:132:1: module \"service-art\" variant \"android_common\": service-art is missing tracking files
error: x/Android.bp:1:1: unrelated text
internal error: panic in GenerateBuildActions for module \"test.apex\" variant \"android_common\"
";
        assert_eq!(
            failing_modules(log),
            BTreeSet::from(["service-art".to_owned(), "test.apex".to_owned()])
        );
        let line = "error: art/Android.bp:132:1: \"service-art\" depends on undefined module \"framework-system-server-module-defaults\".";
        assert_eq!(depender(line), Some("service-art"));
        assert_eq!(depender("no such line"), None);
    }

    #[test]
    fn product_from_arch_table() {
        let arches: Arches = toml::from_str(
            "[x86_64]\npointer_bits = 64\n[x86_64.host]\nrust = \"r\"\nmeson_cpu_family = \"m\"\n\
             debian = \"amd64\"\n[x86_64.guest]\nandroid_abis = [\"x86_64\"]\nclang = \"c\"\n\
             qemu_target = \"q\"\n[arm64]\npointer_bits = 64\nsoong_arch_variant = \"armv8-a\"\n[arm64.guest]\n\
             android_abis = [\"arm64-v8a\"]\nclang = \"c\"\nqemu_target = \"q\"\n\
             [arm]\npointer_bits = 32\n[arm.guest]\nandroid_abis = [\"armeabi-v7a\"]\nclang = \"c\"\n\
             qemu_target = \"q\"\n",
        )
        .unwrap();
        let p = product(&arches, "x86_64", "arm64").unwrap();
        assert_eq!(p.name, "atl_x86_64_arm64");
        let board = &p
            .files
            .iter()
            .find(|(f, _)| f == "BoardConfig.mk")
            .unwrap()
            .1;
        // x86_64 row has no variant: Soong's baseline
        assert!(board.contains(
            "TARGET_ARCH := x86_64\nTARGET_ARCH_VARIANT := \nTARGET_CPU_ABI := x86_64\n"
        ));
        assert!(board.contains(
            "TARGET_NATIVE_BRIDGE_ARCH := arm64\nTARGET_NATIVE_BRIDGE_ARCH_VARIANT := armv8-a\n\
             TARGET_NATIVE_BRIDGE_ABI := arm64-v8a\n"
        ));
        // arm64 has no host row; arm is 32-bit
        assert!(product(&arches, "arm64", "x86_64").is_err());
        assert!(product(&arches, "x86_64", "arm").is_err());
    }
}
