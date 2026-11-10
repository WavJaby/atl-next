/* SPDX-License-Identifier: MIT */
/*
 * The engine C ABI. The engine `.so` (libatl_engine_<arch>.so) exports one table, atl_engine_v1, through its version script (atl_bridgegen::engine_abi::version_script), plus the TLS names the engine reads with dlsym (thread_cpu, helper_retaddr). The Rust mirror is crates/atl-engine-api/src/lib.rs; atl-qemu-engine's build script checks every layout and value here through clang.
 * Host: refuses an engine whose major differs from its own, or whose size is below atl_engine_v1's size as the host knows it. Minor versions only append fields.
 * One engine per process: create once. The booted engine lives until the process ends (QEMU's main never returns), so destroy has nothing to free.
 * Threads: create, map and destroy run on any thread. attach gives the calling thread its CPU; call and detach take that CPU and must run on the thread that attached it.
 * Errors: 0 or -errno: -EINVAL (bad argument or state), -EEXIST (second create), -EIO (boot or mapping failed). A call on a bad engine or CPU aborts the process.
 */
#ifndef ATL_ENGINE_H
#define ATL_ENGINE_H

#include <stdint.h>

#define ATL_ENGINE_ABI_MAJOR 1
#define ATL_ENGINE_ABI_MINOR 1

/* Page protection, QEMU's page flags (include/exec/page-protection.h). */
#define ATL_PROT_READ 0x1u
#define ATL_PROT_WRITE 0x2u
#define ATL_PROT_EXEC 0x4u

enum atl_trap_kind {
    /* regs[0] = auxv (x0), regs[1] = return stub (x1); the answer is the boot executable's exit status. */
    ATL_TRAP_BOOT = 0,
    /* A host call (svc #CALL): trap_ex, lanes in; the answer is lanes->x[0] and lanes->v[0]. */
    ATL_TRAP_CALL = 1,
    /* A binding thunk: id = its slot; trap_ex, lanes in; the answer is lanes->x[0] and lanes->v[0]. */
    ATL_TRAP_BIND = 2,
    /* A value reported for a gate: regs[0] = x0; the answer is ignored. */
    ATL_TRAP_REPORT = 3,
};

typedef struct atl_engine atl_engine; /* opaque, one per process */
typedef struct atl_cpu atl_cpu;       /* opaque, one per attached host thread */

/* Minor 1: the lanes of a host call or trap beyond the integer registers. x = x0..x7 and v = the low 64 bits of v0..v7 (in); stack = the eight words at the guest's sp (in; a word past the guest's mapped memory is 0). The answer is x[0] and v[0]. */
typedef struct atl_lanes_v1 {
    uint64_t x[8];
    uint64_t v[8];
    uint64_t stack[8];
} atl_lanes_v1;

typedef struct atl_host_v1 {
    uint32_t major, minor, size;
    void *ctx;
    /* BOOT and REPORT. Called on the guest's thread, inside the trap; may call back into the engine (create published the handle before BOOT). */
    uint64_t (*trap)(void *ctx, uint32_t kind, uint32_t id, const uint64_t regs[8]);
    /* Minor 1: CALL and BIND, the same contract with lanes; the answer is lanes->x[0] and lanes->v[0]. */
    void (*trap_ex)(void *ctx, uint32_t kind, uint32_t id, atl_lanes_v1 *lanes);
} atl_host_v1;

typedef struct atl_engine_v1 {
    uint32_t major, minor, size;
    /* boot_exe: NUL-terminated path of a static guest executable; the engine runs its own QEMU main on a helper thread with its own command line (the host passes no QEMU flags). *out is written before QEMU runs; returns after BOOT. */
    int (*create)(const atl_host_v1 *host, const char *boot_exe, atl_engine **out);
    void (*destroy)(atl_engine *engine);
    /* Marks [start, start + len) as guest-mapped with prot (identity addresses: guest = host). */
    int (*map)(atl_engine *engine, uint64_t start, uint64_t len, uint32_t prot);
    /* Gives the calling thread its guest CPU (a copy of the boot CPU, as clone makes one). */
    int (*attach)(atl_engine *engine, atl_cpu **out);
    void (*detach)(atl_engine *engine, atl_cpu *cpu);
    /* Minor 0's call (x0..x3 in, x0 out) is retired in minor 1: call_ex replaces it, and the engine leaves this slot NULL. The slot stays so call_ex keeps its append-only offset. */
    void *retired_call;
    /* Minor 1: runs guest f(lanes) on cpu; *tp is the thread pointer in and out. lanes in and out: x, v and stack are the callee's arguments (stack at the callee's sp, below the interrupted sp); the answer is lanes->x[0] and lanes->v[0]. */
    void (*call_ex)(atl_engine *engine, atl_cpu *cpu, uint64_t *tp, uint64_t f, atl_lanes_v1 *lanes);
} atl_engine_v1;

#endif /* ATL_ENGINE_H */
