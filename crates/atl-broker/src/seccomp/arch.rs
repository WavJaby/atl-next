// SPDX-License-Identifier: MIT
//! The seccomp target arch of the host this crate is built for and its per-arch rows: seccompiler's arch (its audit value) and binderd's measured allow-list.
//! Numbers are the target's own (libc::SYS_*, per target arch), never another arch's; seccompiler 0.5 checks HOST's audit arch (AUDIT_ARCH_X86_64 / AUDIT_ARCH_RISCV64) first, so a filter built for one arch kills every call on another.
//! riscv64 rows (asm-generic syscall table: no open, access, poll, readlink, unlink, chmod, rename, mkdir, arch_prctl): the x86_64 measurement with each x86-only call swapped for the *at / p* call glibc issues instead (sysdeps/unix/sysv/linux: access -> faccessat, rename -> renameat2, poll -> ppoll, ...). Not measured on riscv64 yet: kill mode on a riscv64 host (plan B3b) confirms or extends them; each swap is a cfg pair in the list.
//!
//! BINDERD_SYSCALLS derivation (x86_64, measured 2026-11-06, WSL2 kernel 6.6.87.2; strace is not installed, and RET_LOG lost records to kauditd's printk ratelimit: 1470 suppressed in one run):
//! kill-mode iteration -- binderd's filter = allow the known set, default KILL_PROCESS; each run dies at the first call outside it and the kernel's one audit record (`type=1326 ... syscall=N`, in dmesg) names it; add, rerun, until the workload passes.
//! Workloads: `xtask test binder` (converged after 20 runs), then `test app`, `test parcel`, `test window --stage ime` added nothing. socketpair is not listed here: binderd_program allows it for AF_UNIX only.
//! atl-sandbox-stub (2026-11-06, same kernel): bwrap installs this filter, then execs the stub, which applies Landlock and execs binderd -- so this one filter sees the stub's calls and binderd's; Landlock sees only file access after restrict_self (binderd's start-up opens).
//! Kill mode, `test binder`: run 1 died at 444 (landlock_create_ruleset); 445/446 added with it (the next two calls of the same sequence, landlock-0.4.7 compat.rs) and prctl (its PR_SET_NO_NEW_PRIVS before restrict_self, errors.rs); run 2 passed with no record. openat/close of the stub's PathFds were already listed.
//! Not measured, so killed: every path the gates do not reach, e.g. a panic's abort (rt_sigprocmask, tgkill) -- the panic message is written first, the abort ends as SIGSYS instead of SIGABRT.
//! Re-measure after binderd or its Rust/glibc start code changes (a new call ends every gate with "binderd ended"); the procedure needs nothing but the filter and dmesg.
//! Message sizes (2026-11-13, kill mode, same kernel; `xtask test binder` stage `sweep`): a message past two 32 KiB fragments (65536 payload bytes) takes a buffer glibc maps (128 KiB and up), and freeing it is munmap (11). With the list as it was, the sweep's put of 65536 bytes ended at munmap; with munmap, every sweep row passed, both directions and oneway, up to the region limit. mremap (25) and madvise (28) were not hit by that sweep, so they are not listed; a new call ends the sweep with dmesg naming it.

use seccompiler::TargetArch;

#[cfg(target_arch = "x86_64")]
pub const HOST: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "riscv64")]
pub const HOST: TargetArch = TargetArch::riscv64;
#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
compile_error!(
    "atl-broker's seccomp allow-lists exist for x86_64 (measured) and riscv64 (derived) only: measure this arch (BINDERD_SYSCALLS derivation) before building the broker here"
);

/// binderd's syscalls besides socketpair, by role: socket I/O (its whole job), memory, start-up (ld.so + glibc + Rust std; execve is the call that starts it, the filter being installed before), std's signal set-up, exit; and bwrap's pid 1 (wait4).
pub const BINDERD_SYSCALLS: &[i64] = &[
    // socket I/O and fds
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_close,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_poll,
    libc::SYS_ppoll,
    libc::SYS_fcntl,
    // memory
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_brk,
    // free of a buffer glibc mapped (a message past 65536 bytes; header, 2026-11-13 sweep)
    libc::SYS_munmap,
    // start-up: the loader opening its closure, glibc and std init
    libc::SYS_execve,
    libc::SYS_openat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_access,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_faccessat,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_pread64,
    #[cfg(target_arch = "x86_64")]
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

/// riscv64 uapi asm/unistd.h: __NR_riscv_flush_icache = __NR_arch_specific_syscall (244) + 15; libc 0.2 does not export it.
#[cfg(target_arch = "riscv64")]
const SYS_RISCV_FLUSH_ICACHE: i64 = 244 + 15;

/// The installer's syscalls (x86_64, 2026-11-13, WSL2 kernel 6.6.87.2), derived by measurement:
/// 1. gdb `catch syscall` census of the installer's gate paths: the union of 57 numbers. alto's run was cut at 900 s (8.4 M stops) before its native copy; `feature` ran to its end (56 numbers); the others ended early.
/// 2. Kill-mode gate runs: alto's native copy makes calls no census reached: unlink (87), chmod (90), fsync (74), utimensat (280), fallocate (285), rename (82). Each run ended at the first unlisted call (SIGSYS, the call named by dmesg `type=1326`).
/// 3. The sandbox's own start-up (bwrap's pid 1 and atl-sandbox-stub run under this filter too): execve (59), wait4 (61), the Landlock calls (444-446), from binderd's measured list.
/// 4. Kill-mode app-gate run after the install path gained certificate collection (ParsingPackageUtils.getSigningDetails -> ApkSignatureVerifier.verify): getsockopt (55), named by dmesg `type=1326`.
///
/// The list gives 68 numbers. socket(2) is absent from it: seccomp.rs allows it for AF_UNIX only. setpriority(2) is measured (ART's thread priorities); no EACCES from it reached the gate's stderr.
///
/// Final evidence: the installer gate green in kill mode with 0 denials in dmesg on three runs. Re-measure after installer, ART or glibc start-up code changes: a new call ends the run with SIGSYS, and dmesg names it.
pub const INSTALLER_SYSCALLS: &[i64] = &[
    // file and socket I/O (socket itself: AF_UNIX only, binderd-style rule in seccomp.rs); riscv64: open -> the openat already listed
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_writev,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_open,
    libc::SYS_openat,
    libc::SYS_close,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    // riscv64 measured (sifive, Ubuntu glibc 2.39, kernel kill + strace 2026-11-15): atl-installer's first call after exec is statx(openat("/", O_PATH), ".", TYPE|INO|MNT_ID)
    #[cfg(target_arch = "riscv64")]
    libc::SYS_statx,
    libc::SYS_lseek,
    libc::SYS_pread64,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_access,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_faccessat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_readlink,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_readlinkat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_unlink,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_unlinkat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chmod,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_fchmodat,
    libc::SYS_fallocate,
    libc::SYS_fsync,
    libc::SYS_utimensat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_rename,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_renameat2,
    libc::SYS_getdents64,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_mkdir,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_mkdirat,
    libc::SYS_ftruncate,
    libc::SYS_getcwd,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_poll,
    #[cfg(target_arch = "riscv64")]
    libc::SYS_ppoll,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_connect,
    libc::SYS_getsockopt,
    // memory
    libc::SYS_mmap,
    libc::SYS_mprotect,
    libc::SYS_munmap,
    libc::SYS_mincore,
    libc::SYS_brk,
    libc::SYS_madvise,
    libc::SYS_memfd_create,
    libc::SYS_userfaultfd,
    // threads, signals, ids, priority
    libc::SYS_clone3,
    libc::SYS_futex,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_setpriority,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
    libc::SYS_membarrier,
    // riscv64 measured (sifive 2026-11-15, the run after statx): ART's code writes flush the icache by syscall; x86 icache is coherent, no counterpart
    #[cfg(target_arch = "riscv64")]
    SYS_RISCV_FLUSH_ICACHE,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigtimedwait,
    libc::SYS_sigaltstack,
    libc::SYS_tgkill,
    libc::SYS_restart_syscall,
    libc::SYS_gettid,
    libc::SYS_getpid,
    libc::SYS_getuid,
    libc::SYS_uname,
    libc::SYS_prlimit64,
    libc::SYS_getrandom,
    libc::SYS_clock_gettime,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_arch_prctl,
    libc::SYS_prctl,
    libc::SYS_exit,
    libc::SYS_exit_group,
    // the sandbox's own start-up (bwrap's pid 1 and atl-sandbox-stub run under this filter too): exec of the stub, wait, its Landlock domain
    libc::SYS_execve,
    libc::SYS_wait4,
    libc::SYS_landlock_create_ruleset,
    libc::SYS_landlock_add_rule,
    libc::SYS_landlock_restrict_self,
];
