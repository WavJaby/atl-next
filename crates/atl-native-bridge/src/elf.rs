// SPDX-License-Identifier: MIT
//! Which libraries the NB core takes: `isSupported` answers from the file alone, as libnativeloader asks it after glibc's `dlopen` refused the path.
//! Ours = an ELF of the host's machine (route N: identity engine, bridge-1 plan section 0) that links against bionic: DT_NEEDED `libc.so` (bionic's soname; glibc's is `libc.so.6`).
//! No symbol-version marker: NDK stubs without version nodes produced binaries with no verneed at all, and those are the old games; a `LIBC` verneed exists only in newer NDK output.
//! A foreign machine is the QEMU engine's (bridge 2) and is refused until that engine exists.
//! The host machine is read from `/proc/self/exe`, so no arch name or constant is stored here (arches.toml stays the only arch list).

use object::read::elf::{Dyn, FileHeader};
use object::{Endianness, FileKind, elf};

/// bionic's soname.
const BIONIC_LIBC: &[u8] = b"libc.so";

/// What `classify` makes of a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Host machine, linked against bionic: ours.
    Bridged,
    /// Not an ELF object this reader parses (the reason).
    NotElf(String),
    /// Another machine's code: the QEMU engine's, not built yet.
    ForeignMachine {
        lib: elf::Machine,
        host: elf::Machine,
    },
    /// Host machine but no bionic marker: glibc's loader's business, not ours.
    NotBionic,
}

/// `lib`'s verdict on a host whose ELF machine is `host`.
pub fn classify(lib: &[u8], host: elf::Machine) -> Verdict {
    let parsed = match FileKind::parse(lib) {
        Ok(FileKind::Elf32) => markers::<elf::FileHeader32<Endianness>>(lib),
        Ok(FileKind::Elf64) => markers::<elf::FileHeader64<Endianness>>(lib),
        Ok(k) => return Verdict::NotElf(format!("{k:?}")),
        Err(e) => return Verdict::NotElf(e.to_string()),
    };
    let m = match parsed {
        Ok(m) => m,
        Err(e) => return Verdict::NotElf(e.to_string()),
    };
    if m.machine != host {
        return Verdict::ForeignMachine {
            lib: m.machine,
            host,
        };
    }
    if m.needed.iter().any(|n| n == BIONIC_LIBC) {
        Verdict::Bridged
    } else {
        Verdict::NotBionic
    }
}

/// The ELF machine of `data`'s header.
pub fn machine(data: &[u8]) -> Result<elf::Machine, String> {
    let m = match FileKind::parse(data).map_err(|e| e.to_string())? {
        FileKind::Elf32 => head::<elf::FileHeader32<Endianness>>(data),
        FileKind::Elf64 => head::<elf::FileHeader64<Endianness>>(data),
        k => return Err(format!("{k:?}")),
    };
    m.map_err(|e| e.to_string())
}

/// The host's ELF machine: this process's executable's.
pub fn host_machine() -> Result<elf::Machine, String> {
    let exe = std::fs::read("/proc/self/exe").map_err(|e| format!("/proc/self/exe: {e}"))?;
    machine(&exe)
}

fn head<F: FileHeader<Endian = Endianness>>(data: &[u8]) -> object::Result<elf::Machine> {
    let h = F::parse(data)?;
    Ok(h.e_machine(h.endian()?))
}

struct Markers {
    machine: elf::Machine,
    needed: Vec<Vec<u8>>,
}

/// Read through the section table (`.dynamic`): NDK output keeps it; a library without one has no marker and is refused.
fn markers<F: FileHeader<Endian = Endianness>>(data: &[u8]) -> object::Result<Markers> {
    let h = F::parse(data)?;
    let endian = h.endian()?;
    let sections = h.sections(endian, data)?;
    let mut needed = Vec::new();
    if let Some((dyns, link)) = sections.dynamic(endian, data)? {
        let strings = sections.strings(endian, data, link)?;
        for d in dyns {
            if d.tag(endian) == elf::DT_NEEDED
                && let Ok(name) = d.string(endian, strings)
            {
                needed.push(name.to_vec());
            }
        }
    }
    Ok(Markers {
        machine: h.e_machine(endian),
        needed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test binary itself: a glibc executable of the host machine.
    fn own_exe() -> Vec<u8> {
        std::fs::read("/proc/self/exe").unwrap()
    }

    #[test]
    fn glibc_object_of_host_machine_is_not_ours() {
        let exe = own_exe();
        let host = host_machine().unwrap();
        assert_eq!(machine(&exe).unwrap(), host);
        assert_eq!(classify(&exe, host), Verdict::NotBionic);
    }

    /// Same file judged on a host of another machine (`host + 1`: any other e_machine): foreign, before any marker is looked at.
    #[test]
    fn other_machine_is_foreign() {
        let exe = own_exe();
        let host = host_machine().unwrap();
        let other = elf::Machine(host.0.wrapping_add(1));
        assert_eq!(
            classify(&exe, other),
            Verdict::ForeignMachine {
                lib: host,
                host: other
            }
        );
    }

    #[test]
    fn non_elf_is_refused() {
        let host = host_machine().unwrap();
        assert!(matches!(classify(b"#!/bin/sh\n", host), Verdict::NotElf(_)));
        assert!(matches!(classify(b"", host), Verdict::NotElf(_)));
        // ELF magic and class, header cut short
        assert!(matches!(
            classify(b"\x7fELF\x02\x01\x01", host),
            Verdict::NotElf(_)
        ));
    }
}
