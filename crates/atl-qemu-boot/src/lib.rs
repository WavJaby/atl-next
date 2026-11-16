// SPDX-License-Identifier: MIT
//! The QEMU engine's static guest executables, which the engine `.so`'s QEMU main loads first (`atl_engine.create`'s `boot_exe`). Two entries, one chosen per build by cfg:
//! - boot (default; atl-android-paths `engine_boot`): `_start` hands the host the auxv QEMU built for it through BOOT; the host records that trap, attaches a CPU and starts the guest linker from it (atl-native-bridge engine/qemu `boot`); BOOT's answer in x0 is this process's exit status.
//! - cpuinfo (`--cfg atl_qb_cpuinfo`; atl-android-paths `engine_cpuinfo`): copies the guest's `/proc/cpuinfo` to fd 1 and exits 0 (1 on any syscall error), no trap. The engine answers that open with its own formatter (QEMU linux-user `open_cpuinfo`), so the output is exactly what guest code reads; atld runs it once (atl-engine-cpuinfo) and binds the file over the host readers' /proc/cpuinfo, as a device binds the bridge's `cpuinfo.<isa>.txt` (libnativebridge `MountCpuinfoForInstructionSet`).
//!
//! Built by xtask (qemu.rs `boot_exe`, `cpuinfo_exe`) as `--crate-type bin` for the guest triple; on any other target (the host lint build) the crate is empty.
//! Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a11 RET (a host call returns), 0x7a13 BOOT (x0 = auxv, x1 = return stub; x0 = exit status).
//! Break (build.rs `CFGS`, passed by xtask qemu.rs as `--cfg`): `atl_qb_skip_boot` drops the BOOT svc, so `_start` exits with the auxv pointer's low byte and the host never attaches (qemu-spike boot gate red).
#![no_std]
#![cfg_attr(any(target_arch = "aarch64", target_arch = "arm"), no_main)]

// Assembly, not Rust: the entry point has no Rust ABI (argc/argv/envp on the stack, no return address) and BOOT is an `svc` with a custom immediate.
// Initial stack (fs/binfmt_elf.c create_elf_tables; QEMU's loader writes the same layout): [sp] = argc, then argv[argc] + NULL, envp + NULL, then the auxv.
// The return stub: where a host call returns (LR points here); the hook leaves the innermost cpu_loop.
// `$boot`: the BOOT trap line, or nothing under the break.
#[cfg(all(target_arch = "aarch64", not(atl_qb_cpuinfo)))]
macro_rules! start {
    ($($boot:literal)?) => {
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
            $($boot,)?
            // exit_group(x0) (asm-generic unistd.h __NR_exit_group = 94)
            "    mov x8, #94",
            "    svc #0",
            "2:  svc #0x7a11",
            "    b 2b",
        );
    };
}

#[cfg(all(target_arch = "aarch64", not(atl_qb_cpuinfo), not(atl_qb_skip_boot)))]
start!("    svc #0x7a13");
#[cfg(all(target_arch = "aarch64", not(atl_qb_cpuinfo), atl_qb_skip_boot))]
start!();

// The cpuinfo entry. asm-generic unistd.h: openat 56, read 63, write 64, exit_group 94; AT_FDCWD = -100, O_RDONLY = 0. x19 = fd, buffer = 4 KiB on the stack; a short write loops on the rest.
#[cfg(all(target_arch = "aarch64", atl_qb_cpuinfo))]
core::arch::global_asm!(
    ".globl _start",
    ".type _start, %function",
    "_start:",
    "    mov x0, #-100",
    "    adr x1, 9f",
    "    mov x2, #0",
    "    mov x8, #56",
    "    svc #0",
    "    tbnz x0, #63, 8f",
    "    mov x19, x0",
    "    sub sp, sp, #4096",
    // read a chunk; 0 = end
    "1:  mov x0, x19",
    "    mov x1, sp",
    "    mov x2, #4096",
    "    mov x8, #63",
    "    svc #0",
    "    tbnz x0, #63, 8f",
    "    cbz x0, 7f",
    "    mov x20, sp",
    "    mov x21, x0",
    // write x21 bytes from x20
    "2:  mov x0, #1",
    "    mov x1, x20",
    "    mov x2, x21",
    "    mov x8, #64",
    "    svc #0",
    "    tbnz x0, #63, 8f",
    "    cbz x0, 8f",
    "    add x20, x20, x0",
    "    subs x21, x21, x0",
    "    b.ne 2b",
    "    b 1b",
    "7:  mov x0, #0",
    "    b 6f",
    "8:  mov x0, #1",
    "6:  mov x8, #94",
    "    svc #0",
    "9:  .asciz \"/proc/cpuinfo\"",
);

// arm (armv7, phase 4 P1), Arm mode: the same two entries. Immediates are the arm trap numbers (trap.rs: the low byte, 0x11 RET, 0x13 BOOT; r0 = auxv, r1 = return stub). EABI syscall numbers (arch/arm/tools/syscall.tbl): exit_group 248, openat 322, read 3, write 4, taken from r7.
#[cfg(all(target_arch = "arm", not(atl_qb_cpuinfo)))]
macro_rules! start_arm {
    ($($boot:literal)?) => {
        core::arch::global_asm!(
            ".arm",
            ".globl _start",
            ".type _start, %function",
            "_start:",
            "    ldr r2, [sp]",
            "    add r3, sp, #4",
            // skip argv and its NULL
            "    add r3, r3, r2, lsl #2",
            "    add r3, r3, #4",
            // skip envp through its NULL: r3 = auxv
            "1:  ldr r4, [r3], #4",
            "    cmp r4, #0",
            "    bne 1b",
            "    mov r0, r3",
            "    adr r1, 2f",
            $($boot,)?
            "    mov r7, #248",
            "    svc #0",
            "2:  svc #0x11",
            "    b 2b",
        );
    };
}

#[cfg(all(target_arch = "arm", not(atl_qb_cpuinfo), not(atl_qb_skip_boot)))]
start_arm!("    svc #0x13");
#[cfg(all(target_arch = "arm", not(atl_qb_cpuinfo), atl_qb_skip_boot))]
start_arm!();

// The arm cpuinfo entry, as aarch64's: r5 = fd, buffer = 4 KiB on the stack; a short write loops on the rest. AT_FDCWD = -100 = `mvn #99`.
#[cfg(all(target_arch = "arm", atl_qb_cpuinfo))]
core::arch::global_asm!(
    ".arm",
    ".globl _start",
    ".type _start, %function",
    "_start:",
    "    mvn r0, #99",
    "    adr r1, 9f",
    "    mov r2, #0",
    "    ldr r7, =322",
    "    svc #0",
    "    cmp r0, #0",
    "    blt 8f",
    "    mov r5, r0",
    "    sub sp, sp, #4096",
    // read a chunk; 0 = end
    "1:  mov r0, r5",
    "    mov r1, sp",
    "    mov r2, #4096",
    "    mov r7, #3",
    "    svc #0",
    "    cmp r0, #0",
    "    blt 8f",
    "    beq 7f",
    "    mov r6, sp",
    "    mov r8, r0",
    // write r8 bytes from r6
    "2:  mov r0, #1",
    "    mov r1, r6",
    "    mov r2, r8",
    "    mov r7, #4",
    "    svc #0",
    "    cmp r0, #0",
    "    ble 8f",
    "    add r6, r6, r0",
    "    subs r8, r8, r0",
    "    bne 2b",
    "    b 1b",
    "7:  mov r0, #0",
    "    b 6f",
    "8:  mov r0, #1",
    "6:  mov r7, #248",
    "    svc #0",
    ".ltorg",
    "9:  .asciz \"/proc/cpuinfo\"",
);

/// Required by a `no_std` executable; nothing here can panic (no Rust code runs).
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
