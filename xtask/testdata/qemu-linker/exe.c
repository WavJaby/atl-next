// SPDX-License-Identifier: MIT
// Main executable of the Q4-4 start gate (xtask `test qemu-linker`, QEMU bridge plan Q4-4): the guest linker loads it as argv[1] and returns its entry; the engine never jumps there.
// PIE without libc (`-nostdlib -pie`): the bionic linker accepts only position-independent executables, and no DT_NEEDED keeps libc's constructors out of this gate (Q4-5).
// One TLS segment, so the linker's static-TLS report has an executable block to check: its initial words are the gate's literals, read back from the report's init_img. Aligned to 64: bionic reserves TLS slots 0..7 after the arm64 thread pointer and rejects an executable whose segment could overlap them (linker_main.cpp, "TLS segment is underaligned").
#include <stdint.h>

__attribute__((used, aligned(64))) __thread uint64_t exe_tls[2] = {0x51a1000000000001u, 0x51a1000000000002u};

// Exits with status 0 if ever run: exit_group (asm-generic/unistd.h: 94).
__attribute__((naked, noreturn)) void _start(void) {
    __asm__ volatile(
        "mov x0, #0\n"
        "mov x8, #94\n"
        "svc #0\n");
}
