// SPDX-License-Identifier: MIT
//! Declares `cfg(soong)` (Soong's and the converter's global rustc flag, lib.rs's gate) so cargo's check-cfg accepts it.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(soong)");
}
