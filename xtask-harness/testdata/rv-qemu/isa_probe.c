// SPDX-License-Identifier: MIT
// rv-qemu model probe (rv_qemu.rs): what a riscv64 process sees of its CPU, through the two probes a tier may use (AT_HWCAP letters, riscv_hwprobe IMA_EXT_0 names), then one V instruction.
// Lines: `hwcap <letters>`, `hwprobe <names>`, flushed before `vlenb <n>`; on a core without V the vsetvli raises SIGILL after the first two lines (the red control: the model matters).
// Compiled at the host row's -march: the V instructions alone are enabled, by `.option arch` around them.
#include <asm/hwprobe.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <unistd.h>

#define EXT(name, bit) {name, bit}

// every IMA_EXT_0 bit the sysroot's uapi header names, by the extension's own name
static const struct {
  const char *name;
  uint64_t bit;
} EXTS[] = {
    EXT("fd", RISCV_HWPROBE_IMA_FD),        EXT("c", RISCV_HWPROBE_IMA_C),
    EXT("v", RISCV_HWPROBE_IMA_V),          EXT("zba", RISCV_HWPROBE_EXT_ZBA),
    EXT("zbb", RISCV_HWPROBE_EXT_ZBB),      EXT("zbs", RISCV_HWPROBE_EXT_ZBS),
    EXT("zicboz", RISCV_HWPROBE_EXT_ZICBOZ), EXT("zbc", RISCV_HWPROBE_EXT_ZBC),
    EXT("zbkb", RISCV_HWPROBE_EXT_ZBKB),    EXT("zbkc", RISCV_HWPROBE_EXT_ZBKC),
    EXT("zbkx", RISCV_HWPROBE_EXT_ZBKX),    EXT("zknd", RISCV_HWPROBE_EXT_ZKND),
    EXT("zkne", RISCV_HWPROBE_EXT_ZKNE),    EXT("zknh", RISCV_HWPROBE_EXT_ZKNH),
    EXT("zksed", RISCV_HWPROBE_EXT_ZKSED),  EXT("zksh", RISCV_HWPROBE_EXT_ZKSH),
    EXT("zkt", RISCV_HWPROBE_EXT_ZKT),      EXT("zvbb", RISCV_HWPROBE_EXT_ZVBB),
    EXT("zvbc", RISCV_HWPROBE_EXT_ZVBC),    EXT("zvkb", RISCV_HWPROBE_EXT_ZVKB),
    EXT("zvkg", RISCV_HWPROBE_EXT_ZVKG),    EXT("zvkned", RISCV_HWPROBE_EXT_ZVKNED),
    EXT("zvknha", RISCV_HWPROBE_EXT_ZVKNHA), EXT("zvknhb", RISCV_HWPROBE_EXT_ZVKNHB),
    EXT("zvksed", RISCV_HWPROBE_EXT_ZVKSED), EXT("zvksh", RISCV_HWPROBE_EXT_ZVKSH),
    EXT("zvkt", RISCV_HWPROBE_EXT_ZVKT),    EXT("zfh", RISCV_HWPROBE_EXT_ZFH),
    EXT("zfhmin", RISCV_HWPROBE_EXT_ZFHMIN), EXT("zihintntl", RISCV_HWPROBE_EXT_ZIHINTNTL),
    EXT("zvfh", RISCV_HWPROBE_EXT_ZVFH),    EXT("zvfhmin", RISCV_HWPROBE_EXT_ZVFHMIN),
    EXT("zfa", RISCV_HWPROBE_EXT_ZFA),      EXT("ztso", RISCV_HWPROBE_EXT_ZTSO),
    EXT("zacas", RISCV_HWPROBE_EXT_ZACAS),  EXT("zicond", RISCV_HWPROBE_EXT_ZICOND),
    EXT("zihintpause", RISCV_HWPROBE_EXT_ZIHINTPAUSE), EXT("zve32x", RISCV_HWPROBE_EXT_ZVE32X),
    EXT("zve32f", RISCV_HWPROBE_EXT_ZVE32F), EXT("zve64x", RISCV_HWPROBE_EXT_ZVE64X),
    EXT("zve64f", RISCV_HWPROBE_EXT_ZVE64F), EXT("zve64d", RISCV_HWPROBE_EXT_ZVE64D),
    EXT("zimop", RISCV_HWPROBE_EXT_ZIMOP),  EXT("zca", RISCV_HWPROBE_EXT_ZCA),
    EXT("zcb", RISCV_HWPROBE_EXT_ZCB),      EXT("zcd", RISCV_HWPROBE_EXT_ZCD),
    EXT("zcf", RISCV_HWPROBE_EXT_ZCF),      EXT("zcmop", RISCV_HWPROBE_EXT_ZCMOP),
    EXT("zawrs", RISCV_HWPROBE_EXT_ZAWRS),
};

int main(void) {
  unsigned long hwcap = getauxval(AT_HWCAP);
  printf("hwcap ");
  // Linux riscv: bit (letter - 'a') per single-letter extension
  for (int i = 0; i < 26; i++)
    if (hwcap & (1UL << i)) putchar('a' + i);
  putchar('\n');

  struct riscv_hwprobe pair = {.key = RISCV_HWPROBE_KEY_IMA_EXT_0};
  // raw syscall: glibc's __riscv_hwprobe wrapper is newer than the glibc floor
  long rc = syscall(__NR_riscv_hwprobe, &pair, 1, 0, NULL, 0);
  if (rc != 0 || pair.key < 0) {
    printf("hwprobe unavailable rc=%ld\n", rc);
  } else {
    printf("hwprobe");
    for (size_t i = 0; i < sizeof EXTS / sizeof EXTS[0]; i++)
      if (pair.value & EXTS[i].bit) printf(" %s", EXTS[i].name);
    putchar('\n');
  }
  fflush(stdout);

  unsigned long vlenb;
  __asm__ volatile(
      ".option push\n.option arch, +v\n"
      "vsetvli t0, zero, e8, m1, ta, ma\n"
      "csrr %0, vlenb\n"
      ".option pop\n"
      : "=r"(vlenb)
      :
      : "t0");
  printf("vlenb %lu\n", vlenb);
  return 0;
}
