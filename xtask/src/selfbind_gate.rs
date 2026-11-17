// SPDX-License-Identifier: MIT
//! Gate after `xtask native`'s build: no converter-built glibc-side shared library reaches a strong definition of its own through a dynamic relocation.
//! Such a relocation is resolved by ld.so's scope search, global scope first, so any library already in the global scope that defines the same name takes the reference over (seen: GTK's GL renderer put Mesa's libLLVM in the global scope before ART loaded; libart bound LLVM's `__jit_debug_descriptor` and aborted).
//! A device cannot hit this: its linker namespaces keep foreign libraries out. Here the converter links glibc-side shared libraries `-Bsymbolic-non-weak` (bp2ninja ninja.go `selfBind`); the gate checks the class on the built files, whatever the host loads.
//! Weak and non-default-visibility definitions are not checked: weak ones are meant to lose to a provided library's strong one (provided_gate.rs), hidden/protected ones already bind locally.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use object::read::elf::{ElfFile64, ProgramHeader};
use object::{
    Endianness, Object, ObjectSymbol, ObjectSymbolTable, RelocationTarget, SymbolFlags, elf,
};

/// Every strong default-visibility symbol of `artifacts` that its own file reaches through a dynamic relocation, as `<artifact>: <symbol>`; plus how many shared objects were read.
pub fn check(out: &Path, artifacts: &[PathBuf]) -> Result<(usize, Vec<String>)> {
    let mut read = 0;
    let mut violations = Vec::new();
    for path in artifacts {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let Some(names) = self_refs(&data).with_context(|| format!("{}", path.display()))? else {
            continue;
        };
        read += 1;
        let name = path.strip_prefix(out).unwrap_or(path).display().to_string();
        violations.extend(names.into_iter().map(|s| format!("{name}: {s}")));
    }
    Ok((read, violations))
}

/// The strong default-visibility symbols a shared object defines and reaches through its own dynamic relocations, sorted, each once; None for a file that is not an ELF shared object (an executable, PIE = ET_DYN + PT_INTERP included, binds its own definitions at link time).
fn self_refs(data: &[u8]) -> Result<Option<Vec<String>>> {
    // 64-bit only, as provided_gate.rs: every arches.toml host row is
    let Ok(file) = ElfFile64::<Endianness>::parse(data) else {
        return Ok(None);
    };
    let endian = file.endian();
    let interp = file
        .elf_program_headers()
        .iter()
        .any(|p| p.p_type(endian) == elf::PT_INTERP);
    if file.kind() != object::ObjectKind::Dynamic || interp {
        return Ok(None);
    }
    let Some(dynsym) = file.dynamic_symbol_table() else {
        return Ok(Some(Vec::new()));
    };
    let mut out = Vec::new();
    for (_, r) in file.dynamic_relocations().into_iter().flatten() {
        let RelocationTarget::Symbol(index) = r.target() else {
            continue;
        };
        let sym = dynsym.symbol_by_index(index)?;
        let SymbolFlags::Elf { st_info, st_other } = sym.flags() else {
            continue;
        };
        if sym.is_definition()
            && st_info.st_bind() == elf::STB_GLOBAL
            && st_other.visibility() == elf::STV_DEFAULT
        {
            out.push(sym.name()?.to_owned());
        }
    }
    out.sort();
    out.dedup();
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library defining a strong datum, a strong function and a weak datum, each reached through its address: default link -> the two strong ones reach through dynamic relocations;
    /// the converter's flag -> none (the weak one stays preemptible and is not reported either way). Built with the build machine's clang, the converter's link driver.
    #[test]
    fn strong_self_references_fail_until_bound_locally() {
        let dir = std::env::temp_dir().join(format!("selfbind-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.c");
        std::fs::write(
            &src,
            "int strong_d = 1; __attribute__((weak)) int weak_d = 2; int strong_f(void) { return 3; }\n\
             int *pd(void) { return &strong_d; } int *pw(void) { return &weak_d; } int (*pf(void))(void) { return strong_f; }\n",
        )
        .unwrap();
        let build = |flag: Option<&str>| {
            let so = dir.join(format!("a{}.so", u8::from(flag.is_some())));
            let status = std::process::Command::new("clang-19")
                .args(["-fuse-ld=lld", "-fPIC", "-shared", "-O1"])
                .args(flag)
                .arg(&src)
                .arg("-o")
                .arg(&so)
                .status()
                .unwrap();
            assert!(status.success());
            self_refs(&std::fs::read(&so).unwrap()).unwrap()
        };
        let default = build(None);
        let bound = build(Some("-Wl,-Bsymbolic-non-weak"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            default,
            Some(vec!["strong_d".to_owned(), "strong_f".to_owned()])
        );
        assert_eq!(bound, Some(Vec::new()));
    }

    /// The built xtask binary: a PIE executable -> not checked; a non-ELF -> not checked.
    #[test]
    fn executables_and_other_files_are_skipped() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(self_refs(&exe).unwrap(), None);
        assert_eq!(self_refs(b"#!/bin/sh\n").unwrap(), None);
    }
}
