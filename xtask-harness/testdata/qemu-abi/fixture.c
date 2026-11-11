// SPDX-License-Identifier: MIT
// Guest side of the Q6-1 ABI gate (xtask `test qemu-abi`): a static executable. main hands the host a table of three entries through BOOT; the host calls them.
//  qa_run:  one host call (svc #CALL) with x0..x7, v0..v7 and two stack words set; returns the bits of the v0 the host answered.
//  qa_sum:  host->guest callee with 9 int and 9 double arguments (the 9th of each on the stack).
//  qa_edge: sets sp to the host's address and makes one host call; its stack words run past the page the host maps.
// The host checks what it receives against the literals in tests/qemu_abi.rs (which repeat them).
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

// Host->guest callee: the ints are the digits 1..9 (x0..x7, then the ninth, which is stack word 0), the doubles d_i weighted 2^i (d0..d7, then d8 = stack word 1).
// Returns digits + sum d_i * 2^i: exact in binary64 for the host's values, so the host compares bits.
double qa_sum(int64_t i0, int64_t i1, int64_t i2, int64_t i3, int64_t i4, int64_t i5, int64_t i6,
              int64_t i7, int64_t i8, double d0, double d1, double d2, double d3, double d4,
              double d5, double d6, double d7, double d8) {
    int64_t digits = i0;
    digits = digits * 10 + i1;
    digits = digits * 10 + i2;
    digits = digits * 10 + i3;
    digits = digits * 10 + i4;
    digits = digits * 10 + i5;
    digits = digits * 10 + i6;
    digits = digits * 10 + i7;
    digits = digits * 10 + i8;
    double w = d0 + 2 * d1 + 4 * d2 + 8 * d3 + 16 * d4 + 32 * d5 + 64 * d6 + 128 * d7 + 256 * d8;
    return (double)digits + w;
}

// Stack-bound callee: sp becomes x0 (16-aligned, the host's choice), s0 and s1 are stored at sp, then one host call. The host reads eight words from sp; the last six lie past the mapped page.
uint64_t qa_edge(uint64_t top) {
    uint64_t s0 = 0x0f0e0d0c0b0a0908ULL;
    uint64_t s1 = 0x1716151413121110ULL;
    __asm__ volatile(
        "mov x9, sp\n"
        "mov sp, %[top]\n"
        "str %[s0], [sp, #0]\n"
        "str %[s1], [sp, #8]\n"
        "svc #0x7a10\n"
        "mov sp, x9\n"
        :
        : [top] "r"(top), [s0] "r"(s0), [s1] "r"(s1)
        : "x9", "x0", "v0", "memory");
    return 0;
}

// The entries BOOT carries (x0 = this table): qa_run, qa_sum, qa_edge, in that order.
static void *const qa_table[3] = {(void *)qa_run, (void *)qa_sum, (void *)qa_edge};

// BOOT: x0 = the table, x1 = the return stub. The host's answer is this process's exit status; the boot thread never resumes after BOOT.
int main(void) {
    register uint64_t x0 __asm__("x0") = (uint64_t)(uintptr_t)qa_table;
    register uint64_t x1 __asm__("x1") = (uint64_t)qa_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
