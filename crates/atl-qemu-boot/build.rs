// SPDX-License-Identifier: MIT
//! Declares the break cfgs for the host lint build. The guest build is a direct rustc call (xtask qemu.rs `boot_exe`), which passes `--cfg` and its own `--check-cfg`; keep [`BREAKS`] equal to qemu.rs `BOOT_BREAKS`.

/// The deliberate breaks' cfgs (lib.rs doc).
const BREAKS: &[&str] = &["atl_qb_skip_boot"];

fn main() {
    for cfg in BREAKS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
}
