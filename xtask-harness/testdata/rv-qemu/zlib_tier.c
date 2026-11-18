// SPDX-License-Identifier: MIT
// rv-qemu zlib tier check (rv_qemu.rs): the converter-built libz, whose adler32_simd.c is the one V-tier TU (isa-tiers.toml), against an independent adler32 and a compress/uncompress round trip.
// zlib dispatches on getauxval(AT_HWCAP) bit 'v' (cpu_features.c); this prints the same bit, so the verdict names the tier the library took. Whether V code actually ran is measured outside (QEMU's translation log).
// Lines: `tier v=<0|1>`, `adler32 <n> sizes ok` or `adler32 len <n>: lib <x> ref <y>`, `roundtrip <n> sizes ok` or a mismatch line. Exit 0 only when every line is ok.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <zlib.h>

// RFC 1950 section 8.2: s1 = 1 + sum of bytes, s2 = sum of s1 after each byte, both mod 65521 (the largest prime below 2^16)
static uint32_t adler32_ref(const unsigned char *p, size_t n) {
  uint32_t s1 = 1, s2 = 0;
  for (size_t i = 0; i < n; i++) {
    s1 = (s1 + p[i]) % 65521;
    s2 = (s2 + s1) % 65521;
  }
  return (s2 << 16) | s1;
}

int main(void) {
  printf("tier v=%d\n", (getauxval(AT_HWCAP) & (1UL << ('v' - 'a'))) != 0);
  // lengths around adler32_simd's thresholds: below and at the 32-byte block, chunk tails, and past NMAX (5552) so the modulo folding runs mid-buffer
  static const size_t SIZES[] = {1, 31, 32, 33, 64, 255, 4096, 5552, 5553, 65536 + 7, 1 << 20};
  const size_t count = sizeof SIZES / sizeof SIZES[0];
  const size_t max = SIZES[count - 1];
  unsigned char *buf = malloc(max);
  // a fixed LCG (Numerical Recipes constants): deterministic, no long runs, every byte value
  uint32_t x = 12345;
  for (size_t i = 0; i < max; i++) {
    x = x * 1664525u + 1013904223u;
    buf[i] = (unsigned char)(x >> 24);
  }
  int bad = 0;
  for (size_t i = 0; i < count; i++) {
    uint32_t lib = (uint32_t)adler32(1, buf, (uInt)SIZES[i]);
    uint32_t ref = adler32_ref(buf, SIZES[i]);
    if (lib != ref) {
      printf("adler32 len %zu: lib %08x ref %08x\n", SIZES[i], lib, ref);
      bad = 1;
    }
  }
  if (!bad) printf("adler32 %zu sizes ok\n", count);

  int rt_bad = 0;
  unsigned char *out = malloc(max);
  for (size_t i = 0; i < count; i++) {
    uLongf zlen = compressBound(SIZES[i]);
    unsigned char *z = malloc(zlen);
    uLongf olen = SIZES[i];
    int c = compress2(z, &zlen, buf, SIZES[i], Z_DEFAULT_COMPRESSION);
    int u = c == Z_OK ? uncompress(out, &olen, z, zlen) : c;
    if (u != Z_OK || olen != SIZES[i] || memcmp(out, buf, SIZES[i]) != 0) {
      printf("roundtrip len %zu: compress %d uncompress %d out %lu\n", SIZES[i], c, u, (unsigned long)olen);
      rt_bad = 1;
    }
    free(z);
  }
  if (!rt_bad) printf("roundtrip %zu sizes ok\n", count);
  return bad || rt_bad;
}
