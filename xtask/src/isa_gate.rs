// SPDX-License-Identifier: MIT
//! Static gate over a cross host's ELF outputs: `xtask native --host <arch>` runs it after the build, `xtask isa-gate --arch <arch>` alone.
//! Every riscv64 ELF64 file under the arch's out dir must claim only the ISA its arches.toml row allows (`Tag_RISCV_arch`, implied extensions included: arches::isa_closure), and every GLIBC_x.y version its undefined dynamic symbols need must be at or below the row's `glibc_floor`.
//! It reads what the object declares, not what its code executes: `Tag_RISCV_arch` is the assembler's -march, so a TU built with a wider -march fails even if it never runs the extension.
//! Run-time tiers (isa-tiers.toml, isa_tiers.rs): an object named by a row may claim the row's extension and must claim it; a product of the row's module may claim it; nothing else may.
//! Not checked: static archives (object is built without its `archive` feature), GLIBCXX_/CXXABI_/GCC_ needs, GTK (no riscv64 output links it yet). GLIBC_PRIVATE is the loader's own interface, not a number: skipped.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use object::read::SymbolIndex;
use object::read::elf::{ElfFile64, FileHeader, SectionHeader, Sym};
use object::{Endianness, elf};

use crate::{arches, isa_tiers, slot};

/// psABI build attribute `Tag_RISCV_arch`: an odd tag, so its value is a NUL-terminated string.
const TAG_RISCV_ARCH: u64 = 5;
/// `e_ident` index of the class byte (ELF gABI).
const EI_CLASS: usize = 4;

#[derive(clap::Args)]
pub struct Args {
    /// The arch of arches.toml whose `[host]` row (isa, glibc_floor) the outputs are checked against.
    #[arg(long)]
    arch: String,
    /// Build dir root, as `native --out` (default: this checkout's slot); the arch's outputs are `<out>/<arch>`.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// One undefined dynamic symbol's version need: the symbol, its version (`GLIBC_2.41`) and the library providing it.
#[derive(Debug, PartialEq)]
pub struct Need {
    pub symbol: String,
    pub version: String,
    pub file: String,
}

/// What the gate read and found under one out dir.
#[derive(Default)]
pub struct Report {
    /// riscv64 ELF64 files read.
    pub files: usize,
    /// Each distinct `Tag_RISCV_arch` string seen.
    pub isas: BTreeSet<String>,
    /// Highest GLIBC version any output needs, with the file and symbol that need it.
    pub max_glibc: Option<(Vec<u32>, String)>,
    /// One line per violation, prefixed with its file.
    pub violations: Vec<String>,
    /// Files a tier row let claim its extension: `<module> <src> (<ext>)` for an object, `<module> (<ext>)` for a product.
    pub tiered: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let arches = arches::load(root)?;
    let host = arches
        .get(&args.arch)
        .and_then(|a| a.host.as_ref())
        .with_context(|| format!("{}: no [{}.host] row", arches::FILE, args.arch))?;
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |workspace| {
        workspace.join("out").join("native")
    })?
    .join(&args.arch);
    let report = check(
        &out,
        host,
        &isa_tiers::load(root)?,
        &isa_tiers::current_objects(&out)?,
    )?;
    println!("isa gate: {}", summary(&report));
    for v in report.violations.iter().take(5) {
        println!("isa gate:   {v}");
    }
    Ok(if report.violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The gate over every riscv64 ELF64 under `out` (symlinks skipped) against `host`'s isa and glibc_floor, and `tiers` (isa-tiers.toml) for the files they let claim more.
/// `current`: this graph's tier objects (isa_tiers::current_objects), which must claim their row's extension; stale objects of earlier graphs in `out` are only held to the closure.
pub fn check(
    out: &Path,
    host: &arches::Host,
    tiers: &isa_tiers::Tiers,
    current: &BTreeSet<String>,
) -> Result<Report> {
    let (Some(isa), Some(floor)) = (&host.isa, &host.glibc_floor) else {
        bail!(
            "{}: the isa gate needs the row's isa and glibc_floor",
            arches::FILE
        );
    };
    let allowed = arches::isa_closure(isa)?;
    let floor = arches::dotted_version(floor)?;
    let mut paths = Vec::new();
    collect(out, &mut paths)?;
    paths.sort();
    let mut report = Report::default();
    for path in &paths {
        let name = path.strip_prefix(out).unwrap_or(path).display().to_string();
        let data = std::fs::read(path).with_context(|| format!("reading {name}"))?;
        let Some(f) = riscv64_elf(&data).with_context(|| name.clone())? else {
            continue;
        };
        report.files += 1;
        let claim = isa_claim(&f, &data).with_context(|| name.clone())?;
        let (violations, tier) =
            file_check(&name, claim.as_deref(), tiers, isa, &allowed, current)?;
        report.violations.extend(violations);
        report.tiered.extend(tier);
        let needs = glibc_needs(&f, &data).with_context(|| name.clone())?;
        for need in &needs {
            let Some(version) = glibc_version(&need.version)? else {
                continue;
            };
            if report
                .max_glibc
                .as_ref()
                .is_none_or(|(max, _)| version > *max)
            {
                report.max_glibc =
                    Some((version, format!("{name}: {}@{}", need.symbol, need.version)));
            }
        }
        report.violations.extend(
            glibc_violations(&needs, &floor)?
                .into_iter()
                .map(|v| format!("{name}: {v}")),
        );
        if let Some(claim) = claim {
            report.isas.insert(claim);
        }
    }
    if report.files == 0 {
        report.violations.push(format!(
            "no riscv64 ELF64 under {}: nothing checked",
            out.display()
        ));
    }
    Ok(report)
}

/// The gate's one-line summary, as `native` and `isa-gate` print it.
pub fn summary(report: &Report) -> String {
    let isas: Vec<&str> = report.isas.iter().map(String::as_str).collect();
    let max = report.max_glibc.as_ref().map_or_else(
        || "none".to_owned(),
        |(version, at)| format!("GLIBC_{} ({at})", dotted(version)),
    );
    format!(
        "{} riscv64 ELF64 files, ISA [{}], max {max}, {} violations, tiers [{}]",
        report.files,
        isas.join(" | "),
        report.violations.len(),
        report.tiered.join(" | ")
    )
}

/// One file's violations (prefixed with its name), and the tier row that let it claim its extension. The file is allowed the row's closure when a row names it: an object by its src, a product by its module.
/// An object of this graph (`current`) that a row names must claim the row's extension: without it the row did not reach the source.
fn file_check(
    name: &str,
    claim: Option<&str>,
    tiers: &isa_tiers::Tiers,
    isa: &str,
    allowed: &BTreeSet<String>,
    current: &BTreeSet<String>,
) -> Result<(Vec<String>, Option<String>)> {
    let row = isa_tiers::object_row(tiers, name)
        .map(|(module, row)| (module, row, true))
        .or_else(|| isa_tiers::product_row(tiers, name).map(|(module, row)| (module, row, false)));
    let Some((module, row, object)) = row else {
        let out = isa_violations(claim, allowed, isa)
            .into_iter()
            .map(|v| format!("{name}: {v}"));
        return Ok((out.collect(), None));
    };
    let set = isa_tiers::closure(isa, &row.ext)?;
    let mut out: Vec<String> = isa_violations(claim, &set, isa)
        .into_iter()
        .map(|v| format!("{name}: {v}"))
        .collect();
    if object && current.contains(name) && claim.is_some_and(|c| !claims(c, &row.ext)) {
        out.push(format!(
            "{name}: tier `{module}` {} does not claim `{}`: the row did not reach its source",
            row.src, row.ext
        ));
    }
    let label = if object {
        format!("{module} {} ({})", row.src, row.ext)
    } else {
        format!("{module} ({})", row.ext)
    };
    Ok((out, Some(label)))
}

/// Whether a Tag_RISCV_arch string names the extension (`v1p0` -> `v`).
fn claims(claim: &str, ext: &str) -> bool {
    claim.split('_').any(|part| ext_name(part) == ext)
}

/// Every regular file under `dir`, recursively; symlinks are not followed.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// The ELF64 view of `data` when it is a riscv64 ELF64; None for any other file, 32-bit ELFs included.
fn riscv64_elf(data: &[u8]) -> Result<Option<ElfFile64<'_, Endianness>>> {
    if !data.starts_with(&elf::ELFMAG) || data.get(EI_CLASS) != Some(&elf::ELFCLASS64.0) {
        return Ok(None);
    }
    let f = ElfFile64::<Endianness>::parse(data)?;
    Ok((f.elf_header().e_machine(f.endian()) == elf::EM_RISCV).then_some(f))
}

/// `Tag_RISCV_arch` of `f`: the ISA string it claims; None when it carries no RISC-V attributes.
fn isa_claim<'data>(f: &ElfFile64<'data, Endianness>, data: &'data [u8]) -> Result<Option<String>> {
    let endian = f.endian();
    for section in f.elf_section_table().iter() {
        if section.sh_type(endian) != elf::SHT_RISCV_ATTRIBUTES {
            continue;
        }
        let attributes = section.attributes(endian, data)?;
        let mut subsections = attributes.subsections()?;
        while let Some(subsection) = subsections.next()? {
            if subsection.vendor() != b"riscv" {
                continue;
            }
            let mut subsubsections = subsection.subsubsections();
            while let Some(subsubsection) = subsubsections.next()? {
                if subsubsection.tag() != elf::Tag_File {
                    continue;
                }
                let mut reader = subsubsection.attributes();
                while let Some(tag) = reader.read_tag()? {
                    if tag == TAG_RISCV_ARCH {
                        return Ok(Some(
                            String::from_utf8_lossy(reader.read_string()?).into_owned(),
                        ));
                    }
                    // psABI: odd tags carry a string, even tags a ULEB128
                    if tag % 2 == 1 {
                        reader.read_string()?;
                    } else {
                        reader.read_integer()?;
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Messages for the claim's extensions outside `allowed` (the row's isa `row`); a missing claim or a non-rv64 one fails too.
fn isa_violations(claim: Option<&str>, allowed: &BTreeSet<String>, row: &str) -> Vec<String> {
    let Some(claim) = claim else {
        return vec![format!(
            "no Tag_RISCV_arch: the ISA cannot be checked against `{row}`"
        )];
    };
    let mut parts = claim.split('_');
    let base = parts.next().unwrap_or_default();
    let Some(base) = base.strip_prefix("rv64") else {
        return vec![format!("Tag_RISCV_arch `{claim}`: not an rv64 ISA string")];
    };
    std::iter::once(ext_name(base))
        .chain(parts.map(ext_name))
        .filter(|name| !allowed.contains(*name))
        .map(|name| format!("Tag_RISCV_arch `{claim}`: extension `{name}` is outside isa `{row}`"))
        .collect()
}

/// `zba1p0` -> `zba`, `m2p0` -> `m`: the name without its `<major>p<minor>` version (every element of a Tag_RISCV_arch string has one).
fn ext_name(part: &str) -> &str {
    match part.rsplit_once('p') {
        Some((name, minor)) if !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()) => {
            name.trim_end_matches(|c: char| c.is_ascii_digit())
        }
        _ => part,
    }
}

/// The undefined dynamic symbols of `f` that carry a version (`.gnu.version_r` needs), named.
fn glibc_needs<'data>(f: &ElfFile64<'data, Endianness>, data: &'data [u8]) -> Result<Vec<Need>> {
    let endian = f.endian();
    let Some(versions) = f.elf_section_table().versions(endian, data)? else {
        return Ok(Vec::new());
    };
    let dynsym = f.elf_dynamic_symbol_table();
    let strings = dynsym.strings();
    let mut needs = Vec::new();
    for (index, sym) in dynsym.iter().enumerate() {
        if !sym.is_undefined(endian) {
            continue;
        }
        let index = versions.version_index(endian, SymbolIndex(index)).index();
        let Some(version) = versions.version(index)? else {
            continue;
        };
        needs.push(Need {
            symbol: String::from_utf8_lossy(sym.name(endian, strings)?).into_owned(),
            version: String::from_utf8_lossy(version.name()).into_owned(),
            file: version.file().map_or_else(String::new, |file| {
                String::from_utf8_lossy(file).into_owned()
            }),
        });
    }
    Ok(needs)
}

/// Messages for each GLIBC_ need above `floor`.
fn glibc_violations(needs: &[Need], floor: &[u32]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for need in needs {
        let Some(version) = glibc_version(&need.version)? else {
            continue;
        };
        if version.as_slice() > floor {
            out.push(format!(
                "{}@{} (from {}) needs glibc {} above glibc_floor {}",
                need.symbol,
                need.version,
                need.file,
                dotted(&version),
                dotted(floor)
            ));
        }
    }
    Ok(out)
}

/// `GLIBC_2.39` -> `[2, 39]`. Other namespaces and `GLIBC_PRIVATE` are not version numbers: None. A GLIBC_ name with non-numeric parts is an error.
fn glibc_version(name: &str) -> Result<Option<Vec<u32>>> {
    match name.strip_prefix("GLIBC_") {
        None | Some("PRIVATE") => Ok(None),
        Some(dotted) => arches::dotted_version(dotted)
            .map(Some)
            .with_context(|| format!("version `{name}` is not GLIBC_<dotted numbers>")),
    }
}

fn dotted(version: &[u32]) -> String {
    version
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An rv64gc_zba_zbb claim with the psABI `<name><major>p<minor>` version form; the versions are not what the gate checks.
    const RV64GC_ZBA_ZBB: &str = "rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zifencei2p0_zmmul1p0_zaamo1p0_zalrsc1p0_zba1p0_zbb1p0";

    fn allowed() -> BTreeSet<String> {
        arches::isa_closure("rv64gc_zba_zbb").unwrap()
    }

    #[test]
    fn claim_inside_the_row_passes() {
        assert!(isa_violations(Some(RV64GC_ZBA_ZBB), &allowed(), "rv64gc_zba_zbb").is_empty());
    }

    #[test]
    fn vector_and_zbs_claims_fail_by_name() {
        let claim = "rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_v1p0_zvl128b1p0_zve64d1p0_zbs1p0";
        let got = isa_violations(Some(claim), &allowed(), "rv64gc_zba_zbb");
        for ext in ["`v`", "`zvl128b`", "`zve64d`", "`zbs`"] {
            assert!(
                got.iter().any(|m| m.contains(&format!("extension {ext}"))),
                "{ext}: {got:?}"
            );
        }
        assert_eq!(got.len(), 4, "{got:?}");
    }

    #[test]
    fn missing_or_non_rv64_claim_fails() {
        assert!(
            isa_violations(None, &allowed(), "rv64gc_zba_zbb")[0].contains("no Tag_RISCV_arch")
        );
        assert!(
            isa_violations(Some("rv32i2p1_m2p0"), &allowed(), "rv64gc_zba_zbb")[0]
                .contains("not an rv64")
        );
    }

    #[test]
    fn ext_names_drop_their_versions() {
        assert_eq!(ext_name("zba1p0"), "zba");
        assert_eq!(ext_name("m2p0"), "m");
        assert_eq!(ext_name("sscofpmf0p9"), "sscofpmf");
        assert_eq!(ext_name("sscofpmf"), "sscofpmf");
    }

    #[test]
    fn glibc_need_above_floor_names_symbol_and_version() {
        let need = |symbol: &str, version: &str| Need {
            symbol: symbol.to_owned(),
            version: version.to_owned(),
            file: "libc.so.6".to_owned(),
        };
        let needs = [
            need("__libc_start_main", "GLIBC_2.41"),
            need("memcpy", "GLIBC_2.39"),
            need("_dl_find_object", "GLIBC_PRIVATE"),
        ];
        let got = glibc_violations(&needs, &arches::dotted_version("2.39").unwrap()).unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("__libc_start_main@GLIBC_2.41"), "{got:?}");
        assert!(
            got[0].contains("needs glibc 2.41 above glibc_floor 2.39"),
            "{got:?}"
        );
    }

    #[test]
    fn host_elf_reads_its_glibc_needs_and_is_not_riscv() {
        let data = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(riscv64_elf(&data).unwrap().is_none());
        let f = ElfFile64::<Endianness>::parse(&*data).unwrap();
        let needs = glibc_needs(&f, &data).unwrap();
        assert!(
            needs
                .iter()
                .any(|n| glibc_version(&n.version).unwrap().is_some())
        );
        assert!(isa_claim(&f, &data).unwrap().is_none());
    }

    #[test]
    fn non_elf_is_not_read() {
        assert!(riscv64_elf(b"#!/bin/sh\n").unwrap().is_none());
    }

    /// The libz row as isa-tiers.toml gives it.
    fn tiers() -> isa_tiers::Tiers {
        toml::from_str(
            "[libz]\nsrc = \"adler32_simd.c\"\next = \"v\"\ngate = \"adler32.c:riscv_cpu_enable_rvv\"\nreason = \"r\"\n",
        )
        .unwrap()
    }

    /// The claim written for -march=rv64gcv_zba_zbb: V, then the names V implies (RVV 1.0 rules, Zvl128b, Zve64d, Zve64f, Zve64x, Zve32f, Zve32x, Zvl64b, Zvl32b), spelled as Tag_RISCV_arch spells them. Written from the rules, not read from clang; the riscv64 run's readelf -A confirms it.
    const V_CLAIM: &str = "rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_v1p0_zvl128b1p0_zve32x1p0_zve64d1p0_zve64f1p0_zve64x1p0_zve32f1p0_zvl32b1p0_zvl64b1p0_zba1p0_zbb1p0";

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn tier_source_may_claim_v_and_must() {
        let host = "obj/host/libz/static/external/zlib/adler32_simd.c.o";
        let (violations, tier) = file_check(
            "obj/device/libz/shared/external/zlib/adler32_simd.c.o",
            Some(V_CLAIM),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[]),
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(tier.as_deref(), Some("libz adler32_simd.c (v)"));
        // this graph's tier object built without the extension: the row did not reach it
        let (violations, _) = file_check(
            host,
            Some(RV64GC_ZBA_ZBB),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[host]),
        )
        .unwrap();
        assert!(
            violations.iter().any(|v| v.contains("does not claim `v`")),
            "{violations:?}"
        );
        // the same object left in the out dir by an earlier graph is not held to it
        let (violations, tier) = file_check(
            host,
            Some(RV64GC_ZBA_ZBB),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[]),
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(tier.as_deref(), Some("libz adler32_simd.c (v)"));
    }

    #[test]
    fn other_source_of_the_module_may_not_claim_v() {
        let (violations, tier) = file_check(
            "obj/device/libz/shared/external/zlib/adler32.c.o",
            Some(V_CLAIM),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[]),
        )
        .unwrap();
        assert!(tier.is_none());
        assert!(
            violations
                .iter()
                .any(|v| v.contains("extension `v` is outside isa `rv64gc_zba_zbb`")),
            "{violations:?}"
        );
    }

    #[test]
    fn product_of_the_row_module_may_claim_v_others_may_not() {
        let (violations, tier) = file_check(
            "device/libz/libz.so",
            Some(V_CLAIM),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[]),
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(tier.as_deref(), Some("libz (v)"));
        let (violations, tier) = file_check(
            "device/libzz/libz.so",
            Some(V_CLAIM),
            &tiers(),
            "rv64gc_zba_zbb",
            &allowed(),
            &set(&[]),
        )
        .unwrap();
        assert!(tier.is_none());
        assert!(!violations.is_empty());
    }

    #[test]
    fn tier_closure_covers_the_claim_v_makes_and_no_more() {
        let closure = isa_tiers::closure("rv64gc_zba_zbb", "v").unwrap();
        assert!(isa_violations(Some(V_CLAIM), &closure, "rv64gc_zba_zbb").is_empty());
        let zbs = "rv64i2p1_zbs1p0";
        assert_eq!(
            isa_violations(Some(zbs), &closure, "rv64gc_zba_zbb").len(),
            1
        );
        assert!(claims(V_CLAIM, "v") && !claims(RV64GC_ZBA_ZBB, "v"));
    }
}
