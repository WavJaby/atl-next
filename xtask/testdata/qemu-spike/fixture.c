// SPDX-License-Identifier: MIT
// Freestanding guest code of the QEMU engine spike (xtask `test qemu-spike`, QEMU bridge plan Q2).
// Stand-in for guest code: no libc, no globals, raw syscalls through `svc #0`. Built with the NDK clang
// (-ffreestanding -nostdlib -fPIC -fno-plt -mno-outline-atomics) and flattened to a blob by xtask; the
// entry offsets come from the `fx_*` symbols. Expected values come from the literals below, never from the engine.
// Trap immediates, shared with crates/atl-qemu-engine/src/trap.rs: 0x7a10 CALL (host hook runs a callback),
// 0x7a11 RET (leave the innermost cpu_loop), 0x7a12 REPORT (x0 = value for the gate).
typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned char u8;

#define SA_SIGINFO 0x4ul
#define SA_RESTORER 0x04000000ul
#define SIGSEGV 11
#define SYS_RT_SIGACTION 134
#define SYS_RT_SIGRETURN 139
// ucontext: uc_mcontext at 176, sigcontext.pc at 8 + 31 * 8 + 8 = 264 within it.
#define UC_PC_OFFSET 440

// The return stub: a guest function returns here (LR is set by the engine); the host leaves cpu_loop.
// Restorer: rt_sigreturn for the guest signal frame. Both are plain asm: no prologue, no stack use.
__asm__(
    ".hidden fx_ret_stub\n"
    ".type fx_ret_stub,%function\n"
    "fx_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b fx_ret_stub\n"
    ".hidden fx_restorer\n"
    ".type fx_restorer,%function\n"
    "fx_restorer:\n"
    "    mov x8, #139\n"
    "    svc #0\n"
);

// Hidden, so taking its address is PC-relative (a default-visibility extern would go through the GOT, which the flat blob lacks).
void fx_restorer(void) __attribute__((visibility("hidden")));

static inline u64 sys_svc4(u64 nr, u64 a, u64 b, u64 c, u64 d) {
    register u64 x8 __asm__("x8") = nr;
    register u64 x0 __asm__("x0") = a;
    register u64 x1 __asm__("x1") = b;
    register u64 x2 __asm__("x2") = c;
    register u64 x3 __asm__("x3") = d;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}

// (a) trap arguments: the guest sets x0..x7 to 1..8, traps; the host hook returns R; the guest returns R + 1.
__attribute__((visibility("hidden"), noinline)) u64 fx_trap_args(void) {
    register u64 x0 __asm__("x0") = 1;
    register u64 x1 __asm__("x1") = 2;
    register u64 x2 __asm__("x2") = 3;
    register u64 x3 __asm__("x3") = 4;
    register u64 x4 __asm__("x4") = 5;
    register u64 x5 __asm__("x5") = 6;
    register u64 x6 __asm__("x6") = 7;
    register u64 x7 __asm__("x7") = 8;
    __asm__ volatile("svc #0x7a10"
                     : "+r"(x0)
                     : "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x6), "r"(x7)
                     : "memory");
    return x0 + 1;
}

// Host-called leaf: the nested callee of (b). Returns into fx_ret_stub.
__attribute__((visibility("hidden"), noinline)) u64 fx_inner(u64 i) {
    return i * i;
}

// (b) nested: each svc CALL runs fx_inner(i) on the host side, through the engine, and yields its result.
__attribute__((visibility("hidden"), noinline)) static u64 call_host(u64 i) {
    register u64 x0 __asm__("x0") = i;
    __asm__ volatile("svc #0x7a10" : "+r"(x0) : : "memory");
    return x0;
}

__attribute__((visibility("hidden"), noinline)) u64 fx_outer(u64 n) {
    u64 acc = 0;
    for (u64 i = 1; i <= n; i++) {
        acc += call_host(i);
    }
    return acc;
}

// (c) host Box: the host passes a raw pointer to {0, 41, 7, 0}; p[0] = p[1] + 1 and an exclusive-pair increment of p[2].
__attribute__((visibility("hidden"), noinline)) void fx_box(u64 *p) {
    p[0] = p[1] + 1;
    __atomic_fetch_add(&p[2], 1, __ATOMIC_SEQ_CST);
}

// (d) guest store fault: a SIGSEGV handler reports 42 via the REPORT svc and skips the faulting store.
__attribute__((visibility("hidden"), noinline)) static void segv_handler(u64 sig, u64 info, u64 uc) {
    register u64 x0 __asm__("x0") = 42;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
    *(u64 *)(uc + UC_PC_OFFSET) += 4;
}

__attribute__((visibility("hidden"), noinline)) u64 fx_store_fault(void) {
    // struct kernel_sigaction: handler, flags, restorer, mask (sigset_t, 8 bytes)
    u64 sa[4];
    sa[0] = (u64)segv_handler;
    sa[1] = SA_SIGINFO | SA_RESTORER;
    sa[2] = (u64)fx_restorer;
    sa[3] = 0;
    sys_svc4(SYS_RT_SIGACTION, SIGSEGV, (u64)sa, 0, 8);
    *(volatile u64 *)0 = 1;
    return 0;
}
