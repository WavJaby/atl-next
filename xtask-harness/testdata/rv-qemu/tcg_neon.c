// SPDX-License-Identifier: MIT
// arm64 guest code for the engine's riscv64 code generator: NEON instructions whose translation ands / ors / xors a vector with a constant, against the same operation on scalar lanes. Static bionic, NDK-built: app code and the guest libc take the same paths (strrchr).
// One line per check, `<name> ok` or `<name> wrong`; exit 1 when any is wrong.
#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define ROUNDS 64
#define N_ELEM(a) (sizeof(a) / sizeof(a[0]))

static uint64_t state = 88172645463325252u; // xorshift64's published seed; any nonzero value
static uint64_t next(void) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    return state;
}

static int failed;
static void report(const char* name, int ok) {
    printf("%s %s\n", name, ok ? "ok" : "wrong");
    failed |= !ok;
}

// `vec` computes r from a and b (quadword registers); `scalar` is lane i of r from x = a[i], y = b[i]. Compiler barrier: asm, so clang cannot fold either side into the other.
#define CHECK(name, type, vec, scalar)                                                             \
    do {                                                                                           \
        int ok = 1;                                                                                \
        for (int k = 0; k < ROUNDS; k++) {                                                         \
            type a[16 / sizeof(type)], b[16 / sizeof(type)], r[16 / sizeof(type)];                 \
            for (size_t i = 0; i < N_ELEM(a); i++) a[i] = (type)next(), b[i] = (type)next();       \
            __asm__ volatile("ldr q0, [%0]\n ldr q1, [%1]\n mov v2.16b, v1.16b\n " vec             \
                             "\n str q2, [%2]"                                                     \
                             : : "r"(a), "r"(b), "r"(r) : "v0", "v1", "v2", "memory");             \
            for (size_t i = 0; i < N_ELEM(a); i++) {                                               \
                type x = a[i], y = b[i];                                                           \
                (void)x, (void)y;                                                                  \
                ok &= r[i] == (type)(scalar);                                                      \
            }                                                                                      \
        }                                                                                          \
        report(name, ok);                                                                          \
    } while (0)

// USHL: the shift is the low byte of y, signed; negative shifts right, a magnitude of the lane width or more gives 0.
static uint64_t ushl(uint64_t x, int8_t s, int bits) {
    if (s >= bits || s <= -bits) return 0;
    return s >= 0 ? x << s : x >> -s;
}

static void check_strrchr(void) {
    int ok = 1;
    char s[128];
    for (int k = 0; k < 2000; k++) {
        size_t at = next() % 16, n = next() % 100, last = SIZE_MAX;
        for (size_t i = 0; i < n; i++) {
            s[at + i] = next() % 8 ? 'a' : '/';
            if (s[at + i] == '/') last = i;
        }
        s[at + n] = 0;
        ok &= strrchr(s + at, '/') == (last == SIZE_MAX ? NULL : s + at + last);
    }
    report("strrchr", ok);
}

int main(void) {
    // or / and-not with a lane constant: 0x0001 per halfword is not a replicated byte
    CHECK("orr.8h", uint16_t, "orr v2.8h, #1", y | 1);
    CHECK("orr.4s", uint32_t, "orr v2.4s, #1", y | 1);
    CHECK("bic.8h", uint16_t, "bic v2.8h, #1, lsl #8", y & ~0x100);
    CHECK("bic.4s", uint32_t, "bic v2.4s, #255, lsl #8", y & ~0xff00u);
    CHECK("orr-255.8h", uint16_t, "orr v2.8h, #255", y | 255);
    // expansions that mask with a lane constant
    CHECK("urshr.8h", uint16_t, "urshr v2.8h, v0.8h, #1", (x >> 1) + (x & 1));
    CHECK("srshr.4s", int32_t, "srshr v2.4s, v0.4s, #1", (x >> 1) + (x & 1));
    CHECK("ursra.16b", uint8_t, "ursra v2.16b, v0.16b, #1", y + (uint8_t)((x >> 1) + (x & 1)));
    CHECK("sli.4s", uint32_t, "sli v2.4s, v0.4s, #16", (x << 16) | (y & 0xffff));
    CHECK("sri.2d", uint64_t, "sri v2.2d, v0.2d, #32", (x >> 32) | (y & 0xffffffff00000000u));
    CHECK("ushl.8h", uint16_t, "ushl v2.8h, v0.8h, v1.8h", ushl(x, (int8_t)y, 16));
    CHECK("ushl.4s", uint32_t, "ushl v2.4s, v0.4s, v1.4s", ushl(x, (int8_t)y, 32));
    check_strrchr();
    return failed;
}
