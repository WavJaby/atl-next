// SPDX-License-Identifier: MIT
// Guest side of the arm32 engine spike (xtask `test qemu-spike --arch arm`, phase 4 P1): a static bionic armv7 executable QEMU's own main loads and runs.
// main hands the host the gate table through the BOOT trap; the host then calls the fx_* functions through the engine. Every expected value is in the table below, from this source, never from the engine.
// Trap immediates, as crates/atl-qemu-engine/src/trap.rs for arm (they fit a Thumb svc's 8 bits): 0x10 CALL (r0 = host answer), 0x11 RET (a host call returns), 0x12 REPORT (r0 = value), 0x13 BOOT (r0 = table, r1 = return stub; never returns: the host parks the boot thread).
// The NDK compiles armv7 code as Thumb by default; the functions whose mode matters say so (target attribute), and the table holds each address as the linker gives it (bit 0 set = Thumb).
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>

typedef uint32_t u32;

// Where a host call returns, in Arm mode (bit 0 clear): LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".pushsection .text\n"
    ".arm\n"
    ".globl fx_ret_stub\n"
    ".type fx_ret_stub,%function\n"
    "fx_ret_stub:\n"
    "    svc #0x11\n"
    "    b fx_ret_stub\n"
    ".popsection\n");
void fx_ret_stub(void);

// (a) trap arguments: r0..r3 = 1..4 and four stack words 5..8 at sp at the CALL, from a Thumb and an Arm caller; the guest returns the answer + A_ADD.
#define A_FIRST 1
#define A_ADD 1
#define TRAP_ARGS_BODY                                                                               \
    register u32 r0 __asm__("r0") = A_FIRST + 0;                                                     \
    register u32 r1 __asm__("r1") = A_FIRST + 1;                                                     \
    register u32 r2 __asm__("r2") = A_FIRST + 2;                                                     \
    register u32 r3 __asm__("r3") = A_FIRST + 3;                                                     \
    u32 s0 = A_FIRST + 4, s1 = A_FIRST + 5, s2 = A_FIRST + 6, s3 = A_FIRST + 7;                      \
    __asm__ volatile("sub sp, sp, #16\n\t"                                                           \
                     "str %[s0], [sp]\n\t"                                                           \
                     "str %[s1], [sp, #4]\n\t"                                                       \
                     "str %[s2], [sp, #8]\n\t"                                                       \
                     "str %[s3], [sp, #12]\n\t"                                                      \
                     "svc #0x10\n\t"                                                                 \
                     "add sp, sp, #16"                                                               \
                     : "+r"(r0)                                                                      \
                     : "r"(r1), "r"(r2), "r"(r3), [s0] "r"(s0), [s1] "r"(s1), [s2] "r"(s2),          \
                       [s3] "r"(s3)                                                                  \
                     : "memory");                                                                    \
    return r0 + A_ADD;

__attribute__((noinline, target("thumb"))) u32 fx_trap_args_thumb(void) {
    TRAP_ARGS_BODY
}

__attribute__((noinline, target("arm"))) u32 fx_trap_args_arm(void) {
    TRAP_ARGS_BODY
}

// (b) nested: fx_outer(n) sums CALL(i) for i = 1..n; the host answers CALL(i) by calling fx_inner(i) in the guest: sum of squares.
__attribute__((noinline)) u32 fx_inner(u32 i) {
    return i * i;
}

__attribute__((noinline)) static u32 call_host(u32 i) {
    register u32 r0 __asm__("r0") = i;
    __asm__ volatile("svc #0x10" : "+r"(r0) : : "memory");
    return r0;
}

__attribute__((noinline)) u32 fx_outer(u32 n) {
    u32 acc = 0;
    for (u32 i = 1; i <= n; i++) {
        acc += call_host(i);
    }
    return acc;
}

// (c) host memory below 4 GiB: the host passes a raw pointer to its own box; a plain and an exclusive (ldrex/strex) read-modify-write.
__attribute__((noinline)) void fx_box(u32 *p) {
    p[0] = p[1] + 1;
    __atomic_fetch_add(&p[2], 1, __ATOMIC_SEQ_CST);
}

// (d) guest store fault: a store below mmap_min_addr (never mapped); the guest's own SIGSEGV handler reports D_MARK and skips the faulting Arm-mode store (4 bytes).
#define D_MARK 42
#define D_RET 7
#define D_ADDR 0x1000u
static void segv_handler(int sig, siginfo_t *info, void *ucv) {
    (void)sig;
    (void)info;
    ucontext_t *uc = ucv;
    uc->uc_mcontext.arm_pc += 4;
    register u32 r0 __asm__("r0") = D_MARK;
    __asm__ volatile("svc #0x12" : : "r"(r0) : "memory");
}

__attribute__((noinline, target("arm"))) u32 fx_store_fault(void) {
    *(volatile u32 *)D_ADDR = 1;
    return D_RET;
}

// (e) guest mmap: E_LEN bytes at `hint` (0 = anywhere), not MAP_FIXED; returns the address or MAP_FAILED.
#define E_LEN (16u << 20)
__attribute__((noinline)) u32 fx_mmap(u32 hint) {
    void *p = mmap((void *)hint, E_LEN, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p != MAP_FAILED) {
        // touch it: the pages are the guest's own
        *(volatile u32 *)p = 1;
    }
    return (u32)(uintptr_t)p;
}

// (f) a call on a host-attached CPU before its detach and the fork: fx_inner(F_ARG).
#define F_ARG 3

// The gate table, read by the host (crates/atl-qemu-engine/tests/spike_arm.rs `t`): u32 words, in this order.
static const u32 table[] = {
    // 0..6: entry points
    (u32)fx_trap_args_thumb, (u32)fx_trap_args_arm, (u32)fx_outer, (u32)fx_inner, (u32)fx_box,
    (u32)fx_store_fault, (u32)fx_mmap,
    // 7..8: (a) first argument value, return increment
    A_FIRST, A_ADD,
    // 9..10: (b) n and the sum of squares 1..n (1+4+9+16+25)
    5, 55,
    // 11..14: (c) box before; 15..18: box after
    0, 41, 7, 0,
    42, 41, 8, 0,
    // 19..20: (d) handler mark, fx_store_fault's return
    D_MARK, D_RET,
    // 21: (e) mapping length
    E_LEN,
    // 22..23: (f) fx_inner's argument and result
    F_ARG, F_ARG * F_ARG,
};

// BOOT from Arm mode: a decode that reads only Arm immediates (the `arm-only-decode` break) still boots, so gate (a)'s Thumb caller is what turns red.
__attribute__((noinline, target("arm"))) static u32 boot(void) {
    register u32 r0 __asm__("r0") = (u32)table;
    register u32 r1 __asm__("r1") = (u32)fx_ret_stub;
    __asm__ volatile("svc #0x13" : "+r"(r0) : "r"(r1) : "memory");
    return r0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = segv_handler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSEGV, &sa, 0) != 0) {
        return 1;
    }
    return (int)boot();
}
