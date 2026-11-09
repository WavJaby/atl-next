// SPDX-License-Identifier: MIT
// DT_NEEDED target of libfix.so (xtask `test qemu-linker`, harness qemu_dlopen, QEMU bridge plan Q4-5). Built `-shared -fPIC -nostdlib -Wl,-soname,libdep.so`.
// Its relocation class the guest linker must apply is R_AARCH64_RELATIVE: `self_addr` holds the address of `table`, written at load time. The NDK's lld packs that relocation into DT_ANDROID_RELR (`llvm-readelf -d`), which the guest linker must unpack.
// Break `skip_reloc` zeroes DT_ANDROID_RELRSZ (xtask qemu_linker.rs), so `self_addr` stays unrelocated and dep_value() returns 0.
// Values the host checks: 0x51a2 = table[1]. Derived from these literals, never from the engine.
#include <stdint.h>

static const uint64_t table[2] = {0x51a1, 0x51a2};

// External, non-const: the compiler cannot fold the load, so the initializer stays a RELATIVE relocation.
uintptr_t self_addr = (uintptr_t)&table[0];

uint64_t dep_value(void) {
    return self_addr == (uintptr_t)&table[0] ? table[1] : 0;
}
