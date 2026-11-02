// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, from libbinder's `BinderDriver.h` (patches/aosp/frameworks/native 0003), `OUT_DIR/driver.rs`: the check that each `binder_driver_*` export of `crate::exports` has the header's signature (bridge control 1 + 2: shells from the header, layouts from the header).
//! No header is installed: libbinder includes its own.

use anyhow::Result;
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS};

const DRIVER: &[Api] = &[Api {
    name: "driver",
    source: "frameworks/native/libs/binder/BinderDriver.h",
    lang: Lang::Cxx,
    header: None,
    functions: &[
        "binder_driver_open",
        "binder_driver_ioctl",
        "binder_driver_mmap",
        "binder_driver_munmap",
        "binder_driver_close",
        "binder_driver_poll_fd",
    ],
    macros: &[],
    include_dirs: &[],
    kept: DECLARATIONS,
}];

fn main() -> Result<()> {
    for path in buildscript::api_crate(DRIVER, "crate::exports")? {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
