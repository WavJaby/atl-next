// SPDX-License-Identifier: MIT
// Guest side of the Q4-3 placement gate (xtask `test qemu-linker`, QEMU bridge plan Q4-3): a static bionic executable that QEMU's main loads.
// main hands the host `run_sites` through BOOT; the host calls it with x0 = a page holding the six placed NB sites. Each site is called as its AOSP prototype says (vdso.h, __libc_init_main_thread.cpp, static_tls_config.h); the answers go back as REPORT traps, in order: R0 the stub address, R1 get_host_pthread, R2 the stub's answer, R3 call_guest's answer.
// Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a11 RET (a host call returns), 0x7a12 REPORT (x0 = value), 0x7a13 BOOT (x0 = run_sites, x1 = return stub; x0 = exit status).
// Every literal below is what the host checks (xtask/testdata/qemu-linker is the only copy the gate reads).
#include <stdint.h>

typedef uint64_t u64;

// Where a host call returns: LR points here; the hook leaves the innermost cpu_loop.
__asm__(
    ".globl fx_ret_stub\n"
    ".type fx_ret_stub,%function\n"
    "fx_ret_stub:\n"
    "    svc #0x7a11\n"
    "    b fx_ret_stub\n");
void fx_ret_stub(void);

// The intercept stub: the NB placeholder (`ldr x3, =0; blr x3`, assembled `mov x3, #0; blr x3`), 8 bytes. The host rewrites it once it reports the address (R0).
__asm__(
    ".globl fx_stub\n"
    ".type fx_stub,%function\n"
    ".p2align 3\n"
    "fx_stub:\n"
    "    .inst 0xd2800003\n"
    "    .inst 0xd63f0060\n");
u64 fx_stub(void);

// The AOSP struct NativeBridgeStaticTlsConfig (static_tls_config.h:25-37): size, tpoff, init_img, two slot ids.
struct tls_config {
    u64 size;
    u64 tpoff;
    const void *init_img;
    int tls_slot_thread_id;
    int tls_slot_bionic_tls;
};
static const char tls_init_img[64] = {0};

#define TRACE_FMT "qemu-linker trace %d %d\n"
#define TRACE_A 7
#define TRACE_B 9
#define LIB_NAME "gate.library"
#define SYM_NAME "gate.symbol"
#define CALL_GUEST_A 0xc0de0001u
#define CALL_GUEST_B 0xc0de0002u
#define CALL_GUEST_C 0xc0de0003u

__attribute__((noinline)) static void report(u64 v) {
    register u64 x0 __asm__("x0") = v;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
}

// The six sites in order (get_host_pthread, config_static_tls, trace, intercept_symbol, post_init, call_guest); each is a plain C call through the address the host passed.
// sites points at the host's page of the six addresses: the engine's call passes four arguments, so the host passes a pointer.
__attribute__((noinline)) u64 run_sites(const u64 *sites) {
    const u64 s0 = sites[0], s1 = sites[1], s2 = sites[2], s3 = sites[3], s4 = sites[4], s5 = sites[5];
    static const struct tls_config cfg = {0x1000, 0x40, tls_init_img, 1, 2};
    report((u64)fx_stub);                                                    // R0
    report(((u64 (*)(void))s0)());                                           // get_host_pthread(): pthread_t
    ((void (*)(const struct tls_config *))s1)(&cfg);                         // config_static_tls(config)
    ((void (*)(const char *, ...))s2)(TRACE_FMT, (u64)TRACE_A, (u64)TRACE_B); // trace(format, ...)
    ((void (*)(void *, const char *, const char *))s3)((void *)fx_stub, LIB_NAME, SYM_NAME); // intercept_symbol(addr, library, symbol)
    report(fx_stub());                                                       // R2: the stub's binding answer
    ((void (*)(void))s4)();                                                  // post_init()
    report(((u64 (*)(u64, u64, u64))s5)(CALL_GUEST_A, CALL_GUEST_B, CALL_GUEST_C)); // R3: call_guest(a, b, c)
    return 0;
}

int main(void) {
    register u64 x0 __asm__("x0") = (u64)run_sites;
    register u64 x1 __asm__("x1") = (u64)fx_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
