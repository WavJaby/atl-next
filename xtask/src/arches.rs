// SPDX-License-Identifier: MIT
//! `arches.toml`: the only list of architectures. Column meanings live in that file's header.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const FILE: &str = "arches.toml";

/// Row key = ART instruction-set string.
pub type Arches = BTreeMap<String, Arch>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arch {
    pub pointer_bits: u32,
    pub soong_arch_variant: Option<String>,
    pub kernel_arch: String,
    pub host: Option<Host>,
    pub guest: Option<Guest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "read by xtask deps/fetch/build, not written yet")]
pub struct Host {
    pub rust: String,
    pub meson_cpu_family: String,
    pub debian: String,
    pub archlinux: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "clang/qemu_target read by xtask fetch/build, not written yet"
)]
pub struct Guest {
    pub android_abis: Vec<String>,
    pub clang: String,
    pub qemu_target: String,
}

pub fn load(root: &Path) -> Result<Arches> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str) -> Result<Arches> {
    let arches: Arches = toml::from_str(text)?;
    validate(&arches)?;
    Ok(arches)
}

fn validate(arches: &Arches) -> Result<()> {
    ensure!(!arches.is_empty(), "no arches");
    // an APK ABI must resolve to exactly one arch for the launcher's ABI selection
    let mut abi_owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, arch) in arches {
        ensure!(
            matches!(arch.pointer_bits, 32 | 64),
            "{name}: pointer_bits {} is not 32 or 64",
            arch.pointer_bits
        );
        ensure!(
            arch.host.is_some() || arch.guest.is_some(),
            "{name}: neither [{name}.host] nor [{name}.guest]"
        );
        if let Some(guest) = &arch.guest {
            ensure!(!guest.android_abis.is_empty(), "{name}: empty android_abis");
            for abi in &guest.android_abis {
                if let Some(other) = abi_owner.insert(abi, name) {
                    bail!("android ABI {abi} listed by both {other} and {name}");
                }
            }
        }
    }
    Ok(())
}

/// bionic's build file, an upstream path; it owns the uapi arch directories.
const BIONIC_LIBC_BP: &str = "aosp/bionic/libc/Android.bp";

/// Every row's `kernel_arch` is the `asm-` directory bionic's `libc_uapi_headers` exports for that
/// (Soong) arch, in the bionic checkout `layout` resolves. Needs the fetched tree: build scripts parse that uapi, so nothing builds without it.
pub fn check_kernel_arch(layout: &atl_layout::Layout, arches: &Arches) -> Result<()> {
    let path = layout.locate(Path::new(BIONIC_LIBC_BP)).dir;
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}: run `cargo xtask fetch --execute`",
            path.display()
        )
    })?;
    kernel_arch_matches(&text, arches).with_context(|| format!("{FILE} vs {}", path.display()))
}

fn kernel_arch_matches(android_bp: &str, arches: &Arches) -> Result<()> {
    let start = android_bp
        .find("name: \"libc_uapi_headers\"")
        .context("no libc_uapi_headers module")?;
    // a module ends at the first closing brace in column 0
    let block = &android_bp[start..];
    let block = &block[..block.find("\n}").unwrap_or(block.len())];
    let flat: String = block.chars().filter(|c| !c.is_whitespace()).collect();
    for (name, arch) in arches {
        let entry = format!(
            "{name}:{{export_system_include_dirs:[\"kernel/uapi/asm-{}\"],}}",
            arch.kernel_arch
        );
        ensure!(
            flat.contains(&entry),
            "{name}: kernel_arch {} is not what libc_uapi_headers exports for {name}",
            arch.kernel_arch
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{kernel_arch_matches, parse};

    const GUEST: &str = "android_abis = [\"x\"]\nclang = \"c\"\nqemu_target = \"q\"\n";
    const K: &str = "kernel_arch = \"k\"\n";

    #[test]
    fn kernel_arch_against_bionic() {
        let bp = "cc_library_headers {\n    name: \"libc_uapi_headers\",\n    arch: {\n        a: {\n            export_system_include_dirs: [\"kernel/uapi/asm-ka\"],\n        },\n    },\n}\n\ncc_library_headers {\n    name: \"other\",\n    arch: {\n        a: {\n            export_system_include_dirs: [\"kernel/uapi/asm-wrong\"],\n        },\n    },\n}\n";
        let row = |k: &str| {
            parse(&format!(
                "[a]\npointer_bits = 32\nkernel_arch = \"{k}\"\n[a.guest]\n{GUEST}"
            ))
            .unwrap()
        };
        kernel_arch_matches(bp, &row("ka")).unwrap();
        // another module's block does not count
        assert!(kernel_arch_matches(bp, &row("wrong")).is_err());
    }

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be rejected"))
    }

    #[test]
    fn minimal_guest_row() {
        let arches = parse(&format!("[a]\npointer_bits = 32\n{K}[a.guest]\n{GUEST}")).unwrap();
        assert_eq!(arches["a"].pointer_bits, 32);
        assert!(arches["a"].host.is_none());
    }

    #[test]
    fn row_without_role() {
        assert!(err(&format!("[a]\npointer_bits = 64\n{K}")).contains("neither"));
    }

    #[test]
    fn odd_pointer_width() {
        let text = format!("[a]\npointer_bits = 48\n{K}[a.guest]\n{GUEST}");
        assert!(err(&text).contains("pointer_bits 48"));
    }

    #[test]
    fn unknown_column() {
        let text = format!("[a]\npointer_bits = 64\n{K}priority = 1\n[a.guest]\n{GUEST}");
        assert!(err(&text).contains("unknown field `priority`"));
    }

    #[test]
    fn abi_in_two_rows() {
        let text = format!(
            "[a]\npointer_bits = 64\n{K}[a.guest]\n{GUEST}[b]\npointer_bits = 64\n{K}[b.guest]\n{GUEST}"
        );
        assert!(err(&text).contains("listed by both a and b"));
    }
}
