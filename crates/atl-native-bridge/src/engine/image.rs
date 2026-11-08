// SPDX-License-Identifier: MIT
//! Maps an ELF's `PT_LOAD` segments the way the kernel's ELF loader does for an `ET_DYN` image (fs/binfmt_elf.c `load_elf_binary` / `elf_map`): one reservation spanning all segments at a kernel-chosen base, each segment mapped from the file at `bias + p_vaddr` page-rounded, the tail past `p_filesz` zeroed, then the segment's own protections.
//! Segments stay writable until `seal`: the engine edits the NB vdso's dynamic symbols in between (vdso.rs).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

use object::elf;
use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
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
    /// runtime entry point (auxv AT_ENTRY)
    pub entry: usize,
    /// the file, for symbol and section lookups
    data: Vec<u8>,
    /// (page start, length, final protection) per segment, applied by `seal`
    segments: Vec<(usize, usize, i32)>,
}

fn page() -> usize {
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
        let file = ElfFile64::<Endianness>::parse(&*data)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let endian = file.endian();
        let hdr = file.elf_header();
        if hdr.e_type(endian) != elf::ET_DYN {
            return Err(format!("{}: not ET_DYN", path.display()));
        }
        let loads: Vec<_> = file
            .elf_program_headers()
            .iter()
            .filter(|p| p.p_type(endian) == elf::PT_LOAD)
            .map(|p| {
                (
                    p.p_vaddr(endian) as usize,
                    p.p_offset(endian) as usize,
                    p.p_filesz(endian) as usize,
                    p.p_memsz(endian) as usize,
                    p.p_flags(endian),
                )
            })
            .collect();
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
        // SAFETY: a fresh anonymous PROT_NONE reservation; nothing else refers to it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                hi - lo,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(err("reserve", path));
        }
        let bias = (base as usize).wrapping_sub(lo);
        let mut segments = Vec::new();
        for &(vaddr, offset, filesz, memsz, flags) in &loads {
            let has = |f: elf::ProgramFlags| flags.0 & f.0 != 0;
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
            phdr: ehdr + hdr.e_phoff(endian) as usize,
            phnum: hdr.e_phnum(endian) as usize,
            entry: bias + hdr.e_entry(endian) as usize,
            data,
            segments,
        })
    }

    /// (page start, length, `PROT_*` of the segment) per `PT_LOAD`, page-rounded: the ranges a guest maps (`arch::qemu` registers them with QEMU).
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
        let file = ElfFile64::<Endianness>::parse(&*self.data).ok()?;
        file.symbols()
            .chain(file.dynamic_symbols())
            .find(|s| s.name() == Ok(name) && s.is_definition())
            .map(|s| self.bias + s.address() as usize)
    }

    /// (name, runtime address of its `st_value` field) of every defined dynamic symbol.
    pub fn dynsym_values(&self) -> Vec<(String, usize)> {
        let Ok(file) = ElfFile64::<Endianness>::parse(&*self.data) else {
            return Vec::new();
        };
        let Some(dynsym) = file.section_by_name(".dynsym") else {
            return Vec::new();
        };
        // Elf64_Sym: st_name u32, st_info u8, st_other u8, st_shndx u16, st_value u64 (offset 8), st_size u64 -> 24 bytes
        const SYM_BYTES: usize = 24;
        const VALUE_OFFSET: usize = 8;
        let base = self.bias + dynsym.address() as usize;
        file.dynamic_symbols()
            .filter(|s| s.is_definition())
            .filter_map(|s| {
                Some((
                    s.name().ok()?.to_owned(),
                    base + s.index().0 * SYM_BYTES + VALUE_OFFSET,
                ))
            })
            .collect()
    }
}
