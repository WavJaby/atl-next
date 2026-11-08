// SPDX-License-Identifier: MIT
// Guest program of the QEMU gate (xtask/src/qemu_gate.rs): static bionic, built by the NDK clang.
// The gate expects exactly the line printed here; the sum is 1..=100 = 5050.
#include <stdio.h>

int main(void) {
    long sum = 0;
    for (int i = 1; i <= 100; i++) {
        sum += i;
    }
    printf("hello from static aarch64: sum=%ld\n", sum);
    return 0;
}
