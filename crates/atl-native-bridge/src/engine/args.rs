// SPDX-License-Identifier: MIT
//! The kernel argument block the guest linker reads (bionic `KernelArgumentBlock`): `argc`, argv pointers, NULL, envp pointers, NULL, auxv pairs, AT_NULL -- the layout fs/binfmt_elf.c `create_elf_tables` puts at the initial stack pointer.
//! auxv = the kernel's for this guest (`arch::kernel_auxv`: hwcaps, page size, clock tick, ids, AT_SECURE, AT_RANDOM, AT_MINSIGSTKSZ as the kernel -- or QEMU, for an emulated guest -- gave them) with the entries that describe the image replaced as the kernel sets them for a directly run interpreter: AT_PHDR/AT_PHNUM/AT_PHENT = the linker's, AT_BASE = 0, AT_ENTRY = the linker's entry, AT_EXECFN = its path; AT_SYSINFO_EHDR = the NB vdso.
//! Leaked: bionic keeps pointers into it for the process's life (`environ`, `__libc_shared_globals()->auxv`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

use super::Config;
use super::image::Image;

/// Elf64_Phdr size (AT_PHENT).
const PHENT: usize = 56;

/// The argument block and the host memory the guest reads through it.
pub struct Block {
    /// The block's address (`__linker_init`'s argument).
    pub addr: usize,
    /// (start, length) of the block and of each string it points to: guest syscalls take these pointers (the linker `open`s argv[1]), and an emulator checks them (`arch::map_guest`).
    pub ranges: Vec<(usize, usize)>,
}

/// A leaked C string of `bytes`: its address, its range recorded in `ranges`.
fn leak_cstr(bytes: &[u8], ranges: &mut Vec<(usize, usize)>) -> Result<usize, String> {
    let c = CString::new(bytes).map_err(|e| e.to_string())?;
    let len = c.as_bytes_with_nul().len();
    let at = c.into_raw() as usize;
    ranges.push((at, len));
    Ok(at)
}

/// This process's auxv (`/proc/self/auxv`), AT_NULL dropped: the identity guest's kernel auxv.
#[cfg_attr(
    feature = "qemu-guest",
    allow(
        dead_code,
        reason = "the QEMU guest's kernel auxv is QEMU's (arch::kernel_auxv)"
    )
)]
pub fn process_auxv() -> Result<Vec<(usize, usize)>, String> {
    let raw = std::fs::read("/proc/self/auxv").map_err(|e| format!("/proc/self/auxv: {e}"))?;
    let word = std::mem::size_of::<usize>();
    Ok(raw
        .chunks_exact(2 * word)
        .map(|c| {
            let (k, v) = c.split_at(word);
            (
                usize::from_ne_bytes(k.try_into().unwrap_or_default()),
                usize::from_ne_bytes(v.try_into().unwrap_or_default()),
            )
        })
        .take_while(|&(k, _)| k != libc::AT_NULL as usize)
        .collect())
}

/// The block. `kernel`: the kernel's auxv for this guest (`arch::kernel_auxv`), AT_NULL excluded.
pub fn build(
    cfg: &Config,
    linker: &Image,
    vdso: &Image,
    kernel: &[(usize, usize)],
) -> Result<Block, String> {
    let mut ranges = Vec::new();
    let argv0 = leak_cstr(cfg.linker.as_os_str().as_bytes(), &mut ranges)?;
    let argv1 = leak_cstr(cfg.exe.as_os_str().as_bytes(), &mut ranges)?;
    let path = cfg
        .library_path
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(":");
    let env0 = leak_cstr(format!("LD_LIBRARY_PATH={path}").as_bytes(), &mut ranges)?;
    let replaced: &[(libc::c_ulong, usize)] = &[
        (libc::AT_PHDR, linker.phdr),
        (libc::AT_PHNUM, linker.phnum),
        (libc::AT_PHENT, PHENT),
        (libc::AT_BASE, 0),
        (libc::AT_ENTRY, linker.entry),
        (libc::AT_EXECFN, argv0),
        (libc::AT_SYSINFO_EHDR, vdso.ehdr),
    ];
    let mut block = vec![2, argv0, argv1, 0, env0, 0];
    for &(k, v) in kernel {
        if !replaced.iter().any(|&(r, _)| r as usize == k) {
            block.extend([k, v]);
        }
    }
    for &(k, v) in replaced {
        block.extend([k as usize, v]);
    }
    block.extend([libc::AT_NULL as usize, 0]);
    let words = Box::leak(block.into_boxed_slice());
    let addr = words.as_ptr() as usize;
    ranges.push((addr, std::mem::size_of_val(words)));
    Ok(Block { addr, ranges })
}
