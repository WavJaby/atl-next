// SPDX-License-Identifier: MIT
// Main executable of the guest-main gate (xtask `test qemu-linker`, harness qemu_main): a PIE on the guest's stub libc. The host calls fx_run, which ends the way bionic's __libc_init ends a process: exit(main(...)).
// Guest exit runs the guest's __cxa_finalize, then NBS's native_bridge_exit stub, whose host side is the host's exit: the harness reads the status there.
#include <pthread.h>
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

// Registers thread-exit destructors 'A' then 'B' and a key value whose destructor records 'K': bionic's pthread_exit order is __cxa_thread_finalize (reverse registration: B, A), then key destructors (K).
static void *tls_worker(void *arg) {
    (void)arg;
    if (__cxa_thread_atexit_impl(record, (void *)'A', &__dso_handle) != 0) return (void *)1;
    if (__cxa_thread_atexit_impl(record, (void *)'B', &__dso_handle) != 0) return (void *)1;
    pthread_setspecific(key, (void *)'K');
    return 0;
}

// 0 when the worker's thread-exit destructors ran exactly once in bionic's order ("BAK"), and the main thread's two are registered; else the step (6..8).
static int thread_dtors(void) {
    pthread_t t;
    void *answer = (void *)1;
    if (pthread_key_create(&key, record) != 0) return 6;
    if (pthread_create(&t, 0, tls_worker, 0) != 0 || pthread_join(t, &answer) != 0 || answer != 0) return 7;
    if (n_events != 3 || fx_events[0] != 'B' || fx_events[1] != 'A' || fx_events[2] != 'K') return 8;
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

int main(void) {
    int failed = thread_round_trip();
    if (!failed) failed = thread_dtors();
    return failed ? failed : MAIN_STATUS;
}

// What __libc_init does after the start: exit with main's value. Never returns.
__attribute__((used, noinline)) void fx_run(void) {
    exit(main());
}
