/* SPDX-License-Identifier: MIT */
/*
 * The engine C ABI. The engine `.so` (libatl_engine_<arch>.so) exports one table, atl_engine, through its version script (atl_bridgegen::engine_abi::version_script), plus the TLS names the engine reads with dlsym (thread_cpu, helper_retaddr). The Rust mirror is crates/atl-engine-api/src/lib.rs; atl-qemu-engine's build script checks every layout and value here through clang.
 * Host: refuses an engine whose major differs from its own, or whose size is below atl_engine's size as the host knows it, or whose minor or entries are older than the host's (host and engine are built together, so the host requires what it calls). Minor versions only append fields.
 * One engine per process: create once. The booted engine lives until the process ends (QEMU's main never returns), so destroy has nothing to free.
 * Threads: create runs on the thread that loaded the engine (-EINVAL elsewhere): QEMU's constructor registered that thread with its RCU, and create moves the registration to the engine's boot thread. map and destroy run on any thread. attach_ex gives the calling thread its CPU; call and detach take that CPU and must run on the thread that attached it.
 * Errors: 0 or -errno: -EINVAL (bad argument or state), -EEXIST (second create), -EIO (boot or mapping failed). A call on a bad engine or CPU aborts the process.
 */
#ifndef ATL_ENGINE_H
#define ATL_ENGINE_H

#include <stdbool.h>
#include <stdint.h>

#define ATL_ENGINE_ABI_MAJOR 4
#define ATL_ENGINE_ABI_MINOR 0

/* Lanes per bank of atl_lanes: the widest guest convention's registers (arm r0..r15 / aarch64 x0..x15 cover every argument register), and a stack window of 16 guest stack slots. */
#define ATL_LANE_REGS 16
#define ATL_LANE_FP 16
#define ATL_LANE_STACK 16

/* Page protection, QEMU's page flags (include/exec/page-protection.h). */
#define ATL_PROT_READ 0x1u
#define ATL_PROT_WRITE 0x2u
#define ATL_PROT_EXEC 0x4u

enum atl_trap_kind {
    /* regs[0] = auxv (first argument register), regs[1] = return stub (second); the answer is the boot executable's exit status. */
    ATL_TRAP_BOOT = 0,
    /* A host call (svc #CALL): trap_ex, lanes in; the answer is lanes->ret and lanes->fp[0]. */
    ATL_TRAP_CALL = 1,
    /* A binding thunk: id = its slot; trap_ex, lanes in; the answer is lanes->ret and lanes->fp[0]. */
    ATL_TRAP_BIND = 2,
    /* A value reported for a gate: regs[0] = the first argument register; the answer is ignored. */
    ATL_TRAP_REPORT = 3,
};

typedef struct atl_engine_handle atl_engine_handle; /* opaque, one per process */
typedef struct atl_cpu atl_cpu;       /* opaque, one per attached host thread */

/* The lanes of a call or trap, guest-neutral: each bank holds one convention's slots in order, zero-extended to 64 bits, unused lanes 0. Which lanes a guest uses is its calling convention's, not the ABI's:
 * regs: the integer argument registers (aarch64 AAPCS64 x0..x7; arm AAPCS32 r0..r3).
 * fp: the low 64 bits of the fp argument registers (aarch64 v0..v7; arm softfp none: fp values travel in regs and stack); out, fp[0] is the fp result (aarch64 v0).
 * stack: the stack window, the argument slots at the callee's sp, one guest stack slot per lane (aarch64 8 bytes, arm 4 bytes); a slot past the guest's mapped memory reads 0. aarch64 reads and writes stack[0..8], arm stack[0..16].
 * ret: out, the integer result registers (aarch64 x0, x1; arm r0, r1: a 64-bit or softfp double result is the pair). The answer of a call or trap is ret and fp[0]. */
typedef struct atl_lanes {
    uint64_t regs[ATL_LANE_REGS];
    uint64_t fp[ATL_LANE_FP];
    uint64_t stack[ATL_LANE_STACK];
    uint64_t ret[2];
} atl_lanes;

/* The guest stack of an attach_ex, bionic's per-thread layout: a PROT_NONE guard of `guard` bytes right below a stack of `size` bytes; sp starts at base + len. In: size and guard, each rounded up to whole pages; 0 = the default (size 1 MiB, guard one page). Out: base and len, the usable stack (what bionic's pthread_getattr_np reports). */
typedef struct atl_stack {
    uint64_t size;
    uint64_t guard;
    uint64_t base;
    uint64_t len;
} atl_stack;

typedef struct atl_host {
    uint32_t major, minor, size;
    void *ctx;
    /* BOOT and REPORT. Called on the guest's thread, inside the trap; may call back into the engine (create published the handle before BOOT). */
    uint64_t (*trap)(void *ctx, uint32_t kind, uint32_t id, const uint64_t regs[8]);
    /* CALL and BIND, the same contract with lanes; the answer is lanes->ret and lanes->fp[0]. */
    void (*trap_ex)(void *ctx, uint32_t kind, uint32_t id, atl_lanes *lanes);
    /* 3.1: a guest fault the engine is about to hand to its guest delivery; info / ucontext are the kernel's siginfo_t * / ucontext_t *. Signal context, every signal blocked, possibly on a guest thread pointer: async-signal-safe, no TLS, must return. */
    void (*fault_report)(void *ctx, int sig, void *info, void *ucontext);
} atl_host;

typedef struct atl_engine {
    uint32_t major, minor, size;
    /* boot_exe: NUL-terminated path of a static guest executable; the engine runs its own QEMU main on a helper thread with its own command line (the host passes no QEMU flags). *out is written before QEMU runs; returns after BOOT. */
    int (*create)(const atl_host *host, const char *boot_exe, atl_engine_handle **out);
    void (*destroy)(atl_engine_handle *engine);
    /* Marks [start, start + len) as guest-mapped with prot (identity addresses: guest = host). */
    int (*map)(atl_engine_handle *engine, uint64_t start, uint64_t len, uint32_t prot);
    /* Gives the calling thread its guest CPU (a copy of the boot CPU, as clone makes one) on a sized guest stack: stack in (size, guard; 0 = default) and out (base, len). */
    int (*attach_ex)(atl_engine_handle *engine, atl_stack *stack, atl_cpu **out);
    void (*detach)(atl_engine_handle *engine, atl_cpu *cpu);
    /* Runs guest f(lanes) on cpu; *tp is the thread pointer in and out. lanes in and out: regs, fp and stack are the callee's arguments (stack at the callee's sp, below the interrupted sp); the answer is lanes->ret and lanes->fp[0]. */
    void (*call_ex)(atl_engine_handle *engine, atl_cpu *cpu, uint64_t *tp, uint64_t f, atl_lanes *lanes);
    /* The engine's part of a host SIGSEGV / SIGBUS handler, for a special handler in front of the process's user action (ART sigchain via NativeBridge getSignalHandler). info is the kernel's siginfo_t *, ucontext its ucontext_t *. True: the engine took the signal (a guest fault, delivered to the guest; or the translator's own fatal exit). False: not the engine's, pass it on. Signal context, on the thread that took the signal, with every signal blocked; may not return (it longjmps into the guest CPU loop), so the caller registers it as no-return (sigchain SIGCHAIN_ALLOW_NORETURN). False before create has booted. */
    bool (*fault)(int sig, void *info, void *ucontext);
    /* From inside a CALL or BIND trap callback, on the trapping thread: when the innermost trap of this thread returns, the guest call it interrupted (the innermost call_ex on this thread's CPU) ends at once with the trap's answer as its answer, instead of resuming the trapping guest code. For a thread-exit call that never returns to its guest caller (bionic pthread_exit). 0, or -EINVAL when no trap of this thread is running. */
    int (*end_call)(atl_engine_handle *engine);
} atl_engine;

#endif /* ATL_ENGINE_H */
