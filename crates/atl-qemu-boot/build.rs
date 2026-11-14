// SPDX-License-Identifier: MIT
//! Declares the cfgs for the host lint build. The guest build is a direct rustc call (xtask qemu.rs `guest_exe`), which passes `--cfg`; keep [`CFGS`] equal to qemu.rs `CPUINFO` + `SKIP_BOOT`.

/// The entry selector and the deliberate break (lib.rs doc).
const CFGS: &[&str] = &["atl_qb_cpuinfo", "atl_qb_skip_boot"];

fn main() {
    for cfg in CFGS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
}
