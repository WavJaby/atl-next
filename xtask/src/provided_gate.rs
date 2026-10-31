// SPDX-License-Identifier: MIT
//! Gate after `xtask native`'s build: the shared libraries providers.toml provides outside the tree (`crate:`, `upstream:` and `system:` rows) win the process's symbol lookup over the weak fallbacks AOSP code carries for non-bionic builds (libbase's and libcutils' `__system_property_*`, system/libbase/properties.cpp), and nothing in the tree implements their API a second time. Per device artifact:
//! (System libraries get both rules too: the converter links every provided library first, so the order holds by construction, and the symbol rule is what catches an in-tree module still implementing, say, `egl*` next to the host's libEGL.)
//! - the provided libraries it needs lead its DT_NEEDED: ld.so searches the executable's needed list breadth first, so provided libraries ahead of every other entry in every list precede every library carrying a fallback;
//! - every symbol it defines that a provided library exports is weak in a shared library (a strong one would be a second implementation of the provided API), and absent from an executable (an executable's own definition binds before any library's).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use object::read::elf::ElfFile64;
use object::{Endianness, Object, ObjectSymbol, elf};

/// What a provided library exports.
pub struct Exported {
    pub soname: String,
    pub symbols: BTreeSet<String>,
}

/// One gate run.
#[derive(Debug, Default)]
pub struct Report {
    pub artifacts: usize,
    /// Weak definitions of provided symbols, which lose to the provided library.
    pub fallbacks: usize,
    pub violations: Vec<String>,
}

struct Elf {
    needed: Vec<String>,
    soname: Option<String>,
    /// Defined symbols the dynamic linker can bind to (or, in an executable, binds itself to): name, weak.
    defined: Vec<(String, bool)>,
}

/// None for a file that is not a 64-bit ELF (hosts are 64-bit, arches.toml).
fn read_elf(path: &Path, executable: bool) -> Result<Option<Elf>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(file) = ElfFile64::<Endianness>::parse(&*data) else {
        return Ok(None);
    };
    let endian = file.endian();
    let table = file
        .elf_section_table()
        .dynamic_table(endian, &*data)
        .with_context(|| format!("{}: dynamic section", path.display()))?;
    let mut needed = Vec::new();
    let mut soname = None;
    for d in &table {
        if d.tag != elf::DT_NEEDED && d.tag != elf::DT_SONAME {
            continue;
        }
        let name = String::from_utf8_lossy(table.string(d)?).into_owned();
        if d.tag == elf::DT_NEEDED {
            needed.push(name);
        } else {
            soname = Some(name);
        }
    }
    // an executable binds to its own definitions whether it exports them or not
    let symbols: Vec<_> = if executable {
        file.symbols().collect()
    } else {
        file.dynamic_symbols().collect()
    };
    let defined = symbols
        .iter()
        .filter(|s| s.is_definition() && s.is_global())
        .filter_map(|s| Some((s.name().ok()?.to_owned(), s.is_weak())))
        .collect();
    Ok(Some(Elf {
        needed,
        soname,
        defined,
    }))
}

/// Section-boundary symbols the linker defines in every output it links and exports where no version script hides them (BoringSSL's CMake-linked libraries do; cargo-c's do not): each object's own, never an API, so never a competing definition.
const LINKER_DEFINED: &[&str] = &["_end", "_edata", "__bss_start"];

/// What the provided shared library `lib` exports.
pub fn exported(lib: &Path) -> Result<Exported> {
    let elf =
        read_elf(lib, false)?.with_context(|| format!("{}: not a 64-bit ELF", lib.display()))?;
    Ok(Exported {
        soname: elf
            .soname
            .with_context(|| format!("{}: no DT_SONAME", lib.display()))?,
        symbols: elf
            .defined
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| !LINKER_DEFINED.contains(&n.as_str()))
            .collect(),
    })
}

/// The converter's link outputs under `<out>/device`: `<module>/<module>.so` and `<module>/<module>`.
fn artifacts(device: &Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(device) else {
        return Ok(out);
    };
    for dir in dirs {
        let dir = dir?.path();
        let Some(module) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        for (path, executable) in [
            (dir.join(format!("{module}.so")), false),
            (dir.join(module), true),
        ] {
            if path.is_file() {
                out.push((path, executable));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Checks every artifact under `device` against the `provided` libraries.
pub fn check(device: &Path, provided: &[Exported]) -> Result<Report> {
    if provided.is_empty() {
        bail!("no provided library to check against");
    }
    let mut report = Report::default();
    for (path, executable) in artifacts(device)? {
        let Some(elf) = read_elf(&path, executable)? else {
            continue;
        };
        report.artifacts += 1;
        let name = path
            .strip_prefix(device)
            .unwrap_or(&path)
            .display()
            .to_string();
        report.violations.extend(violations(
            &name,
            &elf,
            executable,
            provided,
            &mut report.fallbacks,
        ));
    }
    Ok(report)
}

fn violations(
    name: &str,
    elf: &Elf,
    executable: bool,
    provided: &[Exported],
    fallbacks: &mut usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let is_provided = |n: &String| provided.iter().any(|p| p.soname == *n);
    // the provided entries must form a prefix of the needed list
    if let Some(first_other) = elf.needed.iter().position(|n| !is_provided(n)) {
        for (i, n) in elf.needed.iter().enumerate().skip(first_other) {
            if is_provided(n) {
                out.push(format!(
                    "{name}: {n} is DT_NEEDED #{}, after {}",
                    i + 1,
                    elf.needed[first_other]
                ));
            }
        }
    }
    for p in provided {
        for (sym, weak) in &elf.defined {
            if !p.symbols.contains(sym) {
                continue;
            }
            if executable {
                out.push(format!(
                    "{name}: executable defines {sym}, which {} provides",
                    p.soname
                ));
            } else if *weak {
                *fallbacks += 1;
            } else {
                out.push(format!("{name}: strong {sym}, which {} provides", p.soname));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exported(soname: &str, symbols: &[&str]) -> Exported {
        Exported {
            soname: soname.into(),
            symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn provided() -> [Exported; 2] {
        [
            exported(
                "libatl_bionic.so.0.0.0",
                &["__system_property_get", "__system_property_find"],
            ),
            exported("libcrypto.so", &["ATL_SHA256_Init"]),
        ]
    }

    fn elf(needed: &[&str], defined: &[(&str, bool)]) -> Elf {
        Elf {
            needed: needed.iter().map(|s| (*s).to_owned()).collect(),
            soname: None,
            defined: defined.iter().map(|(s, w)| ((*s).to_owned(), *w)).collect(),
        }
    }

    #[test]
    fn weak_fallbacks_lose_strong_ones_fail() {
        let p = provided();
        let mut weak = 0;
        // libbase: needs the provided libraries first, carries weak fallbacks
        let libbase = elf(
            &[
                "libatl_bionic.so.0.0.0",
                "libcrypto.so",
                "liblog.so",
                "libc.so.6",
            ],
            &[("__system_property_get", true), ("unrelated", false)],
        );
        assert!(violations("libbase.so", &libbase, false, &p, &mut weak).is_empty());
        assert_eq!(weak, 1);
        // a strong duplicate, a provided library late in the list
        let bad = elf(
            &["libatl_bionic.so.0.0.0", "liblog.so", "libcrypto.so"],
            &[("__system_property_find", false)],
        );
        let v = violations("bad.so", &bad, false, &p, &mut weak);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(
            v[0].contains("libcrypto.so is DT_NEEDED #3, after liblog.so"),
            "{v:?}"
        );
        assert!(v[1].contains("strong __system_property_find"), "{v:?}");
        // an executable's own definition always binds first, weak or not
        let exe = elf(
            &["libatl_bionic.so.0.0.0"],
            &[("__system_property_get", true)],
        );
        assert_eq!(violations("aidl", &exe, true, &p, &mut weak).len(), 1);
        // not needing a provided library at all is fine: the process's first needed list brings it
        let leaf = elf(&["libc.so.6"], &[]);
        assert!(violations("leaf.so", &leaf, false, &p, &mut weak).is_empty());
    }
}
