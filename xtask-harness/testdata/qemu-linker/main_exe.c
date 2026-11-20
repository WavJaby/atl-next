// SPDX-License-Identifier: MIT
// Main executable of the guest-main gate (xtask `test qemu-linker`, harness qemu_main): a PIE on the guest's stub libc. The host calls fx_run, which ends the way bionic's __libc_init ends a process: exit(main(...)).
// Guest exit runs the guest's __cxa_finalize, then NBS's native_bridge_exit stub, whose host side is the host's exit: the harness reads the status there.
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>

// The status main returns when the thread round trip held: the harness expects exactly this (qemu_main.rs MAIN_STATUS).
#define MAIN_STATUS 0x2a
// The worker's argument and what it answers (arg + 1): pthread_join must hand that back through the guest's void**.
#define WORKER_ARG 0x40
// A stack size the guest's bionic attr carries to the engine (intercepts.rs stack_req): 256 KiB, not bionic's default.
#define WORKER_STACK (256 * 1024)

static void *worker(void *arg) {
    return (void *)((uintptr_t)arg + 1);
}

// What a C++ `thread_local` with a non-trivial destructor compiles to: libc++abi's __cxa_thread_atexit forwards to bionic's __cxa_thread_atexit_impl (NBS guest libc -> native_bridge___cxa_thread_atexit_impl stub); called directly here, the fixture links no C++ runtime.
extern int __cxa_thread_atexit_impl(void (*dtor)(void *), void *obj, void *dso);
extern void *__dso_handle;

// Thread-exit events, one letter per destructor run, in run order; the harness reads the final string at the host's exit (qemu_main.rs `EVENTS`).
__attribute__((used)) char fx_events[16];
static unsigned n_events;

static void record(void *letter) {
    if (n_events + 1 < sizeof fx_events) fx_events[n_events++] = (char)(uintptr_t)letter;
}

static pthread_key_t key;

// Registers thread-exit destructors 'A' then 'B', a key value whose destructor records 'K' and a cleanup handler 'C', then pthread_exit inside the handler's scope: bionic pthread_exit.cpp runs __cxa_thread_finalize (reverse registration: B, A), then the cleanup handlers (C), then key destructors (K).
static void *tls_worker(void *arg) {
    (void)arg;
    if (__cxa_thread_atexit_impl(record, (void *)'A', &__dso_handle) != 0) return (void *)1;
    if (__cxa_thread_atexit_impl(record, (void *)'B', &__dso_handle) != 0) return (void *)1;
    pthread_setspecific(key, (void *)'K');
    pthread_cleanup_push(record, (void *)'C');
    pthread_exit(0);
    pthread_cleanup_pop(0);
    return (void *)1;
}

// A plain return after a handler popped with execute: 'D' runs at the pop, so the stack is empty at the return (POSIX: a return inside push/pop scope is undefined; bionic's start routine return goes through pthread_exit, which would run what is left). Then the thread-exit destructor 'E' and key 'K'.
static void *return_worker(void *arg) {
    (void)arg;
    if (__cxa_thread_atexit_impl(record, (void *)'E', &__dso_handle) != 0) return (void *)1;
    pthread_setspecific(key, (void *)'K');
    pthread_cleanup_push(record, (void *)'D');
    pthread_cleanup_pop(1);
    return 0;
}

static pthread_key_t late_key;

// A key destructor that records its value, then registers a thread-exit destructor 'L': bionic pthread_exit.cpp ran __cxa_thread_finalize before pthread_key_clean_all, so 'L' is never run (leaked) and the registration answers 0. The engine must not abort here (its thread-exit list is already destroyed at this point).
static void late_dtor(void *value) {
    record(value);
    __cxa_thread_atexit_impl(record, (void *)'L', &__dso_handle);
}

// Thread-exit destructor 'F', then a late_key value 'G': 'F' at exit, then 'G' from the key destructor, never 'L'.
static void *late_worker(void *arg) {
    (void)arg;
    if (__cxa_thread_atexit_impl(record, (void *)'F', &__dso_handle) != 0) return (void *)1;
    pthread_setspecific(late_key, (void *)'G');
    return 0;
}

// detached_worker's verdict: 0 pending, 1 its pthread_getattr_np reported PTHREAD_CREATE_DETACHED, 2 anything else.
static int detached_seen;

// bionic pthread_getattr_np ORs PTHREAD_ATTR_FLAG_DETACHED from the thread's join state.
static void *detached_worker(void *arg) {
    (void)arg;
    pthread_attr_t attr;
    int state = -1;
    int rc = pthread_getattr_np(pthread_self(), &attr);
    if (rc == 0) {
        pthread_attr_getdetachstate(&attr, &state);
        pthread_attr_destroy(&attr);
    }
    __atomic_store_n(&detached_seen, rc == 0 && state == PTHREAD_CREATE_DETACHED ? 1 : 2, __ATOMIC_RELEASE);
    return 0;
}

// 0 when a thread created detached reads itself as detached; else 10.
static int detached_state(void) {
    pthread_attr_t attr;
    pthread_t t;
    if (pthread_attr_init(&attr) != 0 || pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED) != 0) return 10;
    if (pthread_create(&t, &attr, detached_worker, 0) != 0) return 10;
    pthread_attr_destroy(&attr);
    while (__atomic_load_n(&detached_seen, __ATOMIC_ACQUIRE) == 0) sched_yield();
    return detached_seen == 1 ? 0 : 10;
}

// 0 when the workers' exit work ran exactly once each in bionic's order ("BACK", "DEK", then "FG" with 'L' leaked), and the main thread's two are registered; else the step (6..9, 11).
static int thread_dtors(void) {
    pthread_t t;
    void *answer = (void *)1;
    if (pthread_key_create(&key, record) != 0) return 6;
    if (pthread_create(&t, 0, tls_worker, 0) != 0 || pthread_join(t, &answer) != 0 || answer != 0) return 7;
    if (n_events != 4 || fx_events[0] != 'B' || fx_events[1] != 'A' || fx_events[2] != 'C' || fx_events[3] != 'K') return 8;
    answer = (void *)1;
    if (pthread_create(&t, 0, return_worker, 0) != 0 || pthread_join(t, &answer) != 0 || answer != 0) return 7;
    if (n_events != 7 || fx_events[4] != 'D' || fx_events[5] != 'E' || fx_events[6] != 'K') return 9;
    if (pthread_key_create(&late_key, late_dtor) != 0) return 6;
    if (pthread_create(&t, 0, late_worker, 0) != 0 || pthread_join(t, 0) != 0) return 7;
    if (n_events != 9 || fx_events[7] != 'F' || fx_events[8] != 'G') return 11;
    // the main thread's: run at exit after the guest's __cxa_finalize (NBS exit.c leaves __cxa_thread_finalize to the host's exit), reverse order: 'n' then 'm'
    if (__cxa_thread_atexit_impl(record, (void *)'m', &__dso_handle) != 0) return 6;
    if (__cxa_thread_atexit_impl(record, (void *)'n', &__dso_handle) != 0) return 6;
    return 0;
}

// 0 when a thread of an explicit attr ran and joined with its answer; else which step failed (1..5): main then returns that instead of MAIN_STATUS.
static int thread_round_trip(void) {
    pthread_attr_t attr;
    pthread_t t;
    void *answer = 0;
    if (pthread_attr_init(&attr) != 0) return 1;
    if (pthread_attr_setstacksize(&attr, WORKER_STACK) != 0) return 2;
    if (pthread_create(&t, &attr, worker, (void *)WORKER_ARG) != 0) return 3;
    pthread_attr_destroy(&attr);
    if (pthread_join(t, &answer) != 0) return 4;
    if ((uintptr_t)answer != WORKER_ARG + 1) return 5;
    return 0;
}

// The failure answers of fx_join_one: a flag above any worker answer the host uses, plus the call's errno.
#define CREATE_FAILED 0x100000
#define JOIN_FAILED 0x200000

// One default-attr thread created and joined: its answer (arg + 1), or CREATE_FAILED / JOIN_FAILED + the rc. The host calls it in a loop for the churn check (qemu_main.rs `c`).
__attribute__((used, noinline)) uintptr_t fx_join_one(uintptr_t arg) {
    pthread_t t;
    void *answer = 0;
    int rc = pthread_create(&t, 0, worker, (void *)arg);
    if (rc != 0) return CREATE_FAILED + rc;
    rc = pthread_join(t, &answer);
    if (rc != 0) return JOIN_FAILED + rc;
    return (uintptr_t)answer;
}

#include "../common/resolver.h"

// bionic getaddrinfo.c ai_errlist[EAI_NONAME]: the text gai_strerror must answer (a window copy on a 32-bit guest)
#define NONAME_TEXT "hostname nor servname provided, or not known"

// The resolver round (qemu_main.rs check n): ../common/resolver.h rs_resolve (bits 1..9) and rs_walk (10..12), then gai_strerror (bit 13). 0 when all held. The host calls it twice and reads the engine's window count around the second: every list and hostent copy given back leaves it unchanged.
__attribute__((used, noinline)) long fx_dns(void) {
    long bad = rs_resolve() | rs_walk();
    if (!rs_streq(gai_strerror(EAI_NONAME), NONAME_TEXT)) bad |= 1L << 13;
    return bad;
}

// Check (k): what a host call through a down shell delivers, stored for the host to read (qemu_main.rs `callbacks`). fx_cb's prototype makes AAPCS32 skip r1 (b takes the aligned pair r2:r3) and put p on the stack (AAPCS32 C.3, C.5); AAPCS64 takes x0, x1, x2. [0] a sign-extended, [1] b, [2] p, [3] fx_cbh's obj, [4] its data.
__attribute__((used)) uint64_t fx_seen[5];

__attribute__((used, noinline)) void fx_cb(int32_t a, int64_t b, void *p) {
    fx_seen[0] = (uint64_t)(int64_t)a;
    fx_seen[1] = (uint64_t)b;
    fx_seen[2] = (uintptr_t)p;
}

// A callback whose first argument is a host object (choreographer.h vsync callback shape): the guest sees a handle, never the host address.
__attribute__((used, noinline)) void fx_cbh(void *obj, void *data) {
    fx_seen[3] = (uintptr_t)obj;
    fx_seen[4] = (uintptr_t)data;
}

int main(void) {
    int failed = thread_round_trip();
    if (!failed) failed = thread_dtors();
    if (!failed) failed = detached_state();
    return failed ? failed : MAIN_STATUS;
}

// What __libc_init does after the start: exit with main's value. Never returns.
__attribute__((used, noinline)) void fx_run(void) {
    exit(main());
}
