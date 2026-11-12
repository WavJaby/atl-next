// SPDX-License-Identifier: MIT
//! `isa-tiers.toml`: run-time ISA tiers of a cross host (route B). A source of a module is compiled with the host row's isa plus one extension, because upstream runs it only behind a run-time CPU check.
//! Readers: the `tables` check step (`check`), `native` (the converter's input `isa-tiers.json` by `write_converter_file`; the probe `probe` before ninja, gate a), `isa_gate` (`object_row`, `product_row`, `closure`: gate b).
//! The converter (tools/bp2ninja isatier.go) checks each row's module, src and gate against the tree at the pin, and applies the march to the source.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::arches;

/// The table, at the repo root beside providers.toml.
pub const FILE: &str = "isa-tiers.toml";
/// The converter's input, in the cross host's out dir: module -> {src, march, gate}.
const CONVERTER_FILE: &str = "isa-tiers.json";
/// The converter's output: the tier sources' compile argv, probed before ninja.
const PROBE_FILE: &str = "isa-tier-probes.json";
/// The one extension with implication rules here (`closure`).
const EXT: &str = "v";
/// What V implies, as clang's Tag_RISCV_arch lists it for -march=rv64gcv (RVV 1.0 implication rules: Zvl128b, Zve64d -> Zve64f -> Zve32f, Zve64x -> Zve32x, Zvl64b, Zvl32b).
const V_IMPLIED: [&str; 8] = [
    "zvl128b", "zve64d", "zve64f", "zve64x", "zve32f", "zve32x", "zvl64b", "zvl32b",
];

/// One row, keyed by module in the table (providers.toml's format).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub src: String,
    pub ext: String,
    pub gate: String,
    pub reason: String,
}

pub type Tiers = BTreeMap<String, Tier>;

#[derive(Serialize)]
struct ConverterRow<'a> {
    src: &'a str,
    march: String,
    gate: &'a str,
}

/// One tier source's compile argv as the converter emits it (tools/bp2ninja isatier.go isaTierProbe).
#[derive(Debug, Deserialize)]
pub struct Probe {
    pub module: String,
    pub side: String,
    pub src: String,
    pub base: Vec<String>,
    pub tier: Vec<String>,
}

/// Reads the table and checks each row's own fields. The tree-dependent part (module, src, gate file) is the converter's, at the pin.
pub fn load(root: &Path) -> Result<Tiers> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let tiers: Tiers = toml::from_str(&text).with_context(|| format!("in {FILE}"))?;
    for (module, row) in &tiers {
        check_row(module, row).with_context(|| format!("in {FILE} [{module}]"))?;
    }
    Ok(tiers)
}

fn check_row(module: &str, row: &Tier) -> Result<()> {
    ensure!(
        !module.is_empty() && !module.contains([':', '/']),
        "key `{module}`: not a module name"
    );
    ensure!(
        relative_path(&row.src),
        "src `{}`: a path relative to the module directory (no `..`, globs or leading `/`)",
        row.src
    );
    ensure!(
        row.ext == EXT,
        "ext `{}`: only `{EXT}` has implication rules here (isa_tiers.rs closure)",
        row.ext
    );
    let (file, symbol) = row
        .gate
        .split_once(':')
        .with_context(|| format!("gate `{}`: not <file>:<symbol>", row.gate))?;
    ensure!(
        relative_path(file) && is_ident(symbol),
        "gate `{}`: <file> a path relative to the module directory, <symbol> an identifier",
        row.gate
    );
    ensure!(!row.reason.trim().is_empty(), "reason: empty");
    Ok(())
}

/// A relative path with no `..`, empty component, glob or backslash.
fn relative_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['*', '?', '[', '\\'])
        && !s.split('/').any(|c| c.is_empty() || c == "..")
}

fn is_ident(s: &str) -> bool {
    s.chars()
        .enumerate()
        .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        && !s.is_empty()
}

/// The `tables` step: every row's extension takes effect on every cross host's isa (the march derives and the closure names the extension).
pub fn check(root: &Path, arches: &arches::Arches) -> Result<()> {
    let tiers = load(root)?;
    for (name, arch) in arches {
        let Some(isa) = arch.host.as_ref().and_then(|h| h.isa.as_deref()) else {
            continue;
        };
        for (module, row) in &tiers {
            march(isa, &row.ext)
                .and_then(|_| closure(isa, &row.ext))
                .with_context(|| format!("in {FILE} [{module}] for host {name}"))?;
        }
    }
    Ok(())
}

/// The -march of a tier source: the host row's isa with the extension. `v` goes after the base (`rv64gc` -> `rv64gcv`); the row's Z extensions follow unchanged.
pub fn march(isa: &str, ext: &str) -> Result<String> {
    ensure!(
        ext == EXT,
        "ext `{ext}`: only `{EXT}` has a march rule here"
    );
    let rest = isa
        .strip_prefix("rv64gc")
        .with_context(|| format!("isa `{isa}`: the base must be rv64gc"))?;
    ensure!(
        rest.is_empty() || rest.starts_with('_'),
        "isa `{isa}`: already has single-letter extensions past rv64gc"
    );
    Ok(format!("rv64gc{ext}{rest}"))
}

/// The extensions a tier source's objects may claim: the row's closure, the tier extension and what V implies.
pub fn closure(isa: &str, ext: &str) -> Result<BTreeSet<String>> {
    march(isa, ext)?;
    let mut set = arches::isa_closure(isa)?;
    set.insert(ext.to_owned());
    set.extend(V_IMPLIED.iter().map(|e| (*e).to_owned()));
    Ok(set)
}

/// The row whose object `rel` (relative to the out dir) is: `obj/<side>/<module>/<kind>/<tail>.o` with `<tail>` ending at the row's src.
pub fn object_row<'a>(tiers: &'a Tiers, rel: &str) -> Option<(&'a str, &'a Tier)> {
    let mut parts = rel.strip_prefix("obj/")?.splitn(4, '/');
    let (_side, module, _kind, tail) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let (module, row) = tiers.get_key_value(module)?;
    let src = tail.strip_suffix(".o")?;
    (src == row.src || src.ends_with(&format!("/{}", row.src))).then_some((module.as_str(), row))
}

/// The row whose product `rel` is: `device/<module>.so` or `host/<module>.so` (the converter's product sides; `build/` holds the build machine's and is not a product).
pub fn product_row<'a>(tiers: &'a Tiers, rel: &str) -> Option<(&'a str, &'a Tier)> {
    let (side, file) = rel.split_once('/')?;
    if !(side == "device" || side == "host") || file.contains('/') {
        return None;
    }
    let (module, row) = tiers.get_key_value(file.strip_suffix(".so")?)?;
    Some((module.as_str(), row))
}

/// Writes `<out>/isa-tiers.json` for the converter (`-isa-tiers`): module -> {src, march, gate}, the march being the cross host's isa with the extension.
pub fn write_converter_file(tiers: &Tiers, isa: &str, out: &Path) -> Result<PathBuf> {
    let mut rows = BTreeMap::new();
    for (module, row) in tiers {
        rows.insert(
            module.as_str(),
            ConverterRow {
                src: &row.src,
                march: march(isa, &row.ext)?,
                gate: &row.gate,
            },
        );
    }
    let path = out.join(CONVERTER_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&rows)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Gate (a), before ninja: each probed tier source compiles with its tier argv, and with its base argv (the row's isa) fails naming the extension. A row whose source does not need the extension fails here: the row would be a no-op. Returns the count probed (0: no row's module is in the goals).
pub fn probe(out: &Path, cc: &str) -> Result<usize> {
    let path = out.join(PROBE_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let probes: Vec<Probe> =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    for p in &probes {
        let at = format!("{}/{} {}", p.side, p.module, p.src);
        let tier = compile(cc, &p.tier, &p.src)?;
        ensure!(
            tier.status.success(),
            "isa tier {at}: the tier compile fails: {}",
            first_error(&tier.stderr)
        );
        let base = compile(cc, &p.base, &p.src)?;
        let named = String::from_utf8_lossy(&base.stderr)
            .lines()
            .find(|l| l.contains("extension"))
            .map(str::trim)
            .map(str::to_owned);
        let Some(named) = named.filter(|_| !base.status.success()) else {
            anyhow::bail!(
                "isa tier {at}: the row's isa does not fail for the extension: no-op row, or the source fails for another reason ({})",
                first_error(&base.stderr)
            );
        };
        println!("native: isa tier: {at}: row isa fails ({named}); tier compiles");
    }
    Ok(probes.len())
}

fn compile(cc: &str, argv: &[String], src: &str) -> Result<Output> {
    Command::new(cc)
        .args(argv)
        .arg("-fsyntax-only")
        .arg(src)
        .output()
        .with_context(|| format!("running {cc}"))
}

fn first_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .find(|l| l.contains("error"))
        .or_else(|| text.lines().last())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Tiers {
        toml::from_str(
            "[libz]\nsrc = \"adler32_simd.c\"\next = \"v\"\ngate = \"adler32.c:riscv_cpu_enable_rvv\"\nreason = \"r\"\n",
        )
        .unwrap()
    }

    #[test]
    fn march_puts_v_after_the_base() {
        assert_eq!(march("rv64gc_zba_zbb", "v").unwrap(), "rv64gcv_zba_zbb");
        assert_eq!(march("rv64gc", "v").unwrap(), "rv64gcv");
        // written from the spelling of -march=rv64gcv_zba_zbb (the study's clang probe)
        assert!(march("rv64gcv_zba_zbb", "v").is_err());
        assert!(march("rv32gc", "v").is_err());
        assert!(march("rv64gc_zba_zbb", "zbs").is_err());
    }

    #[test]
    fn closure_adds_what_v_implies() {
        let set = closure("rv64gc_zba_zbb", "v").unwrap();
        for e in ["v", "zba", "zbb", "zvl128b", "zve32x", "zve64d", "zvl32b"] {
            assert!(set.contains(e), "{e} missing from {set:?}");
        }
        assert!(!set.contains("zbs"), "the row never has Zbs");
    }

    #[test]
    fn rows_name_objects_and_products() {
        let tiers = rows();
        let (module, row) = object_row(
            &tiers,
            "obj/device/libz/shared/external/zlib/adler32_simd.c.o",
        )
        .unwrap();
        assert_eq!((module, row.src.as_str()), ("libz", "adler32_simd.c"));
        assert!(object_row(&tiers, "obj/device/libz/shared/external/zlib/adler32.c.o").is_none());
        assert!(object_row(&tiers, "obj/device/libzz/shared/x/adler32_simd.c.o").is_none());
        assert!(
            object_row(
                &tiers,
                "obj/device/libz/shared/external/zlib/xadler32_simd.c.o"
            )
            .is_none()
        );
        assert_eq!(
            product_row(&tiers, "device/libz.so").map(|(m, _)| m),
            Some("libz")
        );
        assert_eq!(
            product_row(&tiers, "host/libz.so").map(|(m, _)| m),
            Some("libz")
        );
        assert!(product_row(&tiers, "build/libz.so").is_none());
        assert!(product_row(&tiers, "device/libz.a").is_none());
    }

    #[test]
    fn fields_are_checked_as_written() {
        let mut row = rows().remove("libz").unwrap();
        assert!(check_row("libz", &row).is_ok());
        row.src = "../adler32_simd.c".to_owned();
        assert!(check_row("libz", &row).is_err());
        row.src = "adler32_simd.c".to_owned();
        row.ext = "zbs".to_owned();
        assert!(check_row("libz", &row).is_err());
        row.ext = "v".to_owned();
        row.gate = "adler32.c".to_owned();
        assert!(check_row("libz", &row).is_err());
        row.gate = "adler32.c:riscv_cpu_enable_rvv".to_owned();
        row.reason = " ".to_owned();
        assert!(check_row("libz", &row).is_err());
    }
}
