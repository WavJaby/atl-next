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

int main(void) {
    int failed = thread_round_trip();
    return failed ? failed : MAIN_STATUS;
}

// What __libc_init does after the start: exit with main's value. Never returns.
__attribute__((used, noinline)) void fx_run(void) {
    exit(main());
}
