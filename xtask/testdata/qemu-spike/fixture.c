// SPDX-License-Identifier: MIT
// Guest side of the QEMU engine spike (xtask `test qemu-spike`, QEMU bridge plan Q2/Q3): a static bionic executable QEMU's own main loads and runs.
// main hands the host the gate table through the BOOT trap; the host then calls the fx_* functions through the engine. Every expected value is in the table below, from this source, never from the engine.
// Trap immediates, as crates/atl-qemu-engine/src/trap.rs: 0x7a10 CALL (x0 = host answer), 0x7a11 RET (a host call returns), 0x7a12 REPORT (x0 = value), 0x7a13 BOOT (x0 = table, x1 = return stub; never returns: the host parks the boot thread), 0x7a14 THREAD (a guest thread's first instruction); 0x8000|k a binding thunk (pool slot k, gate l).
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
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

// (d) guest store fault: the guest's own SIGSEGV handler reports D_MARK and skips the faulting instruction.
#define D_MARK 42
#define D_RET 7
static void segv_handler(int sig, siginfo_t *info, void *ucv) {
    (void)sig;
    (void)info;
    register u64 x0 __asm__("x0") = D_MARK;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
    ((ucontext_t *)ucv)->uc_mcontext.pc += 4;
}

static int install_segv_handler(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = segv_handler;
    sa.sa_flags = SA_SIGINFO;
    return sigaction(SIGSEGV, &sa, 0);
}

__attribute__((noinline)) u64 fx_store_fault(void) {
    if (install_segv_handler() != 0) {
        return 0;
    }
    *(volatile u64 *)0 = 1;
    return D_RET;
}

// (f) translated code: a writable, executable page the guest runs and the host rewrites. The host's write meets QEMU's write protection for translated code; QEMU un-protects the page, and the guest runs the rewritten code.
#define F_MOVZ_X0_1 0xD2800020u // movz x0, #1
#define F_MOVZ_X0_2 0xD2800040u // movz x0, #2: the host's rewrite
#define F_RET 0xD65F03C0u       // ret
#define F_OLD 1
#define F_NEW 2
__attribute__((noinline)) u64 fx_smc_setup(void) {
    void *m = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) {
        return 0;
    }
    uint32_t *p = m;
    p[0] = F_MOVZ_X0_1;
    p[1] = F_RET;
    return (u64)(uintptr_t)p;
}

__attribute__((noinline)) u64 fx_smc_run(u64 page) {
    return ((u64(*)(void))(uintptr_t)page)();
}

// (g) a second guest thread: its first instruction registers it with the host (THREAD). Its atomic on address 0 is a helper access (QEMU's helper_retaddr); the guest's SIGSEGV handler reports D_MARK on that thread.
#define G_RET 9
__attribute__((noinline)) static void fault_in_helper(void) {
    register u64 x0 __asm__("x0") = 0;
    register u64 x1 __asm__("x1") = 0;
    register u64 x2 __asm__("x2") = 1;
    __asm__ volatile(".arch_extension lse\n\tcasal x1, x2, [x0]" : "+r"(x1) : "r"(x0), "r"(x2) : "memory");
}

static void *thread_body(void *arg) {
    register u64 x0 __asm__("x0") = 0;
    (void)arg;
    __asm__ volatile("svc #0x7a14" : "+r"(x0) : : "memory");
    fault_in_helper();
    return 0;
}

__attribute__((noinline)) u64 fx_thread_fault(void) {
    pthread_t t;
    if (install_segv_handler() != 0 || pthread_create(&t, 0, thread_body, 0) != 0) {
        return 0;
    }
    pthread_join(t, 0);
    return G_RET;
}

// (i) fatal, one thread: the guest restores SIG_DFL, so a store to address 0 has no guest handler. QEMU ends the process with the signal (die_with_signal): the host sees death by SIGSEGV.
// (j) the same fault on a second guest thread while the main guest thread waits in pthread_join.
static int default_segv(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_DFL;
    return sigaction(SIGSEGV, &sa, 0);
}

__attribute__((noinline)) u64 fx_fatal(void) {
    if (default_segv() != 0) {
        return 0;
    }
    *(volatile u64 *)0 = 1;
    return 0;
}

static void *fatal_thread_body(void *arg) {
    register u64 x0 __asm__("x0") = 0;
    (void)arg;
    __asm__ volatile("svc #0x7a14" : "+r"(x0) : : "memory");
    *(volatile u64 *)0 = 1;
    return 0;
}

__attribute__((noinline)) u64 fx_fatal_thread(void) {
    pthread_t t;
    if (default_segv() != 0 || pthread_create(&t, 0, fatal_thread_body, 0) != 0) {
        return 0;
    }
    pthread_join(t, 0);
    return 0;
}

// (l) binding thunks: atl_bind_pool is the guest pool xtask emits (atl-bridgegen `aarch64` pool_asm, assembled into this fixture), slot k = `svc #(0x8000|k); ret`.
// fx_bind(k) calls slot k with x0..x7 = A_FIRST..A_FIRST+7; the host answers (spike.rs BIND_ANSWER), and the result + BIND_ADD goes through REPORT and is returned.
// The call sets x8 to getpid(2): a run where the svc misses the hook (break `bind-decode`) then makes a harmless syscall instead of one with a stray x8.
#define BIND_ADD 1
#define BIND_GETPID 172
extern const char atl_bind_pool[];
__attribute__((noinline)) static u64 call_thunk(const char *entry) {
    register u64 x0 __asm__("x0") = A_FIRST + 0;
    register u64 x1 __asm__("x1") = A_FIRST + 1;
    register u64 x2 __asm__("x2") = A_FIRST + 2;
    register u64 x3 __asm__("x3") = A_FIRST + 3;
    register u64 x4 __asm__("x4") = A_FIRST + 4;
    register u64 x5 __asm__("x5") = A_FIRST + 5;
    register u64 x6 __asm__("x6") = A_FIRST + 6;
    register u64 x7 __asm__("x7") = A_FIRST + 7;
    register u64 nr __asm__("x8") = BIND_GETPID;
    register const char *fn __asm__("x16") = entry;
    __asm__ volatile("blr %[fn]"
                     : [fn] "+r"(fn), "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x3), "+r"(x4), "+r"(x5), "+r"(x6), "+r"(x7), "+r"(nr)
                     :
                     : "x9", "x10", "x11", "x12", "x13", "x14", "x15", "x17", "x30", "memory", "cc");
    return x0;
}

__attribute__((noinline)) u64 fx_bind(u64 slot) {
    u64 r = call_thunk(atl_bind_pool + 8 * slot) + BIND_ADD;
    register u64 x0 __asm__("x0") = r;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
    return r;
}

// (m), (n), (o): host-attached CPUs. The host calls these on its own threads, each on its own guest CPU (atl-qemu-engine `Booted::attach`), after BOOT parked the boot thread.
// fx_sum(n): 0 + 1 + ... + (n - 1) through a volatile stack slot, long enough that two host threads overlap; the table holds n(n-1)/2.
// fx_tp(): the thread pointer the call runs with (TPIDR_EL0).
// fx_write_msg(fd): raw write(2) of the static message (fixture data, mapped for the guest), so QEMU's lock_user runs (it reads thread_cpu); returns the raw result (count or -errno).
// fx_write(fd, buf, n): the same on a host buffer (o): glibc heap the guest never mapped, which access_ok accepts under identity mapping.
#define M_N1 100000ull
#define M_N2 200000ull
#define NR_WRITE 64
__attribute__((noinline)) u64 fx_sum(u64 n) {
    volatile u64 acc = 0;
    for (u64 i = 0; i < n; i++) {
        acc += i;
    }
    return acc;
}

__attribute__((noinline)) u64 fx_tp(void) {
    u64 tp;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(tp));
    return tp;
}

__attribute__((noinline)) static u64 raw_write(u64 fd, const void *buf, u64 n) {
    register u64 x0 __asm__("x0") = fd;
    register const void *x1 __asm__("x1") = buf;
    register u64 x2 __asm__("x2") = n;
    register u64 x8 __asm__("x8") = NR_WRITE;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
    return x0;
}

static const char msg[] = "q5-2\n";
#define MSG_LEN (sizeof msg - 1)
__attribute__((noinline)) u64 fx_write_msg(u64 fd) {
    return raw_write(fd, msg, MSG_LEN);
}

__attribute__((noinline)) u64 fx_write(u64 fd, u64 buf, u64 n) {
    return raw_write(fd, (const void *)(uintptr_t)buf, n);
}

// (s) per-CPU guest stacks: fx_stack_fill(pattern) fills S_WORDS stack words with pattern + i, hands the host the buffer address (CALL, x0 = buf), which holds every thread at one barrier, then counts words that no longer read pattern + i. Own stacks -> 0; one shared stack -> the last filler wins and the others count S_WORDS.
// S_WORDS = 64 KiB of u64, far above any frame noise and far below the 1 MiB stack.
#define S_WORDS 8192
__attribute__((noinline)) u64 fx_stack_fill(u64 pattern) {
    volatile u64 buf[S_WORDS];
    for (u64 i = 0; i < S_WORDS; i++) {
        buf[i] = pattern + i;
    }
    call_host((u64)(uintptr_t)buf);
    u64 bad = 0;
    for (u64 i = 0; i < S_WORDS; i++) {
        bad += buf[i] != pattern + i;
    }
    return bad;
}

// (t) the stack guard: fx_guard_store(addr) stores to addr (the host passes stack base - 8, inside the guard page). The handler reports si_addr (REPORT), restores SIG_DFL and returns, so the store re-runs with no handler and QEMU ends the process by SIGSEGV. Without a guard the store succeeds and the call returns T_SURVIVED.
#define T_SURVIVED 11
static void guard_handler(int sig, siginfo_t *info, void *ucv) {
    (void)sig;
    (void)ucv;
    register u64 x0 __asm__("x0") = (u64)(uintptr_t)info->si_addr;
    __asm__ volatile("svc #0x7a12" : : "r"(x0) : "memory");
    default_segv();
}

__attribute__((noinline)) u64 fx_guard_store(u64 addr) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = guard_handler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSEGV, &sa, 0) != 0) {
        return 0;
    }
    *(volatile u64 *)(uintptr_t)addr = 1;
    return T_SURVIVED;
}

// The gate table, read by the host (crates/atl-qemu-engine/tests/spike.rs `t`): u64 words, in this order.
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
    // 19..23: (f) setup, run, first result, the host's rewrite, second result
    (u64)fx_smc_setup, (u64)fx_smc_run, F_OLD, F_MOVZ_X0_2, F_NEW,
    // 24..25: (g) thread entry, its return
    (u64)fx_thread_fault, G_RET,
    // 26..27: (i) fatal fault on the main guest thread, (j) on a second one
    (u64)fx_fatal, (u64)fx_fatal_thread,
    // 28..29: (l) fx_bind, and its report: the host's answer (0x1234) + BIND_ADD
    (u64)fx_bind, 0x1234 + BIND_ADD,
    // 30..34: (m) fx_sum, two n and their sums n(n-1)/2
    (u64)fx_sum, M_N1, M_N1 * (M_N1 - 1) / 2, M_N2, M_N2 * (M_N2 - 1) / 2,
    // 35..38: (m) fx_tp, fx_write_msg, the message and its length
    (u64)fx_tp, (u64)fx_write_msg, (u64)msg, MSG_LEN,
    // 39: (o) fx_write, a write(2) on a host buffer the guest never mapped
    (u64)fx_write,
    // 40..41: (s) fx_stack_fill and its word count
    (u64)fx_stack_fill, S_WORDS,
    // 42..43: (t) fx_guard_store and its return when the store did not fault
    (u64)fx_guard_store, T_SURVIVED,
};

int main(void) {
    // the guest's SIGSEGV handler from startup, as an app installs it; gates (i) and (j) remove it and take QEMU's fatal path
    install_segv_handler();
    register u64 x0 __asm__("x0") = (u64)table;
    register u64 x1 __asm__("x1") = (u64)fx_ret_stub;
    __asm__ volatile("svc #0x7a13" : "+r"(x0) : "r"(x1) : "memory");
    return (int)x0;
}
