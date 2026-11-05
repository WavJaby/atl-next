// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, from AOSP's stable-C IMapper header (`IMapper.h`, hardware/interfaces/graphics/mapper), `OUT_DIR/imapper.rs`:
//! the layout of every record the interface passes (`AIMapper`, `AIMapperV5` with its function-pointer table, `AIMapper_MetadataType`, `AIMapper_MetadataTypeDescription`, `ARect`, `native_handle_t`) with clang's sizes and offsets asserted,
//! the enumerators and constants, the check that `crate::exports::AIMapper_loadIMapper` has the header's signature, and the entry shells of the `AIMapperV5` table (`Table`): one `unsafe extern "C"` function per field with the header's exact signature, panic-shielded, calling `crate::exports::<field>`, and `AIMapperV5::shells()` (bridge controls 1 and 2).
//! `ARect` and `native_handle_t` are declared in two other headers (`android/rect.h`, `cutils/native_handle.h`); they are part of the interface (`Source::also`), which `buildscript::Api` cannot say, so this script calls the generator directly.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::ir::Ty;
use atl_bridgegen::rust::{Table, api_module_with_table};
use atl_bridgegen::{Lang, Source};

/// The interface header, relative to the AOSP tree.
const HEADER: &str = "hardware/interfaces/graphics/mapper/stable-c/include/android/hardware/graphics/mapper/IMapper.h";
/// Headers declaring types the interface passes (`ARect`, `native_handle_t` / `buffer_handle_t`), and the include directories the three need.
const ALSO: &[&str] = &[
    "frameworks/native/libs/arect/include/android/rect.h",
    "system/core/libcutils/include/cutils/native_handle.h",
];
const INCLUDE_DIRS: &[&str] = &[
    "frameworks/native/libs/arect/include",
    "system/core/libcutils/include",
];
/// The functions the shared library exports with the header's signature.
const EXPORTS: &[&str] = &["AIMapper_loadIMapper"];

/// What a panic in an entry answers (IMapper.h): the getters return a size or the negative of an error, every other entry an `AIMapper_Error`; `listSupportedMetadataTypes` is documented to answer `UNSUPPORTED` "if there's any error", the rest `NO_RESOURCES` ("resource unavailable").
fn on_panic(field: &str, _ret: &Ty) -> Result<String> {
    Ok(match field {
        "getMetadata" | "getStandardMetadata" => "-AIMAPPER_ERROR_NO_RESOURCES",
        "listSupportedMetadataTypes" => "AIMAPPER_ERROR_UNSUPPORTED",
        _ => "AIMAPPER_ERROR_NO_RESOURCES",
    }
    .to_owned())
}

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    for f in std::iter::once(&HEADER).chain(ALSO) {
        up.file(&format!("aosp/{f}"))?;
    }
    let cxx = buildscript::libcxx_include_dir()?;
    let mut target = buildscript::bionic_libc_target(&up, &host, Some(&cxx));
    target.args.extend(
        INCLUDE_DIRS
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display())),
    );
    let path = up.aosp(HEADER);
    let also: Vec<PathBuf> = ALSO.iter().map(|f| up.aosp(f)).collect();
    let generate = || -> Result<String> {
        let src = Source {
            path: &path,
            lang: Lang::Cxx,
            also: &also,
            macros: &[],
        };
        let iface =
            atl_bridgegen::interface("imapper", HEADER, &src, std::slice::from_ref(&target))
                .context(HEADER)?;
        api_module_with_table(
            &iface,
            &host.arch,
            EXPORTS,
            &[],
            "crate::exports",
            &Table {
                record: "AIMapperV5",
                implementation: "crate::exports",
                on_panic: &on_panic,
                values: &[],
            },
        )
    };
    let code = generate()?;
    ensure!(
        code == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("imapper.rs"), code)?;
    println!("cargo::rerun-if-changed={}", arches.display());
    for rel in std::iter::once(&HEADER).chain(ALSO) {
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
    Ok(())
}
