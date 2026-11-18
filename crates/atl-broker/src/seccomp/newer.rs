// SPDX-License-Identifier: MIT
//! The syscalls newer glibc, Rust std or kernels issue in place of (or beside) a call the measured allow-lists already hold, with each kill-default profile's answer as a field.
//! Condition defended: arch.rs's lists are measured on one host (WSL Debian 13, glibc 2.41, kernel 6.6); a newer host reaches calls that measurement never saw, and an unlisted call kills the process
//! (laptop 2026-11-18, Arch glibc 2.44 kernel 7.2: atl-installer SIGSYS on statx, every install failed). Each row is decided once here, never by a re-measurement on the next host.
//! Answer rule: Allow when the call gives nothing the profile's listed calls do not already give (same check or same metadata through the same filesystem view); Enosys when its caller falls back
//! on ENOSYS and the profile does not need what the call adds -- the convention of container runtimes (moby/profiles seccomp/default.json answers clone3 with errnoRet 38 = ENOSYS, moby commit 9f6b562d,
//! so glibc falls back to clone; Flatpak's common/flatpak-run.c does the same, `{SCMP_SYS (clone3), ENOSYS}`, CVE-2021-41133; both read 2026-11-18). A row's caller is cited from glibc 2.44 (sysdeps/unix/sysv/linux) or Rust std (library/std/src/sys/fs/unix.rs).
//! glibc 2.41 and 2.44 are identical on every path below (diffed 2026-11-18); x86_64 stat/fstat/fstatat never reach statx in either (internal-stat.h: FSTATAT_USE_STATX is 0 where newfstatat exists).

/// What a kill-default profile's filter answers a row's call with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The call runs.
    Allow,
    /// SECCOMP_RET_ERRNO with ENOSYS: the caller takes its documented fallback.
    Enosys,
}

/// One substituted call: its number, its name (the probe's key), the answer of each allow-list profile, who issues it, and why the answer is safe.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub call: i64,
    pub name: &'static str,
    pub binderd: Answer,
    pub installer: Answer,
    pub caller: &'static str,
    pub reason: &'static str,
}

/// Kernel uapi numbers libc 0.2 does not export on every arch; calls from 424 on share one number on every arch (asm-generic and x86_64 tables alike).
const SYS_MAP_SHADOW_STACK: i64 = 453;
const SYS_FCHMODAT2: i64 = 452;
const SYS_MSEAL: i64 = 462;

use Answer::{Allow, Enosys};

pub const ROWS: &[Row] = &[
    Row {
        call: libc::SYS_statx,
        name: "statx",
        binderd: Allow,
        installer: Allow,
        caller: "Rust std try_statx (fs::metadata, symlink_metadata, File::metadata, fs::read's size hint) on every linux-gnu arch -- the laptop kill, glibc being unchanged; glibc's stat family only on arches without newfstatat; the riscv64-measured statx(\"/\" O_PATH, MNT_ID) at installer start",
        reason: "read-only metadata, the same view as the listed fstat/newfstatat; Rust std falls back only on an error, so ENOSYS would also work, but nothing is gained by hiding it",
    },
    Row {
        call: libc::SYS_faccessat2,
        name: "faccessat2",
        binderd: Allow,
        installer: Allow,
        caller: "glibc faccessat.c __faccessat: faccessat2 first for every faccessat(), faccessat on ENOSYS",
        reason: "the same permission check as the listed access/faccessat",
    },
    Row {
        call: libc::SYS_rseq,
        name: "rseq",
        binderd: Allow,
        installer: Allow,
        caller: "glibc rseq-internal.h: registration at process and thread start (measured in both lists)",
        reason: "per-thread registration of the caller's own memory; failure is tolerated by glibc, but it was measured allowed",
    },
    Row {
        call: libc::SYS_set_robust_list,
        name: "set_robust_list",
        binderd: Allow,
        installer: Allow,
        caller: "glibc dl-tls_init_tp.c, pthread_create.c: robust futex list per thread (measured in both lists)",
        reason: "registers the caller's own memory",
    },
    Row {
        call: libc::SYS_clone3,
        name: "clone3",
        binderd: Enosys,
        installer: Allow,
        caller: "glibc clone-internal.c __clone_internal: clone3 first (pthread_create, posix_spawn), clone on ENOSYS",
        reason: "installer: measured (ART's threads), namespace flags are refused by its user namespace anyway; binderd: single-threaded, ENOSYS then reaches clone, which kills (unlisted)",
    },
    Row {
        call: libc::SYS_membarrier,
        name: "membarrier",
        binderd: Enosys,
        installer: Allow,
        caller: "ART (measured in the installer list); no glibc caller",
        reason: "installer: measured; binderd: no caller, and callers probe with MEMBARRIER_CMD_QUERY and fall back",
    },
    Row {
        call: libc::SYS_close_range,
        name: "close_range",
        binderd: Enosys,
        installer: Enosys,
        caller: "glibc spawni.c (posix_spawn close-from action) and closefrom(): /proc/self/fd scan on ENOSYS",
        reason: "neither profile spawns; the fallback closes the same fds",
    },
    Row {
        call: libc::SYS_openat2,
        name: "openat2",
        binderd: Enosys,
        installer: Enosys,
        caller: "no glibc 2.44 caller; crates with RESOLVE_* support fall back to openat",
        reason: "Landlock bounds the view either way; nothing measured needs RESOLVE_* flags",
    },
    Row {
        call: libc::SYS_epoll_pwait2,
        name: "epoll_pwait2",
        binderd: Enosys,
        installer: Enosys,
        caller: "no glibc 2.44 internal caller (wrapper only); event loops fall back to epoll_pwait",
        reason: "epoll is unlisted in both profiles: no measured path uses it",
    },
    Row {
        call: libc::SYS_futex_waitv,
        name: "futex_waitv",
        binderd: Enosys,
        installer: Enosys,
        caller: "no glibc 2.44 caller; its users test for ENOSYS",
        reason: "futex (listed in the installer) covers the measured waits",
    },
    Row {
        call: SYS_FCHMODAT2,
        name: "fchmodat2",
        binderd: Enosys,
        installer: Enosys,
        caller: "glibc fchmodat.c: fchmodat2 for a nonzero flag (AT_SYMLINK_NOFOLLOW), fchmodat_fallback on ENOSYS; flag 0 stays fchmodat",
        reason: "the fallback does the same chmod through an O_PATH fd and the listed calls",
    },
    Row {
        call: SYS_MSEAL,
        name: "mseal",
        binderd: Enosys,
        installer: Enosys,
        caller: "glibc 2.44 exports the wrapper (bits/mman-shared.h), no internal caller",
        reason: "sealing is hardening its caller may skip; a kill would end the process for asking",
    },
    Row {
        call: SYS_MAP_SHADOW_STACK,
        name: "map_shadow_stack",
        binderd: Enosys,
        installer: Enosys,
        caller: "glibc x86_64/allocate-shadow-stack.c: thread shadow stacks when SHSTK is active (only if every loaded object is SHSTK-marked; Rust and ART objects are not)",
        reason: "not reached by the measured binaries; ENOSYS turns a future SHSTK thread start into an error return, not a SIGSYS",
    },
];

/// `profile_answer`'s calls answered `answer`.
pub fn calls(profile_answer: fn(&Row) -> Answer, answer: Answer) -> Vec<i64> {
    ROWS.iter()
        .filter(|r| profile_answer(r) == answer)
        .map(|r| r.call)
        .collect()
}
