// SPDX-License-Identifier: MIT
//! The kernel argument block the guest linker reads (bionic `KernelArgumentBlock`): `argc`, argv pointers, NULL, envp pointers, NULL, auxv pairs, AT_NULL -- the layout fs/binfmt_elf.c `create_elf_tables` puts at the initial stack pointer, every entry one `elf_addr_t`: the guest word (4 bytes for a 32-bit process, 8 for a 64-bit one).
//! auxv = the kernel's for this guest (`Active::kernel_auxv`: hwcaps, page size, clock tick, ids, AT_SECURE, AT_RANDOM, AT_MINSIGSTKSZ as the kernel -- or QEMU, for an emulated guest -- gave them) with the entries that describe the image replaced as the kernel sets them for a directly run interpreter: AT_PHDR/AT_PHNUM/AT_PHENT = the linker's, AT_BASE = 0, AT_ENTRY = the linker's entry, AT_EXECFN = its path; AT_SYSINFO_EHDR = the NB vdso.
//! One allocation from the engine (`Active::reserve`): the words, then the argv/envp strings, so a 32-bit guest can address all of it. Never freed: bionic keeps pointers into it for the process's life (`environ`, `__libc_shared_globals()->auxv`).

use std::os::unix::ffi::OsStrExt;

use super::Config;
use super::image::Image;

/// This process's auxv (`/proc/self/auxv`), AT_NULL dropped: the identity guest's kernel auxv.
#[cfg(atl_nb_identity)]
pub fn process_auxv() -> Result<Vec<(usize, usize)>, String> {
    let raw = std::fs::read("/proc/self/auxv").map_err(|e| format!("/proc/self/auxv: {e}"))?;
    Ok(read_auxv(&raw, std::mem::size_of::<usize>()))
}

/// AT_NULL-terminated auxv pairs of `word`-byte native-endian words (a 32-bit process's are 4 bytes each), AT_NULL and anything after dropped.
pub fn read_auxv(raw: &[u8], word: usize) -> Vec<(usize, usize)> {
    let get = |b: &[u8]| -> usize {
        match word {
            4 => u32::from_ne_bytes(b.try_into().unwrap_or_default()) as usize,
            _ => u64::from_ne_bytes(b.try_into().unwrap_or_default()) as usize,
        }
    };
    raw.chunks_exact(2 * word)
        .map(|c| (get(&c[..word]), get(&c[word..])))
        .take_while(|&(k, _)| k != libc::AT_NULL as usize)
        .collect()
}

/// The block's address. `kernel`: the kernel's auxv for this guest (`Active::kernel_auxv`), AT_NULL excluded. The guest word is the linker's ELF class.
pub fn build(
    cfg: &Config,
    linker: &Image,
    vdso: &Image,
    kernel: &[(usize, usize)],
) -> Result<usize, String> {
    let path = cfg
        .library_path
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(":");
    let env0 = format!("LD_LIBRARY_PATH={path}");
    let argv: [&[u8]; 2] = [
        cfg.linker.as_os_str().as_bytes(),
        cfg.exe.as_os_str().as_bytes(),
    ];
    let envp: [&[u8]; 1] = [env0.as_bytes()];
    let image = [
        (libc::AT_PHDR as usize, linker.phdr),
        (libc::AT_PHNUM as usize, linker.phnum),
        (libc::AT_PHENT as usize, linker.phent),
        (libc::AT_BASE as usize, 0),
        (libc::AT_ENTRY as usize, linker.entry),
        (libc::AT_SYSINFO_EHDR as usize, vdso.ehdr),
    ];
    let len = block(0, linker.word, &argv, &envp, kernel, &image)?.len();
    let base =
        super::active().reserve(len, atl_engine_api::PROT_READ | atl_engine_api::PROT_WRITE)?;
    let bytes = block(base, linker.word, &argv, &envp, kernel, &image)?;
    // SAFETY: `base` is a fresh writable allocation of `len` bytes, and `bytes` is `len` long.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base as *mut u8, bytes.len()) };
    Ok(base)
}

/// The block's bytes at guest address `base`: the words (`word` bytes each, native endian), then each argv and envp string NUL-terminated, in that order. auxv = `kernel` without the keys `image` replaces, then `image`, then AT_EXECFN = argv[0]'s address, then AT_NULL. Fails when a value does not fit the guest word or a string holds a NUL.
fn block(
    base: usize,
    word: usize,
    argv: &[&[u8]],
    envp: &[&[u8]],
    kernel: &[(usize, usize)],
    image: &[(usize, usize)],
) -> Result<Vec<u8>, String> {
    let replaced = |k: usize| k == libc::AT_EXECFN as usize || image.iter().any(|&(r, _)| r == k);
    let kept: Vec<_> = kernel.iter().filter(|&&(k, _)| !replaced(k)).collect();
    // argc, argv + NULL, envp + NULL, (kept + image + AT_EXECFN + AT_NULL) pairs
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * (kept.len() + image.len() + 2);
    let mut strings = Vec::new();
    let mut at = Vec::new();
    for s in argv.iter().chain(envp) {
        if s.contains(&0) {
            return Err(format!(
                "argument {:?} holds a NUL",
                String::from_utf8_lossy(s)
            ));
        }
        at.push(base + words * word + strings.len());
        strings.extend_from_slice(s);
        strings.push(0);
    }
    let (argv_at, envp_at) = at.split_at(argv.len());
    let mut w = vec![argv.len()];
    w.extend(argv_at);
    w.push(0);
    w.extend(envp_at);
    w.push(0);
    for &&(k, v) in &kept {
        w.extend([k, v]);
    }
    for &(k, v) in image {
        w.extend([k, v]);
    }
    w.extend([
        libc::AT_EXECFN as usize,
        argv_at.first().copied().unwrap_or(0),
    ]);
    w.extend([libc::AT_NULL as usize, 0]);
    debug_assert_eq!(w.len(), words);
    let mut out = Vec::with_capacity(words * word + strings.len());
    for v in w {
        match word {
            4 => out.extend(
                u32::try_from(v)
                    .map_err(|_| format!("{v:#x} does not fit a 32-bit guest word"))?
                    .to_ne_bytes(),
            ),
            _ => out.extend((v as u64).to_ne_bytes()),
        }
    }
    out.extend(strings);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // auxv keys from the kernel's uapi linux/auxvec.h, written out so the test does not take them from the code under test's source (libc)
    const AT_NULL: u32 = 0;
    const AT_PHDR: u32 = 3;
    const AT_PAGESZ: u32 = 6;
    const AT_ENTRY: u32 = 9;
    const AT_EXECFN: u32 = 31;

    /// A 32-bit process's block as fs/binfmt_elf.c `create_elf_tables` lays it (`elf_addr_t` = u32 for ELFCLASS32): argc, argv[], NULL, envp[], NULL, auxv pairs, AT_NULL 0 -- all 4-byte words; the strings follow at the addresses the pointers name. A kernel AT_ENTRY is replaced by the image's and a kernel AT_EXECFN dropped for argv[0]'s.
    #[test]
    fn arm_block() {
        let base = 0x1000;
        let bytes = block(
            base,
            4,
            &[b"/l", b"/e"],
            &[b"A=1"],
            &[(6, 4096), (9, 0xdead), (31, 0xbeef)],
            &[(3, 0x2034), (9, 0x2100)],
        )
        .expect("fits");
        // 1 + 2 + 1 + 1 + 1 + 2 * (1 kept + 2 image + EXECFN + NULL) = 16 words = 64 bytes; strings at 0x1040: "/l\0" "/e\0" "A=1\0"
        let words: [u32; 16] = [
            2, 0x1040, 0x1043, 0, 0x1046, 0, //
            AT_PAGESZ, 4096, AT_PHDR, 0x2034, AT_ENTRY, 0x2100, AT_EXECFN, 0x1040, AT_NULL, 0,
        ];
        let mut want: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        want.extend(b"/l\0/e\0A=1\0");
        assert_eq!(bytes, want);
    }

    /// A value beyond 32 bits cannot go in a 32-bit guest's block: refused, not truncated.
    #[test]
    fn arm_block_refuses_wide_values() {
        assert!(block(0x1000, 4, &[b"/l"], &[], &[], &[(3, 1 << 32)]).is_err());
    }

    /// The 64-bit layout keeps 8-byte words.
    #[test]
    fn arm64_block_words() {
        let bytes = block(0x1000, 8, &[b"/l"], &[], &[], &[]).expect("fits");
        // argc, argv0, NULL, NULL (envp), AT_EXECFN pair, AT_NULL pair = 8 words; argv0 at 0x1000 + 64
        let words: [u64; 8] = [1, 0x1040, 0, 0, AT_EXECFN as u64, 0x1040, AT_NULL as u64, 0];
        let mut want: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        want.extend(b"/l\0");
        assert_eq!(bytes, want);
    }

    /// A 32-bit auxv (QEMU's for an arm guest, 4-byte words) reads pairwise up to AT_NULL.
    #[test]
    fn arm_auxv_read() {
        let raw: Vec<u8> = [AT_PAGESZ, 4096, AT_PHDR, 0x40, AT_NULL, 0, 7, 7]
            .iter()
            .flat_map(|w| w.to_ne_bytes())
            .collect();
        assert_eq!(read_auxv(&raw, 4), vec![(6, 4096), (3, 0x40)]);
    }
}
