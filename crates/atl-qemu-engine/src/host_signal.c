// SPDX-License-Identifier: MIT
// QEMU's own host fault decode for signal.rs, exported: `host-signal.h` is the build's host dir (linux-user/include/host/<host>), static inline there.
// x86_64 reads REG_ERR's write bit; riscv64's sigcontext has no fault cause, so QEMU decodes the store at the pc -> one decode per host, QEMU's.
#include "qemu/osdep.h"
#include "host-signal.h"

// the unit's flags carry -Werror=missing-prototypes; signal.rs is the only caller
uintptr_t atl_host_signal_pc(host_sigcontext *uc);
bool atl_host_signal_write(siginfo_t *info, host_sigcontext *uc);

uintptr_t atl_host_signal_pc(host_sigcontext *uc)
{
    return host_signal_pc(uc);
}

bool atl_host_signal_write(siginfo_t *info, host_sigcontext *uc)
{
    return host_signal_write(info, uc);
}
