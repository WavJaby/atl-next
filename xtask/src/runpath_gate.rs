// SPDX-License-Identifier: MIT
//! Gate after `xtask native`'s build: no converter-built device or native-bridge artifact, nor converter-built Rust package library, carries a DT_RUNPATH or DT_RPATH.
//! Soong gives only host binaries an rpath (build/soong/cc/linker.go linkerFlags: `ctx.Host() && !ctx.Windows() && !staticLib` -> RpathFlags, `$ORIGIN`-relative); device and native-bridge ones get none, so the linker's search path decides (here LD_LIBRARY_PATH over the stage, bionic's linker config for guests).
//! Why a gate: an absolute build-tree runpath lets a library missing from the stage load silently from the build tree, so a runtime gate passes on a stage that would fail installed (seen: leaving `libc++.so` out of the guest stage stayed green).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use object::read::elf::{ElfFile, FileHeader};
use object::{Endianness, elf};

/// The link outputs of this out dir's build.ninja under any of `under` that exist (`ninja -t targets rule link`): the current graph's artifacts, not leftovers of an earlier goal set.
pub(crate) fn link_outputs(out: &Path, under: &[PathBuf]) -> Result<Vec<PathBuf>> {
    Ok(crate::licence_gate::lines(&crate::licence_gate::ninja(
        out,
        &["-t", "targets", "rule", "link"],
    )?)
    .map(PathBuf::from)
    .filter(|p| under.iter().any(|u| p.starts_with(u)) && p.is_file())
    .collect())
}

/// The device link outputs the licence edge lists (`deviceLinks`, bp2ninja licence.go: cc links and rustc binaries), as its explicit inputs: the list the licence gate reads.
/// `link_outputs` (`rule link`) misses rustc's binaries. No licence edge: ninja errors (no silent empty list).
pub(crate) fn device_links(out: &Path) -> Result<Vec<PathBuf>> {
    let report = out.join(crate::native::LICENCE_REPORT);
    let query = crate::licence_gate::ninja(out, &["-t", "query", &report.display().to_string()])?;
    Ok(crate::licence_gate::parse_query(&query)
        .into_values()
        .flatten()
        .filter(|p| p.is_file())
        .collect())
}

/// `artifacts` split into (fresh, stale): stale = a failed build left an earlier link's output (`relinked`).
pub(crate) fn fresh(out: &Path, artifacts: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let stale = relinked(out, &artifacts);
    artifacts.into_iter().partition(|a| !stale.contains(a))
}

/// The artifacts whose link edge ninja would run (dry run, `-k 0`: past edges that would fail): those a failed build left as an earlier link's output.
/// Read from the dry run's `[i/n] link <out>` lines, bp2ninja's link rule description. A dry run that cannot start leaves every artifact stale: what it would have told is unknown.
fn relinked(out: &Path, artifacts: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    if artifacts.is_empty() {
        return std::collections::BTreeSet::new();
    }
    // its exit status is not read: a dry run fails where a missing input has no rule, and still prints every edge it would run
    let Ok(output) = std::process::Command::new("ninja")
        .arg("-C")
        .arg(out)
        .args(["-n", "-k", "0"])
        .args(artifacts)
        .output()
    else {
        return artifacts.iter().cloned().collect();
    };
    crate::licence_gate::lines(&String::from_utf8_lossy(&output.stdout))
        .filter_map(|l| {
            let (_, rest) = l.split_once("] link ")?;
            l.starts_with('[').then(|| PathBuf::from(rest))
        })
        .collect()
}

/// Every runpath entry of `artifacts` (the fresh link outputs under `<out>/device` and `<out>/nb/<arch>`, `link_outputs`), as `<artifact>: <tag> <value>`; plus how many ELF artifacts were read.
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

/// The files in `<crates>/<package>/lib/` (converter-built Rust packages' installed libraries); none when `crates` does not exist yet.
pub fn package_libs(crates: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(packages) = std::fs::read_dir(crates) else {
        return Ok(out);
    };
    for p in packages {
        let lib = p?.path().join("lib");
        let Ok(files) = std::fs::read_dir(&lib) else {
            continue;
        };
        for f in files {
            let f = f?.path();
            if f.is_file() {
                out.push(f);
            }
        }
    }
    out.sort();
    Ok(out)
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

    /// A rustc-linked device binary with a build-tree runpath is red. `device_links` lists it (the licence edge's inputs); `link_outputs` (`rule link`) does not, rustc's rule not being `link`.
    #[test]
    fn rust_binary_runpath_is_red() {
        let tmp = std::env::temp_dir().join(format!("xtask-runpath-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (src, out) = (tmp.join("src"), tmp.join("out"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(out.join("device/app")).unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        let rustc = crate::native::rustc(&crate::test_root()).unwrap();
        let (app, report) = (
            out.join("device/app/app"),
            out.join(crate::native::LICENCE_REPORT),
        );
        std::fs::write(
            out.join("build.ninja"),
            format!(
                "rule rustc\n  command = {} --crate-type=bin --edition=2021 -C linker=clang++-19 -C link-arg=-fuse-ld=lld -C link-arg=-Wl,-rpath,/nonexistent/build/lib -o $out $in\nrule licence_gate\n  command = true\nbuild {}: rustc {}\nbuild {}: licence_gate {}\n",
                rustc.display(),
                app.display(),
                src.join("main.rs").display(),
                report.display(),
                app.display(),
            ),
        )
        .unwrap();
        let built = std::process::Command::new("ninja")
            .arg("-C")
            .arg(&out)
            .arg(&app)
            .status()
            .unwrap();
        assert!(built.success());
        let listed = device_links(&out).unwrap();
        let by_rule = link_outputs(&out, &[out.join("device")]).unwrap();
        let (read, violations) = check(&out, &listed).unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(listed, [app]);
        assert!(by_rule.is_empty(), "{by_rule:?}");
        assert_eq!(read, 1);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("/nonexistent/build/lib"),
            "{violations:?}"
        );
    }
}
