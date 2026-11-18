// SPDX-License-Identifier: MIT
// Guest program of the QEMU gate (xtask-harness/src/qemu_gate.rs): static bionic, built by the NDK clang.
// The gate expects exactly the line printed here; the sum is 1..=100 = 5050.
// The arch word is the compiler's (QEMU's target name), so a binary built for the wrong arch fails the line.
#include <stdio.h>

#if defined(__aarch64__)
#define ARCH "aarch64"
#elif defined(__arm__)
#define ARCH "arm"
#else
#error "hello.c: no QEMU target name for this arch"
#endif

int main(void) {
    long sum = 0;
    for (int i = 1; i <= 100; i++) {
        sum += i;
    }
    printf("hello from static " ARCH ": sum=%ld\n", sum);
    return 0;
}
