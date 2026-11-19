// SPDX-License-Identifier: MIT
//! Maps an ELF's `PT_LOAD` segments the way the kernel's ELF loader does for an `ET_DYN` image (fs/binfmt_elf.c `load_elf_binary` / `elf_map`): one reservation spanning all segments at a kernel-chosen base, each segment mapped from the file at `bias + p_vaddr` page-rounded, the tail past `p_filesz` zeroed, then the segment's own protections.
//! Segments stay writable until `seal`: the engine edits the NB vdso's dynamic symbols in between (vdso.rs).
//! ELFCLASS32 (arm guest) and ELFCLASS64 both parse; the class fixes the guest word (`Image::word`): Elf32 fields are 4 bytes, Elf64's 8. The reservation comes from the engine (`Active::reserve`), so a 32-bit guest's image lies below 4 GiB.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use object::elf;
use object::read::elf::{ElfFile, FileHeader, ProgramHeader};
use object::{Endianness, Object, ObjectSection, ObjectSymbol};

/// A mapped image.
pub struct Image {
    /// load bias: runtime address = bias + p_vaddr
    pub bias: usize,
    /// runtime address of the ELF header (the segment holding file offset 0)
    pub ehdr: usize,
    /// runtime address of the program headers, and their count (auxv AT_PHDR / AT_PHNUM)
    pub phdr: usize,
    pub phnum: usize,
    /// program header entry size (auxv AT_PHENT): Elf32_Phdr 32, Elf64_Phdr 56
    pub phent: usize,
    /// runtime entry point (auxv AT_ENTRY)
    pub entry: usize,
    /// the guest word in bytes, from EI_CLASS: 4 (ELFCLASS32) or 8 (ELFCLASS64)
    pub word: usize,
    /// the file, for symbol and section lookups
    data: Vec<u8>,
    /// (page start, length, final protection) per segment, applied by `seal`
    segments: Vec<(usize, usize, i32)>,
}

/// What the ELF and program headers say, before any mapping.
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    /// (p_vaddr, p_offset, p_filesz, p_memsz, p_flags) per PT_LOAD
    loads: Vec<(usize, usize, usize, usize, u32)>,
    phoff: usize,
    phnum: usize,
    phent: usize,
    entry: usize,
    word: usize,
}

/// [`Layout`] of an ELF of either class, `ET_DYN` only.
fn layout(data: &[u8]) -> Result<Layout, String> {
    match object::FileKind::parse(data).map_err(|e| e.to_string())? {
        object::FileKind::Elf32 => layout_of::<elf::FileHeader32<Endianness>>(data, 4),
        object::FileKind::Elf64 => layout_of::<elf::FileHeader64<Endianness>>(data, 8),
        other => Err(format!("not an ELF: {other:?}")),
    }
}

fn layout_of<Elf: FileHeader<Endian = Endianness>>(
    data: &[u8],
    word: usize,
) -> Result<Layout, String> {
    let file = ElfFile::<Elf>::parse(data).map_err(|e| e.to_string())?;
    let endian = file.endian();
    let hdr = file.elf_header();
    if hdr.e_type(endian) != elf::ET_DYN {
        return Err("not ET_DYN".into());
    }
    let n = |v: Elf::Word| -> usize { Into::<u64>::into(v) as usize };
    let loads = file
        .elf_program_headers()
        .iter()
        .filter(|p| p.p_type(endian) == elf::PT_LOAD)
        .map(|p| {
            (
                n(p.p_vaddr(endian)),
                n(p.p_offset(endian)),
                n(p.p_filesz(endian)),
                n(p.p_memsz(endian)),
                p.p_flags(endian).0,
            )
        })
        .collect();
    Ok(Layout {
        loads,
        phoff: n(hdr.e_phoff(endian)),
        phnum: hdr.e_phnum(endian) as usize,
        phent: hdr.e_phentsize(endian) as usize,
        entry: n(hdr.e_entry(endian)),
        word,
    })
}

/// The host page size; QEMU linux-user gives the guest the same (bionic's `page_size()`).
pub fn page() -> usize {
    // SAFETY: sysconf has no memory effects.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Writes `bytes` at `addr` in sealed image text: the pages holding it are writable for the copy only, then read + exec again (a stub rewrite after `seal`).
pub fn write_sealed(addr: usize, bytes: &[u8]) {
    let size = page();
    let pg = addr & !(size - 1);
    let last = addr + bytes.len() - 1;
    let len = (last & !(size - 1)) + size - pg;
    // SAFETY: the image's own text pages around `addr`: RWX for the copy, then RX again; the caller passes an address inside a sealed image it mapped.
    unsafe {
        libc::mprotect(
            pg as *mut _,
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        );
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
        libc::mprotect(pg as *mut _, len, libc::PROT_READ | libc::PROT_EXEC);
    }
}

fn err(what: &str, path: &Path) -> String {
    format!(
        "{}: {what}: {}",
        path.display(),
        std::io::Error::last_os_error()
    )
}

impl Image {
    pub fn map(path: &Path) -> Result<Image, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let Layout {
            loads,
            phoff,
            phnum,
            phent,
            entry,
            word,
        } = layout(&data).map_err(|e| format!("{}: {e}", path.display()))?;
        let pg = page();
        let down = |v: usize| v & !(pg - 1);
        let up = |v: usize| (v + pg - 1) & !(pg - 1);
        let lo = loads.iter().map(|l| down(l.0)).min().ok_or("no PT_LOAD")?;
        let hi = loads
            .iter()
            .map(|l| up(l.0 + l.3))
            .max()
            .ok_or("no PT_LOAD")?;
        let fd = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // the guest-addressable reservation; segments replace it with MAP_FIXED, `qemu::register` marks their final protections
        let base = super::active()
            .reserve(hi - lo, 0)
            .map_err(|e| format!("{}: reserve: {e}", path.display()))?;
        let bias = base.wrapping_sub(lo);
        let mut segments = Vec::new();
        for &(vaddr, offset, filesz, memsz, flags) in &loads {
            let has = |f: elf::ProgramFlags| flags & f.0 != 0;
            let prot = (if has(elf::PF_R) { libc::PROT_READ } else { 0 })
                | (if has(elf::PF_W) { libc::PROT_WRITE } else { 0 })
                | (if has(elf::PF_X) { libc::PROT_EXEC } else { 0 });
            let start = down(bias + vaddr);
            let file_end = up(bias + vaddr + filesz);
            let mem_end = up(bias + vaddr + memsz);
            if file_end > start {
                // SAFETY: MAP_FIXED inside our own reservation; vaddr and offset are congruent modulo the page size (ELF requirement, checked by the kernel too).
                let m = unsafe {
                    libc::mmap(
                        start as *mut _,
                        file_end - start,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_FIXED,
                        fd.as_raw_fd(),
                        down(offset) as libc::off_t,
                    )
                };
                if m == libc::MAP_FAILED {
                    return Err(err("map segment", path));
                }
                let tail = bias + vaddr + filesz;
                let tail_end = file_end.min(mem_end);
                if tail_end > tail {
                    // SAFETY: the bytes past p_filesz on the last file page, writable and ours.
                    unsafe { std::ptr::write_bytes(tail as *mut u8, 0, tail_end - tail) };
                }
            }
            if mem_end > file_end {
                // SAFETY: anonymous zero pages for the rest of .bss, inside our reservation.
                let m = unsafe {
                    libc::mmap(
                        file_end as *mut _,
                        mem_end - file_end,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                        -1,
                        0,
                    )
                };
                if m == libc::MAP_FAILED {
                    return Err(err("map bss", path));
                }
            }
            segments.push((start, mem_end - start, prot));
        }
        let first = loads
            .iter()
            .find(|l| l.1 == 0)
            .ok_or("no PT_LOAD maps the ELF header")?;
        let ehdr = bias + first.0;
        Ok(Image {
            bias,
            ehdr,
            phdr: ehdr + phoff,
            phnum,
            phent,
            entry: bias + entry,
            word,
            data,
            segments,
        })
    }

    /// The `st_value` at `value_at` (a [`Image::dynsym_values`] address, not yet sealed): the symbol's address before the load bias, read at the image's word.
    pub fn st_value(&self, value_at: usize) -> usize {
        // SAFETY: the st_value field of a .dynsym entry of this mapped image, `word` readable bytes (Image::dynsym_values).
        unsafe {
            match self.word {
                4 => (value_at as *const u32).read_unaligned() as usize,
                _ => (value_at as *const u64).read_unaligned() as usize,
            }
        }
    }

    /// (page start, length, `PROT_*` of the segment) per `PT_LOAD`, page-rounded: the ranges a guest maps (`qemu::register` registers them with QEMU).
    pub fn segments(&self) -> &[(usize, usize, i32)] {
        &self.segments
    }

    /// Applies each segment's own protection.
    pub fn seal(&self) -> Result<(), String> {
        for &(start, len, prot) in &self.segments {
            // SAFETY: our own segment mapping.
            if unsafe { libc::mprotect(start as *mut _, len, prot) } != 0 {
                return Err(format!("mprotect: {}", std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// Runtime address of symbol `name` (static `.symtab` first, then `.dynsym`).
    pub fn symbol(&self, name: &str) -> Option<usize> {
        let file = object::File::parse(&*self.data).ok()?;
        file.symbols()
            .chain(file.dynamic_symbols())
            .find(|s| s.name() == Ok(name) && s.is_definition())
            .map(|s| self.bias + s.address() as usize)
    }

    /// (name, runtime address of its `st_value` field) of every defined dynamic symbol.
    pub fn dynsym_values(&self) -> Vec<(String, usize)> {
        let Ok(file) = object::File::parse(&*self.data) else {
            return Vec::new();
        };
        let Some(dynsym) = file.section_by_name(".dynsym") else {
            return Vec::new();
        };
        let (sym_bytes, value_offset) = sym_layout(self.word);
        let base = self.bias + dynsym.address() as usize;
        file.dynamic_symbols()
            .filter(|s| s.is_definition())
            .filter_map(|s| {
                Some((
                    s.name().ok()?.to_owned(),
                    base + s.index().0 * sym_bytes + value_offset,
                ))
            })
            .collect()
    }
}

/// (entry size, st_value offset) of a symbol table entry for guest word `word`. ELF spec "Symbol Table": Elf32_Sym = st_name u32, st_value u32 (+4), st_size u32, st_info u8, st_other u8, st_shndx u16 -> 16 bytes; Elf64_Sym = st_name u32, st_info u8, st_other u8, st_shndx u16, st_value u64 (+8), st_size u64 -> 24 bytes.
fn sym_layout(word: usize) -> (usize, usize) {
    if word == 4 { (16, 4) } else { (24, 8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ELFCLASS32 little-endian EM_ARM ET_DYN file, written field by field from the ELF32 spec (Elf32_Ehdr 52 bytes, Elf32_Phdr 32 bytes), not by any code under test: two PT_LOADs (R+X at 0 with the headers, RW at 0x11000 with .bss) and a PT_DYNAMIC that is no load.
    fn arm_fixture() -> Vec<u8> {
        let mut f = Vec::new();
        let h = |f: &mut Vec<u8>, v: u16| f.extend(v.to_le_bytes());
        let w = |f: &mut Vec<u8>, v: u32| f.extend(v.to_le_bytes());
        // e_ident: 7f 'E' 'L' 'F', EI_CLASS 1 (ELFCLASS32), EI_DATA 1 (LSB), EI_VERSION 1, padding to 16
        f.extend([0x7f, b'E', b'L', b'F', 1, 1, 1, 0]);
        f.extend([0; 8]);
        h(&mut f, 3); // e_type ET_DYN
        h(&mut f, 40); // e_machine EM_ARM
        w(&mut f, 1); // e_version
        w(&mut f, 0x1234); // e_entry
        w(&mut f, 52); // e_phoff: right after the 52-byte header
        w(&mut f, 0); // e_shoff
        w(&mut f, 0x0500_0000); // e_flags EF_ARM_ABI_VER5
        h(&mut f, 52); // e_ehsize
        h(&mut f, 32); // e_phentsize
        h(&mut f, 3); // e_phnum
        h(&mut f, 0); // e_shentsize
        h(&mut f, 0); // e_shnum
        h(&mut f, 0); // e_shstrndx
        assert_eq!(f.len(), 52);
        // Elf32_Phdr: p_type, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_flags, p_align
        for p in [
            [1, 0, 0, 0, 0x200, 0x200, 5, 0x1000], // PT_LOAD R+X
            [1, 0x1000, 0x11000, 0x11000, 0x10, 0x30, 6, 0x1000], // PT_LOAD RW, .bss tail
            [2, 0x1000, 0x11000, 0x11000, 0x10, 0x10, 6, 4], // PT_DYNAMIC
        ] {
            for v in p {
                w(&mut f, v);
            }
        }
        f.resize(0x1010, 0);
        f
    }

    /// ELF32 headers read at their spec offsets: word 4, AT_PHENT 32, entry, phdr offset, and only the PT_LOADs with their flags.
    #[test]
    fn elf32_layout() {
        assert_eq!(
            layout(&arm_fixture()),
            Ok(Layout {
                loads: vec![(0, 0, 0x200, 0x200, 5), (0x11000, 0x1000, 0x10, 0x30, 6)],
                phoff: 52,
                phnum: 3,
                phent: 32,
                entry: 0x1234,
                word: 4,
            })
        );
    }

    /// The symbol entry layouts of the ELF spec, per class.
    #[test]
    fn symbol_entries() {
        assert_eq!(sym_layout(4), (16, 4));
        assert_eq!(sym_layout(8), (24, 8));
    }

    /// ELFCLASS64 reads as word 8 and AT_PHENT 56 (spec: Elf64_Ehdr 64 bytes, Elf64_Phdr 56 bytes, p_flags second).
    #[test]
    fn elf64_layout() {
        let mut f = Vec::new();
        f.extend([0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        f.extend([0; 8]);
        f.extend(3u16.to_le_bytes()); // ET_DYN
        f.extend(183u16.to_le_bytes()); // EM_AARCH64
        f.extend(1u32.to_le_bytes());
        f.extend(0x40u64.to_le_bytes()); // e_entry
        f.extend(64u64.to_le_bytes()); // e_phoff
        f.extend(0u64.to_le_bytes()); // e_shoff
        f.extend(0u32.to_le_bytes()); // e_flags
        for v in [64u16, 56, 1, 0, 0, 0] {
            f.extend(v.to_le_bytes()); // ehsize, phentsize, phnum, shentsize, shnum, shstrndx
        }
        assert_eq!(f.len(), 64);
        f.extend(1u32.to_le_bytes()); // PT_LOAD
        f.extend(5u32.to_le_bytes()); // p_flags R+X
        for v in [0u64, 0, 0, 0x100, 0x100, 0x1000] {
            f.extend(v.to_le_bytes()); // offset, vaddr, paddr, filesz, memsz, align
        }
        f.resize(0x100, 0);
        let l = layout(&f).expect("elf64 parses");
        assert_eq!((l.word, l.phent, l.phoff, l.entry), (8, 56, 64, 0x40));
        assert_eq!(l.loads, vec![(0, 0, 0x100, 0x100, 5)]);
    }
}
