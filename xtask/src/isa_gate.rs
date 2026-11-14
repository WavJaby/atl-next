// SPDX-License-Identifier: MIT
//! Static gate over a cross host's ELF outputs: `xtask native --host <arch>` runs it after the build, `xtask isa-gate --arch <arch>` alone.
//! ISA: every riscv64 ELF64 file under the arch's out dir claims only the ISA its arches.toml row allows (`Tag_RISCV_arch`, implied extensions included: arches::isa_closure), and every GLIBC_x.y version its undefined dynamic symbols need is at or below the row's `glibc_floor`.
//! Machine: every ELF in a product dir (device/, host/, the provided set's crates/, upstream/, cinstall/, and obj/device, obj/host) is RISC-V; every ELF in a build dir (build/, build-provided/, obj/build) is the build machine's, which is this binary's own e_machine (the machine that runs the build). Every ELF is 64-bit. Other dirs (cargo/, bp2ninja/) mix machines and are counted, not read.
//! NEEDED: every DT_NEEDED of a current product output (graph.json device/ and host/) resolves to a product, a provided library (provided.json, srcs.json) or a sysroot library (cross::sysroot_lib_dirs); a name only a build-side library has fails.
//! It reads what the object declares, not what its code executes: `Tag_RISCV_arch` is the assembler's -march, so a TU built with a wider -march fails even if it never runs the extension.
//! Run-time tiers (isa-tiers.toml, isa_tiers.rs): an object named by a row may claim the row's extension and must claim it; a product of the row's module may claim it; nothing else may.
//! Not checked: static archives (object is built without its `archive` feature, so their members are not read), GLIBCXX_/CXXABI_/GCC_ needs, GTK (no riscv64 output links it yet). GLIBC_PRIVATE is the loader's own interface, not a number: skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use object::read::SymbolIndex;
use object::read::elf::{ElfFile64, FileHeader, SectionHeader, Sym};
use object::{Endianness, elf};
use serde::Deserialize;

use crate::provided_gate::{self, Linkage};
use crate::{arches, cross, deps, isa_tiers, slot};

/// psABI build attribute `Tag_RISCV_arch`: an odd tag, so its value is a NUL-terminated string.
const TAG_RISCV_ARCH: u64 = 5;
/// `e_ident` indexes of the class and data-encoding bytes (ELF gABI).
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
/// Offset of `e_machine` in the ELF header, the same in both classes (after e_ident and e_type).
const E_MACHINE_AT: usize = 18;
/// Bytes of an ELF64 header: what `build_machine` reads of xtask's own binary.
const ELF64_HEADER_LEN: usize = 64;
/// The cross host's e_machine: this gate is the riscv64 host's (its isa is rv64).
const PRODUCT_MACHINE: u16 = elf::EM_RISCV.0;

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

/// The machine a directory's ELFs must have: the cross host's (product) or the build machine's (build).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Product,
    Build,
}

/// The check a violation belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Isa,
    Glibc,
    Machine,
    Needed,
}

/// What the gate read and found under one out dir.
#[derive(Default)]
pub struct Report {
    /// riscv64 ELF64 files read (ISA and GLIBC needs checked).
    pub files: usize,
    /// Each distinct `Tag_RISCV_arch` string seen.
    pub isas: BTreeSet<String>,
    /// Highest GLIBC version any output needs, with the file and symbol that need it.
    pub max_glibc: Option<(Vec<u32>, String)>,
    /// ELF files in a product dir, in a build dir, and in neither (not machine-checked).
    pub product_elfs: usize,
    pub build_elfs: usize,
    pub unclassified: usize,
    /// The build machine's e_machine (this binary's own).
    pub build_machine: u16,
    /// Current product outputs read for DT_NEEDED, and their DT_NEEDED entries.
    pub needed_products: usize,
    pub needed_entries: usize,
    /// One line per violation, prefixed with its file, and the check that found it.
    pub violations: Vec<(Check, String)>,
    /// Files a tier row let claim its extension: `<module> <src> (<ext>)` for an object, `<module> (<ext>)` for a product.
    pub tiered: Vec<String>,
}

impl Report {
    fn fail(&mut self, check: Check, line: String) {
        self.violations.push((check, line));
    }

    /// The violations of one check.
    pub fn count(&self, check: Check) -> usize {
        self.violations.iter().filter(|(c, _)| *c == check).count()
    }
}

/// The names each class of library answers to. A DT_NEEDED name resolves when a product, provided or sysroot library has it; `build` only says where a name would have come from.
#[derive(Default)]
struct Index {
    products: BTreeSet<String>,
    provided: BTreeSet<String>,
    sysroot: BTreeSet<String>,
    /// Build-side library name -> the file that has it.
    build: BTreeMap<String, String>,
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
        &sysroot_dirs(root, host)?,
    )?;
    for line in summary(&report) {
        println!("isa gate: {line}");
    }
    for (_, v) in report.violations.iter().take(5) {
        println!("isa gate:   {v}");
    }
    Ok(if report.violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The sysroot library dirs of a cross host that exist (cross::sysroot_lib_dirs); none for a row without a sysroot.
pub fn sysroot_dirs(root: &Path, host: &arches::Host) -> Result<Vec<PathBuf>> {
    let (Some(name), Some(clang)) = (&host.sysroot, &host.clang) else {
        return Ok(Vec::new());
    };
    let sysroot = deps::sysroot_root(root, name)?;
    Ok(cross::sysroot_lib_dirs(&sysroot, clang)
        .into_iter()
        .filter(|dir| dir.is_dir())
        .collect())
}

/// The gate over every file under `out` (symlinks skipped) against `host`: its isa, glibc_floor and clang triple, and `sysroot` (sysroot_dirs). `tiers` (isa-tiers.toml) lets some files claim more; `current` (isa_tiers::current_objects) is this graph's tier objects, which must claim their row's extension. Stale objects of earlier graphs in `out` are only held to the closure.
pub fn check(
    out: &Path,
    host: &arches::Host,
    tiers: &isa_tiers::Tiers,
    current: &BTreeSet<String>,
    sysroot: &[PathBuf],
) -> Result<Report> {
    let (Some(isa), Some(floor)) = (&host.isa, &host.glibc_floor) else {
        bail!(
            "{}: the isa gate needs the row's isa and glibc_floor",
            arches::FILE
        );
    };
    let allowed = arches::isa_closure(isa)?;
    let floor = arches::dotted_version(floor)?;
    let build = build_machine()?;
    let products = provided_gate::current_outputs(out, &["device", "host"])?;
    let mut paths = Vec::new();
    collect(out, &mut paths)?;
    paths.sort();
    let mut report = Report {
        build_machine: build,
        ..Report::default()
    };
    let mut index = Index::default();
    let mut owners: Vec<(String, Linkage)> = Vec::new();
    for path in &paths {
        let rel = path.strip_prefix(out).unwrap_or(path);
        let name = rel.display().to_string();
        let data = std::fs::read(path).with_context(|| format!("reading {name}"))?;
        let Some((is64, machine)) = elf_header(&data) else {
            continue;
        };
        let side = side_of(rel);
        match side {
            Some(Side::Product) => report.product_elfs += 1,
            Some(Side::Build) => report.build_elfs += 1,
            None => report.unclassified += 1,
        }
        if let Some(side) = side
            && let Some(line) = machine_violation(&name, side, (is64, machine), build)
        {
            report.fail(Check::Machine, line);
        }
        if let Some(f) = riscv64_elf(&data).with_context(|| name.clone())? {
            report.files += 1;
            let claim = isa_claim(&f, &data).with_context(|| name.clone())?;
            let (violations, tier) =
                file_check(&name, claim.as_deref(), tiers, isa, &allowed, current)?;
            for line in violations {
                report.fail(Check::Isa, line);
            }
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
            for line in glibc_violations(&needs, &floor)? {
                report.fail(Check::Glibc, format!("{name}: {line}"));
            }
            if let Some(claim) = claim {
                report.isas.insert(claim);
            }
        }
        if is64 && products.contains(path) {
            if let Some(linkage) = provided_gate::linkage(&data).with_context(|| name.clone())? {
                index.products.extend(library_names(rel, &linkage));
                owners.push((name.clone(), linkage));
            }
        } else if is64
            && side == Some(Side::Build)
            && name.contains(".so")
            && let Some(linkage) = provided_gate::linkage(&data).with_context(|| name.clone())?
        {
            for n in library_names(rel, &linkage) {
                index.build.entry(n).or_insert_with(|| name.clone());
            }
        }
    }
    if report.files == 0 {
        report.fail(
            Check::Isa,
            format!("no riscv64 ELF64 under {}: nothing checked", out.display()),
        );
    }
    for lib in provided_libs(out)? {
        let data = std::fs::read(&lib).with_context(|| format!("reading {}", lib.display()))?;
        let linkage = provided_gate::linkage(&data)?
            .with_context(|| format!("provided {}: not a 64-bit ELF", lib.display()))?;
        index.provided.extend(library_names(&lib, &linkage));
    }
    for dir in sysroot {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            let is_shared = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().contains(".so"));
            if !is_shared || !path.is_file() {
                continue;
            }
            let data =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            if let Some(linkage) = provided_gate::linkage(&data)? {
                index.sysroot.extend(library_names(&path, &linkage));
            }
        }
    }
    for (owner, linkage) in &owners {
        report.needed_products += 1;
        report.needed_entries += linkage.needed.len();
        for line in needed_violations(owner, &linkage.needed, &index) {
            report.fail(Check::Needed, line);
        }
    }
    Ok(report)
}

/// The gate's first violation's line, None when it passes.
pub fn first_violation(report: &Report) -> Option<&str> {
    report.violations.first().map(|(_, line)| line.as_str())
}

/// The gate's summary lines, as `native` and `isa-gate` print them.
pub fn summary(report: &Report) -> Vec<String> {
    let isas: Vec<&str> = report.isas.iter().map(String::as_str).collect();
    let max = report.max_glibc.as_ref().map_or_else(
        || "none".to_owned(),
        |(version, at)| format!("GLIBC_{} ({at})", dotted(version)),
    );
    vec![
        format!(
            "{} riscv64 ELF64 files, ISA [{}], max {max}, {} ISA/glibc violations, tiers [{}]",
            report.files,
            isas.join(" | "),
            report.count(Check::Isa) + report.count(Check::Glibc),
            report.tiered.join(" | ")
        ),
        format!(
            "e_machine: {} product ELFs {}, {} build ELFs {}, {} in neither dir (not read), {} violations",
            report.product_elfs,
            machine_label(PRODUCT_MACHINE),
            report.build_elfs,
            machine_label(report.build_machine),
            report.unclassified,
            report.count(Check::Machine)
        ),
        format!(
            "DT_NEEDED: {} current products, {} entries, {} violations",
            report.needed_products,
            report.needed_entries,
            report.count(Check::Needed)
        ),
    ]
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

/// The class and e_machine of an ELF file's header, (64-bit, machine); None for a file that is not ELF. Reads the header bytes only.
fn elf_header(data: &[u8]) -> Option<(bool, u16)> {
    if !data.starts_with(&elf::ELFMAG) {
        return None;
    }
    let class = *data.get(EI_CLASS)?;
    if class != elf::ELFCLASS32.0 && class != elf::ELFCLASS64.0 {
        return None;
    }
    let bytes: [u8; 2] = data.get(E_MACHINE_AT..E_MACHINE_AT + 2)?.try_into().ok()?;
    let machine = if *data.get(EI_DATA)? == elf::ELFDATA2MSB.0 {
        u16::from_be_bytes(bytes)
    } else {
        u16::from_le_bytes(bytes)
    };
    Some((class == elf::ELFCLASS64.0, machine))
}

/// The e_machine of xtask's own binary: the machine that runs this build, the build side's expected machine.
fn build_machine() -> Result<u16> {
    let exe = std::env::current_exe().context("locating xtask's own binary")?;
    let mut head = [0u8; ELF64_HEADER_LEN];
    File::open(&exe)
        .and_then(|mut file| file.read_exact(&mut head))
        .with_context(|| format!("reading {}", exe.display()))?;
    let (is64, machine) =
        elf_header(&head).with_context(|| format!("{}: not an ELF file", exe.display()))?;
    ensure!(is64, "{}: a 32-bit build machine", exe.display());
    Ok(machine)
}

/// The side a file belongs to by its directory under the out dir (`rel`). `obj/` takes the side it names. Other dirs are not classified (None).
fn side_of(rel: &Path) -> Option<Side> {
    let parts: Vec<&str> = rel.iter().filter_map(|p| p.to_str()).collect();
    match parts.as_slice() {
        [
            "device" | "host" | "crates" | "upstream" | "cinstall",
            _,
            ..,
        ] => Some(Side::Product),
        ["build" | "build-provided", _, ..] => Some(Side::Build),
        ["obj", "device" | "host", _, ..] => Some(Side::Product),
        ["obj", "build", _, ..] => Some(Side::Build),
        _ => None,
    }
}

/// The machine check of one ELF in a `side` dir: 64-bit, and the machine the side requires (RISC-V for products, the build machine's for build).
fn machine_violation(
    name: &str,
    side: Side,
    (is64, machine): (bool, u16),
    build: u16,
) -> Option<String> {
    let (want, dir) = match side {
        Side::Product => (PRODUCT_MACHINE, "product"),
        Side::Build => (build, "build"),
    };
    if !is64 {
        return Some(format!(
            "{name}: ELF32 in a {dir} dir: hosts are 64-bit (arches.toml)"
        ));
    }
    (machine != want).then(|| {
        format!(
            "{name}: {} in a {dir} dir, want {}",
            machine_label(machine),
            machine_label(want)
        )
    })
}

/// The name of an e_machine value, for messages.
fn machine_label(machine: u16) -> String {
    match machine {
        m if m == elf::EM_RISCV.0 => "RISC-V".to_owned(),
        m if m == elf::EM_X86_64.0 => "x86-64".to_owned(),
        m => format!("e_machine {m}"),
    }
}

/// The names a library answers to: its DT_SONAME and its file name (a sysroot's symlink names are the ones ld.so finds it by).
fn library_names(path: &Path, linkage: &Linkage) -> Vec<String> {
    let file = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    linkage.soname.clone().into_iter().chain(file).collect()
}

/// Messages for each DT_NEEDED of `owner` that no product, provided or sysroot library answers to. A name only a build-side library has says where it would have come from.
fn needed_violations(owner: &str, needed: &[String], index: &Index) -> Vec<String> {
    needed
        .iter()
        .filter(|n| {
            !(index.products.contains(*n) || index.provided.contains(*n) || index.sysroot.contains(*n))
        })
        .map(|n| match index.build.get(n) {
            Some(file) => format!("{owner}: DT_NEEDED {n} resolves only on the build side ({file})"),
            None => format!(
                "{owner}: DT_NEEDED {n} resolves nowhere: not a product, provided or sysroot library"
            ),
        })
        .collect()
}

/// The provided set of an out dir: the `lib` of each provided.json entry and of each srcs.json provider (native.rs writes both; the provided gate checks their union).
fn provided_libs(out: &Path) -> Result<Vec<PathBuf>> {
    #[derive(Deserialize)]
    struct Lib {
        lib: PathBuf,
    }
    #[derive(Deserialize)]
    struct Partial {
        provider: Option<Lib>,
    }
    let provided: BTreeMap<String, Lib> = read_json(&out.join("provided.json"))?;
    let partial: BTreeMap<String, Partial> = read_json(&out.join("srcs.json"))?;
    let mut libs: Vec<PathBuf> = provided.into_values().map(|l| l.lib).collect();
    libs.extend(
        partial
            .into_values()
            .filter_map(|p| p.provider)
            .map(|l| l.lib),
    );
    libs.sort();
    libs.dedup();
    Ok(libs)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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
    fn glibc_2_40_need_fails_at_the_floor_2_39_and_2_39_passes() {
        let need = |version: &str| Need {
            symbol: "memcpy".to_owned(),
            version: version.to_owned(),
            file: "libc.so.6".to_owned(),
        };
        let floor = arches::dotted_version("2.39").unwrap();
        assert_eq!(
            glibc_violations(&[need("GLIBC_2.40")], &floor).unwrap(),
            ["memcpy@GLIBC_2.40 (from libc.so.6) needs glibc 2.40 above glibc_floor 2.39"]
        );
        assert!(
            glibc_violations(&[need("GLIBC_2.39")], &floor)
                .unwrap()
                .is_empty()
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
        assert!(elf_header(b"#!/bin/sh\n").is_none());
    }

    #[test]
    fn side_names_the_machine_by_directory() {
        let side = |p: &str| side_of(Path::new(p));
        for p in [
            "device/libx/libx.so",
            "host/libx/x",
            "crates/atl-bionic/lib/libatl_bionic.so",
            "upstream/boringssl/prefix/lib/libcrypto.so",
            "cinstall/atl-bionic/x.so",
            "obj/device/libx/a.o",
            "obj/host/libx/a.o",
        ] {
            assert_eq!(side(p), Some(Side::Product), "{p}");
        }
        for p in [
            "build/aidl/aidl",
            "build-provided/crates/atl-bionic/lib/libatl_bionic.so",
            "obj/build/aidl/a.o",
        ] {
            assert_eq!(side(p), Some(Side::Build), "{p}");
        }
        // a directory's own name, or a dir that mixes machines, is not classified
        for p in [
            "device",
            "cargo/release/deps/libserde_derive.so",
            "bp2ninja/bp2ninja",
            "obj/other/x.o",
        ] {
            assert_eq!(side(p), None, "{p}");
        }
    }

    #[test]
    fn machine_check_names_each_wrong_machine() {
        let x86 = elf::EM_X86_64.0;
        let riscv = elf::EM_RISCV.0;
        assert_eq!(
            machine_violation("device/libx/libx.so", Side::Product, (true, x86), x86).as_deref(),
            Some("device/libx/libx.so: x86-64 in a product dir, want RISC-V")
        );
        assert_eq!(
            machine_violation("build/tool", Side::Build, (true, riscv), x86).as_deref(),
            Some("build/tool: RISC-V in a build dir, want x86-64")
        );
        assert!(
            machine_violation("device/libx/libx.so", Side::Product, (true, riscv), x86).is_none()
        );
        assert!(machine_violation("build/tool", Side::Build, (true, riscv), riscv).is_none());
        assert_eq!(
            machine_violation("device/y.so", Side::Product, (false, riscv), x86).as_deref(),
            Some("device/y.so: ELF32 in a product dir: hosts are 64-bit (arches.toml)")
        );
    }

    #[test]
    fn elf_header_reads_class_and_machine_in_either_byte_order() {
        let mut head = fixture::elf64(elf::EM_RISCV.0, &[]);
        assert_eq!(elf_header(&head), Some((true, elf::EM_RISCV.0)));
        head[EI_DATA] = elf::ELFDATA2MSB.0;
        head[E_MACHINE_AT..E_MACHINE_AT + 2].copy_from_slice(&[0x00, 0xf3]);
        assert_eq!(elf_header(&head), Some((true, 243)));
    }

    #[test]
    fn needed_names_resolve_by_class_or_fail_by_name() {
        let mut index = Index::default();
        index.products.insert("libfoo.so".to_owned());
        index.provided.insert("libatl_bionic.so.0.0.0".to_owned());
        index.sysroot.insert("libc.so.6".to_owned());
        index
            .build
            .insert("libbar.so".to_owned(), "build/libbar/libbar.so".to_owned());
        let needed = [
            "libfoo.so",
            "libatl_bionic.so.0.0.0",
            "libc.so.6",
            "libbar.so",
            "libnope.so",
        ]
        .map(str::to_owned);
        assert_eq!(
            needed_violations("device/libx/libx.so", &needed, &index),
            [
                "device/libx/libx.so: DT_NEEDED libbar.so resolves only on the build side (build/libbar/libbar.so)",
                "device/libx/libx.so: DT_NEEDED libnope.so resolves nowhere: not a product, provided or sysroot library",
            ]
        );
    }

    #[test]
    fn linkage_reads_the_needed_entries_and_soname_of_a_fixture() {
        let data = fixture::lib(&fixture::Lib {
            machine: elf::EM_RISCV.0,
            isa: None,
            needed: &["libc.so.6", "libbar.so"],
            soname: Some("libfoo.so"),
            need: None,
        });
        let got = provided_gate::linkage(&data)
            .unwrap()
            .expect("a 64-bit ELF");
        assert_eq!(got.needed, ["libc.so.6", "libbar.so"]);
        assert_eq!(got.soname.as_deref(), Some("libfoo.so"));
        assert!(provided_gate::linkage(b"#!/bin/sh\n").unwrap().is_none());
    }

    /// Positive and negative controls on one tree: each planted violation is named with its file and reason; the clean products and the sysroot's libc pass.
    #[test]
    fn check_names_each_violation_on_a_fixture_tree() {
        let root = std::env::temp_dir().join(format!("xtask-isa-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let out = root.join("out");
        let sysroot = root.join("sysroot").join("usr").join("lib");
        let write = |rel: &str, data: &[u8]| {
            let path = out.join(rel);
            std::fs::create_dir_all(path.parent().expect("a file path")).expect("test dir");
            std::fs::write(path, data).expect("test file");
        };
        let riscv = |needed: &[&str], soname: &str| {
            fixture::lib(&fixture::Lib {
                machine: elf::EM_RISCV.0,
                isa: Some(RV64GC_ZBA_ZBB),
                needed,
                soname: Some(soname),
                need: None,
            })
        };
        let build = build_machine().unwrap();
        write(
            "device/libfoo/libfoo.so",
            &riscv(&["libc.so.6", "libbar.so", "libnope.so"], "libfoo.so"),
        );
        write(
            "device/libok/libok.so",
            &riscv(&["libc.so.6", "libatl_bionic.so.0.0.0"], "libok.so"),
        );
        write(
            "device/libglibc/libglibc.so",
            &fixture::lib(&fixture::Lib {
                machine: elf::EM_RISCV.0,
                isa: Some(RV64GC_ZBA_ZBB),
                needed: &["libc.so.6"],
                soname: Some("libglibc.so"),
                need: Some(("memcpy", "GLIBC_2.40", "libc.so.6")),
            }),
        );
        write(
            "device/libwrong/libwrong.so",
            &fixture::lib(&fixture::Lib {
                machine: elf::EM_X86_64.0,
                isa: None,
                needed: &[],
                soname: Some("libwrong.so"),
                need: None,
            }),
        );
        write(
            "crates/atl-bionic/lib/libatl_bionic.so",
            &riscv(&[], "libatl_bionic.so.0.0.0"),
        );
        write(
            "build/libbar/libbar.so",
            &fixture::lib(&fixture::Lib {
                machine: build,
                isa: None,
                needed: &[],
                soname: Some("libbar.so"),
                need: None,
            }),
        );
        write("build/tool", &riscv(&[], "tool"));
        write(
            "graph.json",
            format!(
                r#"{{"artifacts": {{"device/libfoo": {{"out": "{d}/device/libfoo/libfoo.so"}}, "device/libok": {{"out": "{d}/device/libok/libok.so"}}, "device/libglibc": {{"out": "{d}/device/libglibc/libglibc.so"}}}}}}"#,
                d = out.display()
            )
            .as_bytes(),
        );
        write(
            "provided.json",
            format!(
                r#"{{"libc": {{"lib": "{d}/crates/atl-bionic/lib/libatl_bionic.so"}}}}"#,
                d = out.display()
            )
            .as_bytes(),
        );
        write(
            "srcs.json",
            br#"{"libsrc": {"srcs": [], "provider": null}}"#,
        );
        std::fs::create_dir_all(&sysroot).expect("test dir");
        std::fs::write(sysroot.join("libc.so.6"), riscv(&[], "libc.so.6")).expect("test file");
        let host = arches::Host {
            rust: "riscv64gc-unknown-linux-gnu".to_owned(),
            meson_cpu_family: "riscv64".to_owned(),
            debian: "riscv64".to_owned(),
            archlinux: None,
            soong_prebuilt_os: None,
            isa: Some("rv64gc_zba_zbb".to_owned()),
            clang: Some("riscv64-linux-gnu".to_owned()),
            sysroot: Some("riscv64-sysroot".to_owned()),
            glibc_floor: Some("2.39".to_owned()),
            cc: Some("clang-21".to_owned()),
            cxx: Some("clang-21-cxx".to_owned()),
            lld: Some("lld-21".to_owned()),
            cflags: vec!["-mno-relax".to_owned()],
        };
        let report = check(
            &out,
            &host,
            &BTreeMap::new(),
            &BTreeSet::new(),
            std::slice::from_ref(&sysroot),
        )
        .unwrap();
        let lines = |check: Check| -> Vec<&str> {
            report
                .violations
                .iter()
                .filter(|(c, _)| *c == check)
                .map(|(_, l)| l.as_str())
                .collect()
        };
        assert!(lines(Check::Isa).is_empty(), "{:?}", lines(Check::Isa));
        assert_eq!(
            lines(Check::Glibc),
            [
                "device/libglibc/libglibc.so: memcpy@GLIBC_2.40 (from libc.so.6) needs glibc 2.40 above glibc_floor 2.39"
            ]
        );
        assert_eq!(
            lines(Check::Machine),
            // in path order, as collect() walks them
            [
                format!(
                    "build/tool: RISC-V in a build dir, want {}",
                    machine_label(build)
                ),
                "device/libwrong/libwrong.so: x86-64 in a product dir, want RISC-V".to_owned(),
            ]
        );
        assert_eq!(
            lines(Check::Needed),
            [
                "device/libfoo/libfoo.so: DT_NEEDED libbar.so resolves only on the build side (build/libbar/libbar.so)",
                "device/libfoo/libfoo.so: DT_NEEDED libnope.so resolves nowhere: not a product, provided or sysroot library",
            ]
        );
        // counts: the graph's three products; riscv64 ELF64s read (product, provided and build dirs, the tool); ELFs in each side
        assert_eq!(report.needed_products, 3);
        assert_eq!(report.needed_entries, 6);
        assert_eq!(report.files, 5);
        assert_eq!(report.product_elfs, 5);
        assert_eq!(report.build_elfs, 2);
        assert_eq!(report.unclassified, 0);
        std::fs::remove_dir_all(&root).expect("test dir cleanup");
    }

    /// Test fixtures: small ELF64 little-endian files holding the sections the gates read (header, section table, `.dynstr`, `.dynamic`, `.riscv.attributes`, and `.dynsym` with its version needs).
    mod fixture {
        use super::*;
        use object::elf;

        const ET_DYN: u16 = 3;
        const EV_CURRENT: u8 = 1;
        const ELFDATA2LSB: u8 = 1;
        const DT_NULL: i64 = 0;
        const SYM_UNDEFINED_FUNC: u8 = 0x12;

        /// One section of a fixture file. `link` names another section of the same file ("" for none).
        pub struct Sec<'a> {
            pub name: &'a str,
            pub ty: u32,
            pub link: &'a str,
            pub info: u32,
            pub entsize: u64,
            pub data: Vec<u8>,
        }

        /// A shared object's parts. `isa`: its Tag_RISCV_arch (none: no attributes). `need`: one undefined symbol (name, version, file) with a version need.
        pub struct Lib<'a> {
            pub machine: u16,
            pub isa: Option<&'a str>,
            pub needed: &'a [&'a str],
            pub soname: Option<&'a str>,
            pub need: Option<(&'a str, &'a str, &'a str)>,
        }

        /// The ELF64 little-endian file of `secs` for `machine`, with the section name table last.
        pub fn elf64(machine: u16, secs: &[Sec]) -> Vec<u8> {
            let mut names = vec![0u8];
            let mut name_at = Vec::new();
            for name in secs
                .iter()
                .map(|s| s.name)
                .chain(std::iter::once(".shstrtab"))
            {
                name_at.push(names.len() as u32);
                names.extend(name.as_bytes());
                names.push(0);
            }
            let mut file = vec![0u8; ELF64_HEADER_LEN];
            let mut offsets = Vec::new();
            for s in secs {
                align8(&mut file);
                offsets.push(file.len() as u64);
                file.extend(&s.data);
            }
            align8(&mut file);
            let names_at = file.len() as u64;
            file.extend(&names);
            align8(&mut file);
            let table_at = file.len() as u64;
            // section 0 is the null section; `secs` follow; the name table is last
            let index = |link: &str| {
                secs.iter()
                    .position(|s| s.name == link)
                    .map_or(0, |i| i as u32 + 1)
            };
            let mut table = vec![0u8; ELF64_HEADER_LEN];
            for (i, s) in secs.iter().enumerate() {
                let (ty, link, info, size) = (s.ty, index(s.link), s.info, s.data.len() as u64);
                shdr(
                    &mut table, name_at[i], ty, offsets[i], size, link, info, 8, s.entsize,
                );
            }
            let strtab = elf::SHT_STRTAB.0;
            shdr(
                &mut table,
                name_at[secs.len()],
                strtab,
                names_at,
                names.len() as u64,
                0,
                0,
                1,
                0,
            );
            file.extend(&table);
            file[..4].copy_from_slice(&elf::ELFMAG);
            file[EI_CLASS] = elf::ELFCLASS64.0;
            file[EI_DATA] = ELFDATA2LSB;
            file[6] = EV_CURRENT;
            file[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
            file[E_MACHINE_AT..E_MACHINE_AT + 2].copy_from_slice(&machine.to_le_bytes());
            file[20..24].copy_from_slice(&1u32.to_le_bytes());
            file[40..48].copy_from_slice(&table_at.to_le_bytes());
            file[52..54].copy_from_slice(&64u16.to_le_bytes());
            file[54..56].copy_from_slice(&56u16.to_le_bytes());
            file[58..60].copy_from_slice(&64u16.to_le_bytes());
            file[60..62].copy_from_slice(&((secs.len() + 2) as u16).to_le_bytes());
            file[62..64].copy_from_slice(&((secs.len() + 1) as u16).to_le_bytes());
            file
        }

        /// The `lib`'s file: `.riscv.attributes` (when `isa`), `.dynstr`, `.dynamic` (DT_NEEDED, DT_SONAME), and for `need` `.dynsym`, `.gnu.version`, `.gnu.version_r`.
        pub fn lib(l: &Lib) -> Vec<u8> {
            let mut strtab = vec![0u8];
            let needed: Vec<u32> = l.needed.iter().map(|n| add(&mut strtab, n)).collect();
            let soname = l.soname.map(|s| add(&mut strtab, s));
            let mut dynamic: Vec<(i64, u64)> = Vec::new();
            for at in needed {
                dynamic.push((elf::DT_NEEDED.0, u64::from(at)));
            }
            if let Some(at) = soname {
                dynamic.push((elf::DT_SONAME.0, u64::from(at)));
            }
            dynamic.push((DT_NULL, 0));
            let mut secs = Vec::new();
            if let Some(isa) = l.isa {
                secs.push(Sec {
                    name: ".riscv.attributes",
                    ty: elf::SHT_RISCV_ATTRIBUTES.0,
                    link: "",
                    info: 0,
                    entsize: 0,
                    data: riscv_arch(isa),
                });
            }
            secs.push(Sec {
                name: ".dynstr",
                ty: elf::SHT_STRTAB.0,
                link: "",
                info: 0,
                entsize: 0,
                data: Vec::new(),
            });
            let dyn_bytes: Vec<u8> = dynamic
                .iter()
                .flat_map(|(tag, val)| tag.to_le_bytes().into_iter().chain(val.to_le_bytes()))
                .collect();
            secs.push(Sec {
                name: ".dynamic",
                ty: elf::SHT_DYNAMIC.0,
                link: ".dynstr",
                info: 0,
                entsize: 16,
                data: dyn_bytes,
            });
            if let Some((symbol, version, file)) = l.need {
                let sym_at = add(&mut strtab, symbol);
                let file_at = add(&mut strtab, file);
                let ver_at = add(&mut strtab, version);
                // version index 2: the first index past VER_NDX_LOCAL and VER_NDX_GLOBAL
                let mut dynsym = vec![0u8; 24];
                dynsym.extend(sym(sym_at));
                let mut versym = vec![0u8, 0];
                versym.extend(2u16.to_le_bytes());
                let mut verneed = Vec::new();
                verneed.extend(1u16.to_le_bytes());
                verneed.extend(1u16.to_le_bytes());
                verneed.extend(file_at.to_le_bytes());
                verneed.extend(16u32.to_le_bytes());
                verneed.extend(0u32.to_le_bytes());
                verneed.extend(0u32.to_le_bytes());
                verneed.extend(0u16.to_le_bytes());
                verneed.extend(2u16.to_le_bytes());
                verneed.extend(ver_at.to_le_bytes());
                verneed.extend(0u32.to_le_bytes());
                secs.push(Sec {
                    name: ".dynsym",
                    ty: elf::SHT_DYNSYM.0,
                    link: ".dynstr",
                    info: 1,
                    entsize: 24,
                    data: dynsym,
                });
                secs.push(Sec {
                    name: ".gnu.version",
                    ty: elf::SHT_GNU_VERSYM.0,
                    link: ".dynsym",
                    info: 0,
                    entsize: 2,
                    data: versym,
                });
                secs.push(Sec {
                    name: ".gnu.version_r",
                    ty: elf::SHT_GNU_VERNEED.0,
                    link: ".dynstr",
                    info: 1,
                    entsize: 0,
                    data: verneed,
                });
            }
            // .dynstr is placed with its final bytes
            if let Some(s) = secs.iter_mut().find(|s| s.name == ".dynstr") {
                s.data = strtab;
            }
            elf64(l.machine, &secs)
        }

        /// A string added to a string table: its offset.
        fn add(tab: &mut Vec<u8>, s: &str) -> u32 {
            let at = tab.len() as u32;
            tab.extend(s.as_bytes());
            tab.push(0);
            at
        }

        /// An undefined global function symbol named at `name` in .dynstr.
        fn sym(name: u32) -> [u8; 24] {
            let mut out = [0u8; 24];
            out[0..4].copy_from_slice(&name.to_le_bytes());
            out[4] = SYM_UNDEFINED_FUNC;
            out
        }

        /// `.riscv.attributes` with one Tag_File attribute, Tag_RISCV_arch = `isa`: 'A', the vendor subsection, the file sub-subsection, the ULEB128 tag and the NUL-terminated string.
        fn riscv_arch(isa: &str) -> Vec<u8> {
            let mut attr = vec![TAG_RISCV_ARCH as u8];
            attr.extend(isa.as_bytes());
            attr.push(0);
            let vendor = b"riscv\0";
            let file_tag = 1u8;
            // the sub-subsection's size counts its tag and size bytes too
            let mut sub_sub = vec![file_tag];
            sub_sub.extend(((attr.len() + 5) as u32).to_le_bytes());
            sub_sub.extend(attr);
            let mut sub = Vec::new();
            sub.extend(((4 + vendor.len() + sub_sub.len()) as u32).to_le_bytes());
            sub.extend(vendor);
            sub.extend(sub_sub);
            let mut out = vec![b'A'];
            out.extend(sub);
            out
        }

        fn align8(file: &mut Vec<u8>) {
            while !file.len().is_multiple_of(8) {
                file.push(0);
            }
        }

        /// One ELF64 section header (64 bytes) appended to `table`.
        #[allow(clippy::too_many_arguments)]
        fn shdr(
            table: &mut Vec<u8>,
            name: u32,
            ty: u32,
            offset: u64,
            size: u64,
            link: u32,
            info: u32,
            align: u64,
            entsize: u64,
        ) {
            table.extend(name.to_le_bytes());
            table.extend(ty.to_le_bytes());
            table.extend([0u8; 16]); // sh_flags, sh_addr
            table.extend(offset.to_le_bytes());
            table.extend(size.to_le_bytes());
            table.extend(link.to_le_bytes());
            table.extend(info.to_le_bytes());
            table.extend(align.to_le_bytes());
            table.extend(entsize.to_le_bytes());
        }

        #[test]
        fn fixture_lib_is_read_back_by_object() {
            let data = lib(&Lib {
                machine: elf::EM_RISCV.0,
                isa: Some(RV64GC_ZBA_ZBB),
                needed: &["libc.so.6"],
                soname: Some("libx.so"),
                need: Some(("memcpy", "GLIBC_2.40", "libc.so.6")),
            });
            let f = ElfFile64::<Endianness>::parse(&*data).unwrap();
            assert!(riscv64_elf(&data).unwrap().is_some());
            assert_eq!(
                isa_claim(&f, &data).unwrap().as_deref(),
                Some(RV64GC_ZBA_ZBB)
            );
            let needs = glibc_needs(&f, &data).unwrap();
            assert_eq!(
                needs,
                [Need {
                    symbol: "memcpy".to_owned(),
                    version: "GLIBC_2.40".to_owned(),
                    file: "libc.so.6".to_owned(),
                }]
            );
        }
    }
}
