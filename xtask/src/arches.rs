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

#[cfg(test)]
mod tests {
    use super::parse;

    const GUEST: &str = "android_abis = [\"x\"]\nclang = \"c\"\nqemu_target = \"q\"\n";

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be rejected"))
    }

    #[test]
    fn minimal_guest_row() {
        let arches = parse(&format!("[a]\npointer_bits = 32\n[a.guest]\n{GUEST}")).unwrap();
        assert_eq!(arches["a"].pointer_bits, 32);
        assert!(arches["a"].host.is_none());
    }

    #[test]
    fn row_without_role() {
        assert!(err("[a]\npointer_bits = 64\n").contains("neither"));
    }

    #[test]
    fn odd_pointer_width() {
        let text = format!("[a]\npointer_bits = 48\n[a.guest]\n{GUEST}");
        assert!(err(&text).contains("pointer_bits 48"));
    }

    #[test]
    fn unknown_column() {
        let text = format!("[a]\npointer_bits = 64\npriority = 1\n[a.guest]\n{GUEST}");
        assert!(err(&text).contains("unknown field `priority`"));
    }

    #[test]
    fn abi_in_two_rows() {
        let text = format!(
            "[a]\npointer_bits = 64\n[a.guest]\n{GUEST}[b]\npointer_bits = 64\n[b.guest]\n{GUEST}"
        );
        assert!(err(&text).contains("listed by both a and b"));
    }
}
