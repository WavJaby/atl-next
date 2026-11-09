// SPDX-License-Identifier: MIT
// Main executable of the Q4-5 dlopen gate (xtask `test qemu-linker`, harness qemu_dlopen, QEMU bridge plan Q4-5): a PIE with DT_NEEDED on libc.so (the guest's stub libc, whose constructor intercepts its stubs) and libdl.so.
// The guest linker loads it as argv[1]; the host then calls these functions through the crossing (`Guest::call`), so this code's dlopen runs on the guest.
// `libfix.so` is found by name through LD_LIBRARY_PATH (the gate's library path).
#include <dlfcn.h>
#include <stdint.h>

__attribute__((used, noinline)) void *fx_open(void) {
    return dlopen("libfix.so", RTLD_NOW);
}

__attribute__((used, noinline)) void *fx_fix_value(void *h) {
    return dlsym(h, "fix_value");
}

__attribute__((used, noinline)) void *fx_fix_ctor_value(void *h) {
    return dlsym(h, "fix_ctor_value");
}

// The last dl error (NULL when none): the host prints it when dlopen or dlsym fails.
__attribute__((used, noinline)) const char *fx_dlerror(void) {
    return dlerror();
}

// Never run (the host calls the functions above, not _start): crtbegin_dynamic.o needs a main to link.
int main(void) {
    return 0;
}
