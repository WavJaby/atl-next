// SPDX-License-Identifier: MIT
// The dlopen'd guest library of the Q4-5 gate (xtask `test qemu-linker`, harness qemu_dlopen). Built `-shared -fPIC -nostdlib -Wl,-soname,libfix.so`, linked with libdep.so: DT_NEEDED.
// The constructor runs after libdep's (DT_NEEDED order) and reads libdep's value. fix_value() = dep_value() + 0x10 = 0x51b2 when libdep is relocated.
#include <stdint.h>

uint64_t dep_value(void);

static uint64_t fix_ctor;

__attribute__((constructor)) static void fix_init(void) {
    fix_ctor = dep_value();
}

uint64_t fix_ctor_value(void) {
    return fix_ctor;
}

uint64_t fix_value(void) {
    return dep_value() + 0x10;
}
