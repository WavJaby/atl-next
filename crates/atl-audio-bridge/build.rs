// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the adapters in wrap/ with the C++ argv of the three AOSP modules they include (libaudioclient, libaudioprocessing_base, libaudioutils: graph.json `cxx`), so the adapters see AOSP's class layouts.
//! Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The C++ AIDL parcelables wrap/engine.cpp calls (AudioProductStrategy, AudioVolumeGroup, AudioAttributes); see the loop after link_package.
const AIDL_SHARED: [&str; 3] = [
    "device/audiopolicy-types-aidl-cpp",
    "device/audioclient-types-aidl-cpp",
    "device/android.media.audio.common.types-V4-cpp",
];

/// The converter modules whose C++ argv the adapters compile with and whose shared libraries they link. The engine's argv gives wrap/engine.cpp its EngineInterface headers.
const MODULES: [&str; 4] = [
    "device/libaudioclient",
    "device/libaudioprocessing_base",
    "device/libaudioutils",
    "device/libaudiopolicyenginedefault",
];

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src/bridge.rs");
    println!("cargo::rerun-if-changed=wrap");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let out = Converter::from_env()?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/mix.cpp")
        .file("wrap/engine.cpp");
    // every module's flags in order, as atl-media-player passes its two: the adapters include headers from all three. Each token is kept: de-duplicating single tokens drops the second `-include` of a pair, and clang then reads its header as a source file
    for module in MODULES {
        for flag in &out.artifact(module)?.cxx {
            build.flag(flag);
        }
    }
    build.compile("atl_audio_bridge");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // The C++ AIDL parcelables of wrap/engine.cpp: their file names have no `lib` (audioclient-types-aidl-cpp.so), so `-l` cannot name them (converter.rs shared_name). Linked verbatim by path; each directory on the run path.
    for module in AIDL_SHARED {
        let artifact = out.artifact(module)?;
        let dir = artifact
            .out
            .parent()
            .with_context(|| format!("{module}: output without a directory"))?;
        let file = artifact
            .out
            .file_name()
            .and_then(|f| f.to_str())
            .with_context(|| format!("{module}: output without a file name"))?;
        println!("cargo::rustc-link-search=native={}", dir.display());
        println!("cargo::rustc-link-lib=dylib:+verbatim={file}");
        println!(
            "cargo::rustc-link-arg=-Wl,--enable-new-dtags,-rpath,{}",
            dir.display()
        );
    }
    // AOSP's libaudioclient and libaudioprocessing_base are libc++ C++: its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
