// SPDX-License-Identifier: MIT
// Guest side of the Q6-1 ABI gate, trap side (xtask `test qemu-abi`): a static executable. main hands the host qa_run's address through BOOT; the host calls qa_run, which makes one host call (svc #CALL) with x0..x7, v0..v7 and two stack words set, and returns the bits of v0 that the host answered.
// The host checks the lanes it receives against the literals in tests/qemu_abi.rs (which repeat them): x0..x7 = 0x101..0x108, v0..v7 = 1.5..8.5, stack words 0x0123456789abcdef and 0xfedcba9876543210. The answer is 3.25, which is no input lane.
#include <stdint.h>

// Where a host call returns: LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".globl qa_ret_stub\n"
    ".type qa_ret_stub,%function\n"
    "qa_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b qa_ret_stub\n");
void qa_ret_stub(void);

// One host call with every lane set. The two stack words sit at sp, where a callee's stack arguments are; the host reads eight words, the last six being whatever lies above them.
uint64_t qa_run(void) {
    register uint64_t x0 __asm__("x0") = 0x101;
    register uint64_t x1 __asm__("x1") = 0x102;
    register uint64_t x2 __asm__("x2") = 0x103;
    register uint64_t x3 __asm__("x3") = 0x104;
    register uint64_t x4 __asm__("x4") = 0x105;
    register uint64_t x5 __asm__("x5") = 0x106;
    register uint64_t x6 __asm__("x6") = 0x107;
    register uint64_t x7 __asm__("x7") = 0x108;
    register double d0 __asm__("d0") = 1.5;
    register double d1 __asm__("d1") = 2.5;
    register double d2 __asm__("d2") = 3.5;
    register double d3 __asm__("d3") = 4.5;
    register double d4 __asm__("d4") = 5.5;
    register double d5 __asm__("d5") = 6.5;
    register double d6 __asm__("d6") = 7.5;
    register double d7 __asm__("d7") = 8.5;
    uint64_t s0 = 0x0123456789abcdefULL;
    uint64_t s1 = 0xfedcba9876543210ULL;
    __asm__ volatile(
        "sub sp, sp, #16\n"
        "str %[s0], [sp, #0]\n"
        "str %[s1], [sp, #8]\n"
        "svc #0x7a10\n"
        "add sp, sp, #16\n"
        : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3), "+r"(x4), "+r"(x5), "+r"(x6), "+r"(x7),
          "+w"(d0), "+w"(d1), "+w"(d2), "+w"(d3), "+w"(d4), "+w"(d5), "+w"(d6), "+w"(d7)
        : [s0] "r"(s0), [s1] "r"(s1)
        : "memory", "cc");
    union {
        double d;
        uint64_t u;
    } answer = {.d = d0};
    return answer.u;
}

// BOOT: x0 = qa_run (the host's entry), x1 = the return stub. The host's answer is this process's exit status; the boot thread never resumes after BOOT.
int main(void) {
    register uint64_t x0 __asm__("x0") = (uint64_t)qa_run;
    register uint64_t x1 __asm__("x1") = (uint64_t)qa_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
