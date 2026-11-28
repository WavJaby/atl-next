// SPDX-License-Identifier: MIT
//! The device's ABI lists over arches.toml rows, as AOSP's board_config.mk builds `ro.product.cpu.abilist*` (build/make/core/board_config.mk:355-396, sysprop.mk:58-66).
//! - CPU ABIs: the host row's `android_abis`, 64-bit (one 64-bit process per app: no 32-bit CPU ABI).
//! - bridged ABIs: `android_abis` of each guest row the host row lists in `native_bridge` (the engines its build ships); AOSP's TARGET_NATIVE_BRIDGE_ABI / _2ND_ABI (board_config.mk:340-351).
//! - abilist = CPU, then bridged ("all CPU_ABIs are preferred over all NATIVE_BRIDGE_ABIs", board_config.mk:351); abilist64 = CPU + bridged 64; abilist32 = bridged 32.
//! - Within a class: 64-bit before 32-bit, rows in key order, each row's ABIs preferred first (ATL's rule; AOSP boards bridge one row per width).
//!
//! build.rs includes this file (`#[path]`) and the harness calls [`device_abis`]: std + anyhow only.

use anyhow::{Context, Result, bail, ensure};

/// One arches.toml row, as the rule reads it.
pub struct Row {
    /// The row key: ART's instruction-set name (`arm64`).
    pub key: String,
    pub pointer_bits: u32,
    /// `[<key>.host]` is present.
    pub host: bool,
    /// `[<key>.host] native_bridge`: guest rows whose engine this host ships.
    pub native_bridge: Vec<String>,
    /// `[<key>.guest] android_abis`, preferred first; empty without a guest row.
    pub abis: Vec<String>,
}

/// One device ABI: its APK `lib/<abi>/` name and the row (ISA) that runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abi {
    pub abi: String,
    pub isa: String,
    pub pointer_bits: u32,
}

/// The device's ABIs of host row `host`, preferred first: its CPU ABIs, the bridged 64-bit ones, then the bridged 32-bit ones.
/// Fails on a `native_bridge` name that is no guest row (or is the host's own): a listed ABI must have an engine.
pub fn device_abis(rows: &[Row], host: &str) -> Result<Vec<Abi>> {
    let h = rows
        .iter()
        .find(|r| r.key == host && r.host)
        .with_context(|| format!("{host}: no host row"))?;
    ensure!(h.pointer_bits == 64, "{host}: hosts are 64-bit");
    ensure!(!h.abis.is_empty(), "{host}: no [guest] row (its CPU ABIs)");
    let mut bridged = Vec::new();
    for name in &h.native_bridge {
        ensure!(name != host, "{host}: native_bridge lists its own row");
        let r = rows
            .iter()
            .find(|r| &r.key == name)
            .with_context(|| format!("{host}: native_bridge {name}: no such row"))?;
        ensure!(
            !r.abis.is_empty(),
            "{host}: native_bridge {name}: no [guest] row"
        );
        match r.pointer_bits {
            32 | 64 => bridged.push(r),
            bits => bail!("{}: pointer_bits {bits}", r.key),
        }
    }
    bridged.sort_by(|a, b| a.key.cmp(&b.key));
    ensure!(
        bridged.windows(2).all(|w| w[0].key != w[1].key),
        "{host}: native_bridge lists a row twice"
    );
    let mut out: Vec<Abi> = h
        .abis
        .iter()
        .map(|a| Abi {
            abi: a.clone(),
            isa: host.to_owned(),
            pointer_bits: 64,
        })
        .collect();
    for bits in [64, 32] {
        for r in bridged.iter().filter(|r| r.pointer_bits == bits) {
            out.extend(r.abis.iter().map(|a| Abi {
                abi: a.clone(),
                isa: r.key.clone(),
                pointer_bits: bits,
            }));
        }
    }
    Ok(out)
}

/// `[ro.product.cpu.abilist, abilist64, abilist32]` of a [`device_abis`] result: all ABIs, the 64-bit ones, the 32-bit ones; comma-joined in order. Empty abilist32 on a 64-bit-only device (sysprop.mk).
pub fn lists(abis: &[Abi]) -> [String; 3] {
    let join = |keep: fn(&Abi) -> bool| {
        abis.iter()
            .filter(|a| keep(a))
            .map(|a| a.abi.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    [
        join(|_| true),
        join(|a| a.pointer_bits == 64),
        join(|a| a.pointer_bits == 32),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, bits: u32, host: bool, native_bridge: &[&str], abis: &[&str]) -> Row {
        Row {
            key: key.to_owned(),
            pointer_bits: bits,
            host,
            native_bridge: native_bridge.iter().map(|s| (*s).to_owned()).collect(),
            abis: abis.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// arches.toml's rows (guests arm, arm64, x86, x86_64, riscv64; hosts riscv64 and x86_64) with the given `native_bridge` per host.
    fn arches(riscv64_bridge: &[&str], x86_64_bridge: &[&str]) -> Vec<Row> {
        vec![
            row("arm", 32, false, &[], &["armeabi-v7a", "armeabi"]),
            row("arm64", 64, false, &[], &["arm64-v8a"]),
            row("riscv64", 64, true, riscv64_bridge, &["riscv64"]),
            row("x86", 32, false, &[], &["x86"]),
            row("x86_64", 64, true, x86_64_bridge, &["x86_64"]),
        ]
    }

    fn lists_of(rows: &[Row], host: &str) -> [String; 3] {
        lists(&device_abis(rows, host).expect("device_abis"))
    }

    /// x86_64 host, engines arm64 and arm (both built and booted on x86_64): riscv64 and x86 have no engine, so no list has them.
    #[test]
    fn x86_64_host_lists_its_engines() {
        let rows = arches(&["arm64"], &["arm64", "arm"]);
        assert_eq!(
            lists_of(&rows, "x86_64"),
            [
                "x86_64,arm64-v8a,armeabi-v7a,armeabi",
                "x86_64,arm64-v8a",
                "armeabi-v7a,armeabi",
            ]
        );
    }

    /// riscv64 host, arm64 engine only (measured on riscv64, k3): no x86 ABI, and no 32-bit ABI since the 32-bit engine is not listed.
    #[test]
    fn riscv64_host_lists_arm64_only() {
        let rows = arches(&["arm64"], &["arm64", "arm"]);
        assert_eq!(
            lists_of(&rows, "riscv64"),
            ["riscv64,arm64-v8a", "riscv64,arm64-v8a", ""]
        );
    }

    /// The same host once its 32-bit engine ships: 32-bit ABIs come after every 64-bit one.
    #[test]
    fn riscv64_host_with_a_32_bit_engine() {
        let rows = arches(&["arm64", "arm"], &[]);
        assert_eq!(
            lists_of(&rows, "riscv64"),
            [
                "riscv64,arm64-v8a,armeabi-v7a,armeabi",
                "riscv64,arm64-v8a",
                "armeabi-v7a,armeabi",
            ]
        );
    }

    /// No engine: the CPU ABI alone, and the 64-bit-only device's empty abilist32.
    #[test]
    fn host_without_engines_lists_its_cpu_abi() {
        let rows = arches(&[], &[]);
        assert_eq!(lists_of(&rows, "riscv64"), ["riscv64", "riscv64", ""]);
    }

    #[test]
    fn unknown_or_own_row_fails() {
        let unknown = arches(&["mips"], &[]);
        assert!(device_abis(&unknown, "riscv64").is_err());
        let own = arches(&["riscv64"], &[]);
        assert!(device_abis(&own, "riscv64").is_err());
    }

    #[test]
    fn duplicate_bridged_row_fails() {
        let dup = arches(&["arm64", "arm64"], &[]);
        assert!(device_abis(&dup, "riscv64").is_err());
    }
}
