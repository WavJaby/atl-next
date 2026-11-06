// SPDX-License-Identifier: MIT
//! Gate after `xtask native`'s build: no converter-built device or native-bridge artifact carries a DT_RUNPATH or DT_RPATH.
//! Soong gives only host binaries an rpath (build/soong/cc/linker.go linkerFlags: `ctx.Host() && !ctx.Windows() && !staticLib` -> RpathFlags, `$ORIGIN`-relative); device and native-bridge ones get none, so the linker's search path decides (here LD_LIBRARY_PATH over the stage, bionic's linker config for guests).
//! Why a gate: an absolute build-tree runpath lets a library missing from the stage load silently from the build tree, so a runtime gate passes on a stage that would fail installed (seen: leaving `libc++.so` out of the guest stage stayed green).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use object::read::elf::{ElfFile, FileHeader};
use object::{Endianness, elf};

/// Every runpath entry of `artifacts` (the fresh link outputs under `<out>/device` and `<out>/nb/<arch>`, licence_gate::link_outputs), as `<artifact>: <tag> <value>`; plus how many ELF artifacts were read.
pub fn check(out: &Path, artifacts: &[PathBuf]) -> Result<(usize, Vec<String>)> {
    let mut read = 0;
    let mut violations = Vec::new();
    for path in artifacts {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let Some(paths) =
            runpaths(&data).with_context(|| format!("{}: dynamic section", path.display()))?
        else {
            continue;
        };
        read += 1;
        let name = path.strip_prefix(out).unwrap_or(path).display().to_string();
        violations.extend(paths.into_iter().map(|p| format!("{name}: {p}")));
    }
    Ok((read, violations))
}

/// The DT_RUNPATH / DT_RPATH entries of an ELF (either class); None for a file that is not ELF.
fn runpaths(data: &[u8]) -> Result<Option<Vec<String>>> {
    if let Ok(f) = ElfFile::<elf::FileHeader64<Endianness>>::parse(data) {
        return entries(&f, data).map(Some);
    }
    if let Ok(f) = ElfFile::<elf::FileHeader32<Endianness>>::parse(data) {
        return entries(&f, data).map(Some);
    }
    Ok(None)
}

fn entries<H: FileHeader<Endian = Endianness>>(f: &ElfFile<H>, data: &[u8]) -> Result<Vec<String>> {
    let table = f.elf_section_table().dynamic_table(f.endian(), data)?;
    let mut out = Vec::new();
    for d in &table {
        let name = if d.tag == elf::DT_RUNPATH {
            "DT_RUNPATH"
        } else if d.tag == elf::DT_RPATH {
            "DT_RPATH"
        } else {
            continue;
        };
        out.push(format!(
            "{name} {}",
            String::from_utf8_lossy(table.string(d)?)
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built xtask binary itself: a host ELF from cargo, no runpath (cargo sets none) -> Some(empty); a non-ELF -> None.
    #[test]
    fn reads_elf_and_skips_other_files() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(runpaths(&exe).unwrap().is_some());
        assert!(runpaths(b"#!/bin/sh\n").unwrap().is_none());
    }
}
