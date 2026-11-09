// SPDX-License-Identifier: MIT
// Boot executable of the Q4-4 start gate (xtask `test qemu-linker`, QEMU bridge plan Q4-4): a static bionic executable that QEMU's main loads. main hands the host the auxv QEMU built for it through BOOT; the host starts the guest linker from that trap (engine/arch/qemu.rs `run`) and answers with this process's exit status.
// Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a11 RET (a host call returns), 0x7a13 BOOT (x0 = auxv, x1 = return stub; x0 = exit status).
#include <stdint.h>

typedef uint64_t u64;

// Where a host call returns: LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".globl boot_ret_stub\n"
    ".type boot_ret_stub,%function\n"
    "boot_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b boot_ret_stub\n");
void boot_ret_stub(void);

int main(int argc, char **argv, char **envp) {
    (void)argc;
    (void)argv;
    // the auxv follows envp's NULL (fs/binfmt_elf.c create_elf_tables; QEMU's loader writes the same layout)
    char **p = envp;
    while (*p != 0) {
        p++;
    }
    register u64 x0 __asm__("x0") = (u64)(p + 1);
    register u64 x1 __asm__("x1") = (u64)boot_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
