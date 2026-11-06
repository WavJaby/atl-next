// SPDX-License-Identifier: MIT
//! The seccomp target arch of the host this crate is built for and its per-arch rows: seccompiler's arch (its audit value) and binderd's measured allow-list.
//! seccompiler 0.5 knows x86_64 and aarch64 only; riscv64 (a Tier 1 host later) needs seccompiler support or another backend before its build compiles.
//!
//! BINDERD_SYSCALLS derivation (x86_64, measured 2026-11-06, WSL2 kernel 6.6.87.2; strace is not installed, and RET_LOG lost records to kauditd's printk ratelimit: 1470 suppressed in one run):
//! kill-mode iteration -- binderd's filter = allow the known set, default KILL_PROCESS; each run dies at the first call outside it and the kernel's one audit record (`type=1326 ... syscall=N`, in dmesg) names it; add, rerun, until the workload passes.
//! Workloads: `xtask test binder` (converged after 20 runs), then `test app`, `test parcel`, `test window --stage ime` added nothing. socketpair is not listed here: binderd_program allows it for AF_UNIX only.
//! atl-sandbox-stub (2026-11-06, same kernel): bwrap installs this filter, then execs the stub, which applies Landlock and execs binderd -- so this one filter sees the stub's calls and binderd's; Landlock sees only file access after restrict_self (binderd's start-up opens).
//! Kill mode, `test binder`: run 1 died at 444 (landlock_create_ruleset); 445/446 added with it (the next two calls of the same sequence, landlock-0.4.7 compat.rs) and prctl (its PR_SET_NO_NEW_PRIVS before restrict_self, errors.rs); run 2 passed with no record. openat/close of the stub's PathFds were already listed.
//! Not measured, so killed: every path the gates do not reach, e.g. a panic's abort (rt_sigprocmask, tgkill) -- the panic message is written first, the abort ends as SIGSYS instead of SIGABRT.
//! Re-measure after binderd or its Rust/glibc start code changes (a new call ends every gate with "binderd ended"); the procedure needs nothing but the filter and dmesg.

use seccompiler::TargetArch;

#[cfg(target_arch = "x86_64")]
pub const HOST: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
pub const HOST: TargetArch = TargetArch::aarch64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("atl-broker's seccomp filters have no seccompiler target arch for this host");

/// binderd's syscalls besides socketpair, by role: socket I/O (its whole job), memory, start-up (ld.so + glibc + Rust std; execve is the call that starts it, the filter being installed before), std's signal set-up, exit; and bwrap's pid 1 (wait4).
#[cfg(target_arch = "x86_64")]
pub const BINDERD_SYSCALLS: &[i64] = &[
    // socket I/O and fds
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_close,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_poll,
    libc::SYS_ppoll,
    libc::SYS_fcntl,
    // memory
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_brk,
    // start-up: the loader opening its closure, glibc and std init
    libc::SYS_execve,
    libc::SYS_openat,
    libc::SYS_access,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_pread64,
    libc::SYS_arch_prctl,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_prlimit64,
    libc::SYS_getrandom,
    libc::SYS_rt_sigaction,
    libc::SYS_sigaltstack,
    // atl-sandbox-stub (runs under this filter before it execs binderd): its Landlock domain
    libc::SYS_landlock_create_ruleset,
    libc::SYS_landlock_add_rule,
    libc::SYS_landlock_restrict_self,
    libc::SYS_prctl,
    // exit; bwrap's pid 1 reaping binderd
    libc::SYS_exit_group,
    libc::SYS_wait4,
];
#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "binderd's seccomp allow-list is measured on x86_64 only: measure this arch (arch.rs, BINDERD_SYSCALLS derivation) before building the broker here"
);
