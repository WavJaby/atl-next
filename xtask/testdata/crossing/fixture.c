// SPDX-License-Identifier: MIT
/*
 * Q5-8 crossing counter fixture (crates/atl-native-bridge/tests/qemu_crossing.rs). A PIE on libc.so, built with -fno-builtin so each call stays a call.
 * cx_run makes CX_CALLS calls of each of four bindings: memcpy, memset, memcmp (counted shims under the feature) and time (glibc through the stub libc). Call i of each sized binding takes cx_sizes[i % CX_NSIZES].
 * The harness reads N from cx_calls and the sizes from cx_size_at, so these literals are its only source of expectations.
 * Exports return size_t: the harness reads x0 whole.
 */
#include <stddef.h>
#include <string.h>
#include <time.h>

#define CX_CALLS 64

/* Bit lengths 0, 1, 3, 4, 7, 13: one size per histogram bin class. */
static const size_t cx_sizes[] = {0, 1, 7, 8, 100, 4096};
#define CX_NSIZES (sizeof(cx_sizes) / sizeof(cx_sizes[0]))

static unsigned char cx_src[4096];
static unsigned char cx_dst[4096];

/* Volatile pointers keep each call a call. */
static void *(*volatile cx_memcpy)(void *, const void *, size_t) = memcpy;
static int (*volatile cx_memcmp)(const void *, const void *, size_t) = memcmp;
static void *(*volatile cx_memset)(void *, int, size_t) = memset;
static time_t (*volatile cx_time)(time_t *) = time;

/* A PIE links only with main; the harness never calls it. */
int main(void) { return 0; }

size_t cx_calls(void) { return CX_CALLS; }
size_t cx_size_count(void) { return CX_NSIZES; }
size_t cx_size_at(size_t i) { return cx_sizes[i]; }

/* Returns the number of wrong results: 0 when every shim did its work. */
size_t cx_run(void) {
    size_t bad = 0;
    for (size_t i = 0; i < (size_t)CX_CALLS; i++) {
        size_t n = cx_sizes[i % CX_NSIZES];
        for (size_t k = 0; k < n; k++) {
            cx_src[k] = (unsigned char)(k + i);
        }
        cx_memcpy(cx_dst, cx_src, n);
        for (size_t k = 0; k < n; k++) {
            if (cx_dst[k] != cx_src[k]) {
                bad++;
            }
        }
        if (cx_memcmp(cx_src, cx_dst, n) != 0) {
            bad++;
        }
        cx_memset(cx_dst, 0x5a, n);
        for (size_t k = 0; k < n; k++) {
            if (cx_dst[k] != 0x5a) {
                bad++;
            }
        }
        if (cx_time(NULL) <= 0) {
            bad++;
        }
    }
    return bad;
}
