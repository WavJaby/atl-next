// SPDX-License-Identifier: MIT
// QEMU's own answer, for engine/aarch64.rs: can this build's load/store helpers do an aligned 16-byte access atomically, without `cpu_loop_exit_atomic`?
// RW, not RO: on RO 0 + RW 1 (x86 without AVX, arm64 without LSE2) the helper loads with a 16-byte compare-exchange and does not exit; RO 1 implies RW 1 on every host.
#include "qemu/osdep.h"
#include "qemu/atomic128.h"

// the unit's flags carry -Werror=missing-prototypes
bool atl_host_atomic128_rw(void);

bool atl_host_atomic128_rw(void)
{
    return HAVE_ATOMIC128_RW;
}
