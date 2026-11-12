// SPDX-License-Identifier: MIT
//! NDK-surface gate on runtimes pass 2: `libc++.so` links against the converter-built guest `libc/libm/libdl.so` (runtimes.rs SHARED), where llvm_android links against the NDK stubs.
//! The real libs export more than the stubs -- `LIBC_PRIVATE` / `LIBC_PLATFORM` nodes, apex/llndk/systemapi symbols -- so a libc++ that binds one of those would load on our libc but not on a device's, and depends on a private bionic surface.
//! This gate fails on any version-needed symbol the NDK stub of that lib would not export. Rules are symbolfile's NDK mode (build/soong/cc/symbolfile `should_omit_version` / `should_omit_symbol`): a version node is omitted when private (`*_PRIVATE` / `*_PLATFORM`), `platform-only`, carrying a mode tag, or not for this arch; a symbol when it carries a mode tag or is not for this arch.
//! API levels are not compared: pass 2 targets the platform level (runtimes.rs PLATFORM_API, the future level), at which every `introduced=` is in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use object::read::elf::ElfFile64;
use object::{Endianness, Object, ObjectSymbol};

/// symbolfile ALL_ARCHITECTURES: tags restricting a node or symbol to these arches.
const ARCH_TAGS: &[&str] = &["arm", "arm64", "riscv64", "x86", "x86_64"];
/// symbolfile mode tags: in NDK mode they exclude.
const MODE_TAGS: &[&str] = &["apex", "llndk", "systemapi"];

/// The `.map.txt` of each lib the check knows, by soname, relative to the AOSP tree.
pub const MAPS: &[(&str, &str)] = &[
    ("libc.so", "bionic/libc/libc.map.txt"),
    ("libm.so", "bionic/libm/libm.map.txt"),
    ("libdl.so", "bionic/libdl/libdl.map.txt"),
];

/// (version node, symbol) pairs a stub exports.
pub type Surface = BTreeSet<(String, String)>;

fn tags(line: &str) -> Vec<&str> {
    line.split_once('#')
        .map(|(_, t)| t.split_whitespace().collect())
        .unwrap_or_default()
}

fn in_arch(tags: &[&str], arch: &str) -> bool {
    !tags.iter().any(|t| ARCH_TAGS.contains(t)) || tags.contains(&arch)
}

fn has_mode(tags: &[&str]) -> bool {
    tags.iter().any(|t| MODE_TAGS.contains(t))
}

/// One global symbol line of a map file, with its node's tags and its own.
struct Entry<'a> {
    node: &'a str,
    node_tags: Vec<&'a str>,
    sym: &'a str,
    tags: Vec<&'a str>,
}

/// Every symbol a map file lists, in order (the `local: *;` catch-all aside).
fn entries(map: &str) -> Result<Vec<Entry<'_>>> {
    let mut out = Vec::new();
    let mut node: Option<(&str, Vec<&str>)> = None;
    for (n, raw) in map.lines().enumerate() {
        let code = raw.split('#').next().unwrap_or("").trim();
        if code.is_empty() {
            continue;
        }
        if let Some(name) = code.strip_suffix('{').map(str::trim) {
            node = Some((name, tags(raw)));
        } else if code.starts_with('}') {
            node = None;
        } else if code == "global:" || code == "local:" || code == "*;" {
        } else {
            let (name, node_tags) = node
                .as_ref()
                .with_context(|| format!("line {}: symbol outside a version node", n + 1))?;
            out.push(Entry {
                node: name,
                node_tags: node_tags.clone(),
                sym: code.trim_end_matches(';').trim(),
                tags: tags(raw),
            });
        }
    }
    Ok(out)
}

/// The NDK stub surface of one map file for `arch`.
pub fn surface(map: &str, arch: &str) -> Result<Surface> {
    Ok(entries(map)?
        .into_iter()
        .filter(|e| {
            let t = &e.node_tags;
            !(e.node.ends_with("_PRIVATE") || e.node.ends_with("_PLATFORM"))
                && !t.contains(&"platform-only")
                && !has_mode(t)
                && in_arch(t, arch)
                && !has_mode(&e.tags)
                && in_arch(&e.tags, arch)
        })
        .map(|e| (e.node.to_string(), e.sym.to_string()))
        .collect())
}

/// What a link with `map` as `-Wl,--version-script` exports: every listed symbol under its node. The tags are `#` comments to the linker, so none filters (Soong passes `version_script` to the link unchanged, cc/linker.go).
pub fn script(map: &str) -> Result<Surface> {
    Ok(entries(map)?
        .into_iter()
        .map(|e| (e.node.to_string(), e.sym.to_string()))
        .collect())
}

/// NDK libraries built from AOSP source as-is (gate_goals.rs `NDK_FRONTS`) and the version script their Android.bp names, relative to the AOSP tree; the fronts gate compares each built library's exports with it.
pub const FRONT_SCRIPTS: &[(&str, &str)] =
    &[("libOpenSLES", "frameworks/wilhelm/src/libOpenSLES.map.txt")];

/// The (version node, symbol) pairs `lib` defines and exports dynamically; an unversioned one has node "". The version definitions' own symbols are not exports.
pub fn exports(lib: &Path) -> Result<Surface> {
    let data = std::fs::read(lib).with_context(|| format!("reading {}", lib.display()))?;
    let elf = ElfFile64::<Endianness>::parse(&*data)?;
    let endian = elf.endian();
    let versions = elf.elf_section_table().versions(endian, &*data)?;
    let mut out = Surface::new();
    for s in elf
        .dynamic_symbols()
        .filter(|s| !s.is_undefined() && s.is_global())
    {
        let name = s.name()?;
        let node = match &versions {
            Some(v) => v
                .version(v.version_index(endian, s.index()).index())?
                .map(|v| String::from_utf8_lossy(v.name()).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        };
        if name.is_empty() || name == node {
            continue;
        }
        out.insert((node, name.to_string()));
    }
    Ok(out)
}

/// The differences between `lib`'s exports and the version script at `map`, one line each; empty when equal.
pub fn front_mismatch(lib: &Path, map: &Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(map).with_context(|| format!("reading {}", map.display()))?;
    let want = script(&text).with_context(|| format!("in {}", map.display()))?;
    let have = exports(lib)?;
    Ok(diff(&have, &want))
}

fn diff(have: &Surface, want: &Surface) -> Vec<String> {
    let missing = want
        .difference(have)
        .map(|(n, s)| format!("missing {s}@{n}"));
    let extra = have.difference(want).map(|(n, s)| format!("extra {s}@{n}"));
    missing.chain(extra).collect()
}

/// One version-needed binding of a dynamic undefined symbol: (needed file, version node, symbol).
pub type Binding = (String, String, String);

/// Every versioned undefined dynamic symbol of `lib`.
pub fn bindings(lib: &Path) -> Result<Vec<Binding>> {
    let data = std::fs::read(lib).with_context(|| format!("reading {}", lib.display()))?;
    let elf = ElfFile64::<Endianness>::parse(&*data)?;
    let endian = elf.endian();
    let sections = elf.elf_section_table();
    let versions = sections
        .versions(endian, &*data)?
        .with_context(|| format!("{}: no .gnu.version", lib.display()))?;
    let mut out = Vec::new();
    for s in elf.dynamic_symbols().filter(|s| s.is_undefined()) {
        let name = s.name()?;
        if name.is_empty() {
            continue;
        }
        let idx = versions.version_index(endian, s.index()).index();
        if let Some(v) = versions.version(idx)? {
            let file = v.file().map(|f| String::from_utf8_lossy(f).into_owned());
            out.push((
                file.unwrap_or_default(),
                String::from_utf8_lossy(v.name()).into_owned(),
                name.to_string(),
            ));
        }
    }
    Ok(out)
}

/// The bindings outside the stub surfaces; a binding against a lib with no surface counts as outside.
pub fn outside(bindings: &[Binding], surfaces: &BTreeMap<String, Surface>) -> Vec<String> {
    bindings
        .iter()
        .filter(|(file, node, sym)| {
            !surfaces
                .get(file)
                .is_some_and(|s| s.contains(&(node.clone(), sym.clone())))
        })
        .map(|(file, node, sym)| format!("{sym}@{node} ({file})"))
        .collect()
}

/// Check `lib` against the MAPS stubs in `tree` for `arch`; returns (binding count, violations).
pub fn check(lib: &Path, tree: &Path, arch: &str) -> Result<(usize, Vec<String>)> {
    let mut surfaces = BTreeMap::new();
    for (soname, rel) in MAPS {
        let p = tree.join(rel);
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        surfaces.insert(
            soname.to_string(),
            surface(&text, arch).with_context(|| format!("in {}", p.display()))?,
        );
    }
    let b = bindings(lib)?;
    let bad = outside(&b, &surfaces);
    Ok((b.len(), bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = "LIBC {\n  global:\n    malloc;\n    __apex_only; # apex\n    __arm_only; # arm\n  local:\n    *;\n};\nLIBC_O { # introduced=26\n    pthread_getname_np;\n} LIBC;\nLIBC_PRIVATE {\n    __libc_init_private;\n} LIBC_O;\nLIBC_PLATFORM {\n    __plat;\n} LIBC_O;\nLIBC_DEPRECATED { # arm platform-only\n    __old;\n};\n";

    fn run(node: &str, sym: &str) -> Vec<String> {
        let mut m = BTreeMap::new();
        m.insert("libc.so".to_string(), surface(MAP, "x86_64").unwrap());
        outside(&[("libc.so".into(), node.into(), sym.into())], &m)
    }

    #[test]
    fn ndk_symbols_pass() {
        assert!(run("LIBC", "malloc").is_empty());
        assert!(run("LIBC_O", "pthread_getname_np").is_empty());
    }

    #[test]
    fn private_nodes_fire() {
        assert_eq!(run("LIBC_PRIVATE", "__libc_init_private").len(), 1);
        assert_eq!(run("LIBC_PLATFORM", "__plat").len(), 1);
        assert_eq!(run("LIBC_DEPRECATED", "__old").len(), 1);
    }

    #[test]
    fn tagged_symbols_fire() {
        assert_eq!(run("LIBC", "__apex_only").len(), 1);
        assert_eq!(run("LIBC", "__arm_only").len(), 1);
    }

    /// A version script exports every listed symbol, tags or not; `local: *;` is no symbol.
    #[test]
    fn script_keeps_tagged_symbols() {
        let s = script(MAP).unwrap();
        // MAP lists 7 symbols: malloc, __apex_only, __arm_only, pthread_getname_np, __libc_init_private, __plat, __old
        assert_eq!(s.len(), 7);
        assert!(s.contains(&("LIBC".into(), "__arm_only".into())));
        assert!(s.contains(&("LIBC_DEPRECATED".into(), "__old".into())));
    }

    #[test]
    fn diff_names_both_sides() {
        let pair = |n: &str, s: &str| (n.to_string(), s.to_string());
        let want: Surface = [pair("L", "a"), pair("L", "b")].into();
        let have: Surface = [pair("L", "a"), pair("L", "c")].into();
        assert_eq!(diff(&have, &want), ["missing b@L", "extra c@L"]);
        assert!(diff(&want, &want).is_empty());
    }

    /// Every NDK front the build stages has its version script row.
    #[test]
    fn every_front_has_a_script() {
        for f in crate::gate_goals::NDK_FRONTS {
            assert!(FRONT_SCRIPTS.iter().any(|(m, _)| *m == f), "{f}");
        }
        assert_eq!(FRONT_SCRIPTS.len(), crate::gate_goals::NDK_FRONTS.len());
    }

    #[test]
    fn unknown_lib_fires() {
        let m = BTreeMap::new();
        assert_eq!(
            outside(&[("liblog.so".into(), "LIBLOG".into(), "x".into())], &m).len(),
            1
        );
    }

    /// Real-binary probe: `ATL_NDK_LIB=<.so> ATL_NDK_TREE=<aosp tree> ATL_NDK_ARCH=x86_64 cargo test -p xtask ndk_surface -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_lib() {
        let get = |k| std::env::var(k).unwrap_or_else(|_| panic!("{k} unset"));
        let lib = get("ATL_NDK_LIB");
        let (n, bad) = check(
            Path::new(&lib),
            Path::new(&get("ATL_NDK_TREE")),
            &get("ATL_NDK_ARCH"),
        )
        .unwrap();
        println!("ndk-surface: {lib}: {n} bindings, {} outside", bad.len());
        for b in bad.iter().take(10) {
            println!("  {b}");
        }
        assert!(bad.is_empty());
    }
}
