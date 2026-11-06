// SPDX-License-Identifier: MIT
//! Bit-fields and C++ references against clang's own record layout: the expected values come from
//! `clang++ -Xclang -fdump-record-layouts-complete` run on the same header, never from bridgegen.
//! One LP64 and one ILP32 target, so the pointer width really varies.
//! Deliberate break: `RUSTFLAGS=--cfg=atl_bridgegen_break` sizes a reference by its referent -> fails.
#![expect(
    clippy::disallowed_types,
    reason = "a test: it runs clang++ on the host"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::ir::{Interface, Ty};
use atl_bridgegen::{Lang, Source, Target, interface};

/// Mixed widths; `b` does not fit after `a` in one `unsigned` unit and moves to the next; `d` is a
/// 40-bit `unsigned long long` starting mid-byte; `short : 0` closes the unit before `e`.
/// `R` stores a reference; `take` passes two.
const HEADER: &str = "struct S { unsigned a : 3; int b : 31; char c; unsigned long long d : 40; short : 0; short e : 5; unsigned char f : 1; };\nstruct R { int &r; char c; };\nvoid take(const S &s, int &&x);\n";

const TARGETS: &[(&str, &str)] = &[
    ("x86_64", "x86_64-linux-android"),
    ("x86", "i686-linux-android"),
];

/// One record of clang's dump: size, align, and per named field (bit offset, bit-field width).
#[derive(Debug, Default)]
struct Dumped {
    size: u64,
    align: u64,
    fields: Vec<(String, u64, Option<u64>)>,
}

/// `N` (byte) or `N:lo-hi` (bit-field: byte, then bits within it) -> (bit offset, width).
fn position(pos: &str) -> Result<(u64, Option<u64>)> {
    Ok(match pos.split_once(':') {
        Some((byte, range)) => {
            let (lo, hi) = range.split_once('-').context("bit range")?;
            let (lo, hi): (u64, u64) = (lo.parse()?, hi.parse()?);
            (byte.parse::<u64>()? * 8 + lo, Some(hi - lo + 1))
        }
        None => (pos.parse::<u64>()? * 8, None),
    })
}

fn dump(header: &Path, triple: &str) -> Result<BTreeMap<String, Dumped>> {
    let out = Command::new("clang++")
        .args([
            &format!("--target={triple}"),
            "-nostdlibinc",
            "-fsyntax-only",
            "-Xclang",
            "-fdump-record-layouts-complete",
        ])
        .arg(header)
        .output()
        .context("clang++ on PATH")?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout)?;
    let mut records = BTreeMap::new();
    for block in text.split("*** Dumping AST Record Layout").skip(1) {
        let mut lines = block.lines().filter(|l| l.contains('|'));
        let head = lines.next().context("record line")?;
        let name = head.split_whitespace().last().context("record name")?;
        let mut rec = Dumped::default();
        for l in lines {
            let (pos, rest) = l.split_once('|').context("column")?;
            let (pos, rest) = (pos.trim(), rest.trim());
            if let Some(attrs) = rest.strip_prefix('[') {
                for kv in attrs.split(',') {
                    match kv.trim().split_once('=') {
                        Some(("sizeof", v)) => rec.size = v.parse()?,
                        Some(("align", v)) => rec.align = v.parse()?,
                        _ => {}
                    }
                }
                continue;
            }
            // continuation of the size line, or an unnamed bit-field
            if pos.is_empty() || pos.ends_with(":-") {
                continue;
            }
            let field = rest.split_whitespace().last().context("field name")?;
            let (bit, width) = position(pos).with_context(|| l.to_owned())?;
            rec.fields.push((field.to_owned(), bit, width));
        }
        records.insert(name.to_owned(), rec);
    }
    Ok(records)
}

fn parsed(header: &Path, arch: &str, triple: &str) -> Result<Interface> {
    let src = Source {
        path: header,
        lang: Lang::Cxx,
        also: &[],
        macros: &[],
    };
    let target = Target {
        arch: arch.into(),
        triple: triple.into(),
        args: vec!["-nostdlibinc".into()],
    };
    interface("t", "t.h", &src, &[target])
}

fn check(header: &Path, arch: &str, triple: &str) -> Result<()> {
    let expected = dump(header, triple)?;
    let iface = parsed(header, arch, triple)?;
    let view = &iface.arches[arch];
    for name in ["S", "R"] {
        let (want, got) = (&expected[name], &view.records[name]);
        ensure!(
            (got.size, got.align) == (want.size, want.align),
            "{arch} {name}: size/align {:?}, clang {:?}",
            (got.size, got.align),
            (want.size, want.align)
        );
        let mut got_fields = Vec::new();
        for f in &got.fields {
            let shift = f.bits.map_or(0, |b| u64::from(b.shift));
            if let (Some(_), Ty::Int { bits, .. }) = (f.bits, f.ty.resolved()) {
                // the storage unit sits at a multiple of the declared type's size
                ensure!(
                    f.offset % u64::from(bits / 8) == 0,
                    "{arch} {name}.{}: unit",
                    f.name
                );
            }
            got_fields.push((
                f.name.clone(),
                f.offset * 8 + shift,
                f.bits.map(|b| u64::from(b.width)),
            ));
        }
        ensure!(
            got_fields == want.fields,
            "{arch} {name}: {got_fields:?}\nclang: {:?}",
            want.fields
        );
    }
    // `char c` follows the reference directly (alignment 1): its offset is the reference's size
    let reference_bits = expected["R"].fields[1].1;
    for p in &view.functions["take"].params {
        match p {
            Ty::Reference { bits, .. } => ensure!(
                u64::from(*bits) == reference_bits,
                "{arch} take: reference of {bits} bits, clang {reference_bits}"
            ),
            other => anyhow::bail!("{arch} take: {other:?}"),
        }
    }
    Ok(())
}

#[test]
fn bit_fields_and_references_match_clang_record_layout() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("atl-bridgegen-fixtures-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let header = dir.join("t.h");
    std::fs::write(&header, HEADER)?;
    let result = TARGETS
        .iter()
        .try_for_each(|&(arch, triple)| check(&header, arch, triple));
    std::fs::remove_dir_all(&dir)?;
    result
}
