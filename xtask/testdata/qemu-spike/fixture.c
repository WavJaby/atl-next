// SPDX-License-Identifier: MIT
// Guest side of the QEMU engine spike (xtask `test qemu-spike`, QEMU bridge plan Q2): a static bionic executable QEMU's own main loads and runs.
// main hands the host the gate table through the BOOT trap; the host then calls the fx_* functions through the engine. Every expected value is in the table below, from this source, never from the engine.
// Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a10 CALL (x0 = host answer), 0x7a11 RET (a host call returns), 0x7a12 REPORT (x0 = value), 0x7a13 BOOT (x0 = table, x1 = return stub; x0 = exit status).
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <ucontext.h>

typedef uint64_t u64;

// Where a host call returns: LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".globl fx_ret_stub\n"
    ".type fx_ret_stub,%function\n"
    "fx_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b fx_ret_stub\n");
void fx_ret_stub(void);

// (a) trap arguments: x0..x7 = 1..8 at the CALL; the guest returns the answer + 1.
#define A_FIRST 1
#define A_ADD 1
__attribute__((noinline)) u64 fx_trap_args(void) {
    register u64 x0 __asm__("x0") = A_FIRST + 0;
    register u64 x1 __asm__("x1") = A_FIRST + 1;
    register u64 x2 __asm__("x2") = A_FIRST + 2;
    register u64 x3 __asm__("x3") = A_FIRST + 3;
    register u64 x4 __asm__("x4") = A_FIRST + 4;
    register u64 x5 __asm__("x5") = A_FIRST + 5;
    register u64 x6 __asm__("x6") = A_FIRST + 6;
    register u64 x7 __asm__("x7") = A_FIRST + 7;
    __asm__ volatile("svc #0x7a10"
                     : "+r"(x0)
                     : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x6), "r"(x7)
                     : "memory");
    return x0 + A_ADD;
}

// (b) nested: fx_outer(n) sums CALL(i) for i = 1..n; the host answers CALL(i) by calling fx_inner(i) in the guest: sum of squares.
__attribute__((noinline)) u64 fx_inner(u64 i) {
    return i * i;
}

__attribute__((noinline)) static u64 call_host(u64 i) {
    register u64 x0 __asm__("x0") = i;
    __asm__ volatile("svc #0x7a10" : "+r"(x0) : : "memory");
    return x0;
}

__attribute__((noinline)) u64 fx_outer(u64 n) {
    u64 acc = 0;
    for (u64 i = 1; i <= n; i++) {
        acc += call_host(i);
    }
    return acc;
}

// (c) host memory: the host passes a raw pointer to its own box; plain and exclusive (ldaxr/stlxr) read-modify-writes.
__attribute__((noinline)) void fx_box(u64 *p) {
    p[0] = p[1] + 1;
    __atomic_fetch_add(&p[2], 1, __ATOMIC_SEQ_CST);
}

// (d) guest store fault: the guest's own SIGSEGV handler reports D_MARK and skips the faulting store.
#define D_MARK 42
#define D_RET 7
static void segv_handler(int sig, siginfo_t *info, void *ucv) {
    (void)sig;
    (void)info;
    register u64 x0 __asm__("x0") = D_MARK;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
    ((ucontext_t *)ucv)->uc_mcontext.pc += 4;
}

__attribute__((noinline)) u64 fx_store_fault(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = segv_handler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSEGV, &sa, 0) != 0) {
        return 0;
    }
    *(volatile u64 *)0 = 1;
    return D_RET;
}

// The gate table, read by the host (crates/atl-qemu-engine/tests/spike.rs `Table`): u64 words, in this order.
static const u64 table[] = {
    // 0..3: entry points
    (u64)fx_trap_args, (u64)fx_outer, (u64)fx_box, (u64)fx_store_fault,
    // 4: fx_inner, the host's nested callee
    (u64)fx_inner,
    // 5..6: (a) first argument value, return increment
    A_FIRST, A_ADD,
    // 7..8: (b) n and the sum of squares 1..n (1+4+9+16+25)
    5, 55,
    // 9..12: (c) box before; 13..16: box after
    0, 41, 7, 0,
    42, 41, 8, 0,
    // 17..18: (d) handler mark, fx_store_fault's return
    D_MARK, D_RET,
};

int main(void) {
    register u64 x0 __asm__("x0") = (u64)table;
    register u64 x1 __asm__("x1") = (u64)fx_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
