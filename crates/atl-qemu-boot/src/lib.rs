// SPDX-License-Identifier: MIT
//! The QEMU engine's boot executable (atl-android-paths `engine_boot`): a static guest executable that the engine `.so`'s QEMU main loads first. `_start` hands the host the auxv QEMU built for it through BOOT; the host records that trap, attaches a CPU and starts the guest linker from it (atl-native-bridge engine/qemu `boot`); BOOT's answer in x0 is this process's exit status.
//! Built by xtask (qemu.rs `boot_exe`) as `--crate-type bin` for the guest triple; on any other target (the host lint build) the crate is empty.
//! Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a11 RET (a host call returns), 0x7a13 BOOT (x0 = auxv, x1 = return stub; x0 = exit status).
#![no_std]
#![cfg_attr(target_arch = "aarch64", no_main)]

// Assembly, not Rust: the entry point has no Rust ABI (argc/argv/envp on the stack, no return address) and BOOT is an `svc` with a custom immediate.
// Initial stack (fs/binfmt_elf.c create_elf_tables; QEMU's loader writes the same layout): [sp] = argc, then argv[argc] + NULL, envp + NULL, then the auxv.
// The return stub: where a host call returns (LR points here); the hook leaves the innermost cpu_loop.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".globl _start",
    ".type _start, %function",
    "_start:",
    "    ldr x2, [sp]",
    "    add x3, sp, #8",
    // skip argv and its NULL
    "    add x3, x3, x2, lsl #3",
    "    add x3, x3, #8",
    // skip envp through its NULL: x3 = auxv
    "1:  ldr x4, [x3], #8",
    "    cbnz x4, 1b",
    "    mov x0, x3",
    "    adr x1, 2f",
    "    svc #0x7a13",
    // exit_group(x0) (asm-generic unistd.h __NR_exit_group = 94)
    "    mov x8, #94",
    "    svc #0",
    "2:  svc #0x7a11",
    "    b 2b",
);

/// Required by a `no_std` executable; nothing here can panic (no Rust code runs).
#[cfg(target_arch = "aarch64")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
