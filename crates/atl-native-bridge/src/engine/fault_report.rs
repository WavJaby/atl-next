// SPDX-License-Identifier: MIT
//! The fatal-signal report, one implementation for every engine: identity's signal paths and the QEMU engine's `atl_host.fault_report` both end here.
//! TLS-free and heap-free (stack buffers, raw syscalls): callable from a signal handler on any thread pointer, including a guest TCB or FS 0. Per host arch only the raw syscall and the ucontext's pc/sp differ (`cfg(target_arch)`, this module only).

use std::io::Write;

/// Last-chance fault report on stderr: signal, code, fault address, pc + sp, thread name, the mappings of pc and fault address, then the stack words above sp. `fs`: the interrupted thread pointer, 0 when the caller does not know it.
///
/// # Safety
/// `info` / `uc`: null or the kernel's siginfo / ucontext of the signal being handled.
pub unsafe fn fault_report(
    why: &str,
    sig: i32,
    info: *const libc::siginfo_t,
    uc: *const libc::c_void,
    fs: usize,
) {
    // SAFETY: the kernel's siginfo / ucontext for this delivery, or null.
    let (code, addr) = unsafe {
        info.as_ref()
            .map_or((0, 0), |i| (i.si_code, i.si_addr() as usize))
    };
    // SAFETY: as above.
    let (pc, sp) = unsafe { uc.cast::<libc::ucontext_t>().as_ref() }.map_or((0, 0), pc_sp);
    let mut comm = [0u8; 32];
    let fd = raw_open(c"/proc/thread-self/comm");
    let comm_len = if fd < 0 {
        0
    } else {
        let n = raw_read(fd, &mut comm).max(0) as usize;
        raw_close(fd);
        n
    };
    let name = comm[..comm_len]
        .strip_suffix(b"\n")
        .unwrap_or(&comm[..comm_len]);
    let tid = raw_syscall3(libc::SYS_gettid, 0, 0, 0);
    let mut line = [0u8; 256];
    let mut w = &mut line[..];
    let _ = write!(
        w,
        "atl-native-bridge: fatal-signal report: {why}: sig {sig} code {code} addr {addr:#x} pc {pc:#x} sp {sp:#x} fs {fs:#x} tid {tid} thread \""
    );
    let _ = w.write_all(name);
    let _ = writeln!(w, "\"");
    let left = w.len();
    raw_write_stderr(&line[..line.len() - left]);
    emit_mappings(&[pc, addr]);
    // stack words: the caller chain of frame-pointerless code; skipped when the fault is on the stack itself (overflow), where reading it would fault again inside the handler
    let span = STACK_WORDS * 8;
    if sp == 0 || (sp.saturating_sub(PAGE)..sp + span).contains(&addr) {
        return;
    }
    for row in 0..STACK_WORDS / 4 {
        let mut w = &mut line[..];
        let _ = write!(w, "atl-native-bridge: stack +{:#x}", row * 32);
        for i in 0..4 {
            // SAFETY: the interrupted thread's live stack, read upward from its sp; STACK_WORDS stays inside the frames above it.
            let word = unsafe { ((sp + (row * 4 + i) * 8) as *const usize).read() };
            let _ = write!(w, " {word:#x}");
        }
        let _ = writeln!(w);
        let left = w.len();
        raw_write_stderr(&line[..line.len() - left]);
    }
}

/// Interrupted pc + sp. x86_64: glibc `ucontext_t` is the kernel frame's layout.
#[cfg(target_arch = "x86_64")]
fn pc_sp(u: &libc::ucontext_t) -> (usize, usize) {
    let g = &u.uc_mcontext.gregs;
    (
        g[libc::REG_RIP as usize] as usize,
        g[libc::REG_RSP as usize] as usize,
    )
}

/// Interrupted pc + sp. riscv64: `__gregs` is the kernel's `struct user_regs_struct` (pc at 0, sp = x2 at 2).
#[cfg(target_arch = "riscv64")]
fn pc_sp(u: &libc::ucontext_t) -> (usize, usize) {
    let g = &u.uc_mcontext.__gregs;
    (g[libc::REG_PC] as usize, g[libc::REG_SP] as usize)
}

/// Raw 3-argument syscall. The diagnostics here run on a thread pointer that may not be glibc's (guest TCB, FS 0): glibc's open/read/write/close are cancellation points that read
/// THREAD_SELF and its `syscall` sets errno through TLS, so either faults there (g5 caught it). Returns the kernel's value (negative errno on failure).
fn raw_syscall3(nr: libc::c_long, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the callers pass only open/read/write/close/gettid with pointers to live buffers / NUL-terminated paths; the kernel clobbers rcx and r11 only.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: the same callers; riscv64 `ecall` takes nr in a7, args in a0-a2, returns in a0 and clobbers nothing else.
    unsafe {
        std::arch::asm!(
            "ecall",
            in("a7") nr,
            inlateout("a0") a as isize => ret,
            in("a1") b,
            in("a2") c,
            options(nostack),
        );
    }
    ret
}

/// `open(path, O_RDONLY)` without glibc (see [`raw_syscall3`]); negative on failure.
fn raw_open(path: &std::ffi::CStr) -> i32 {
    raw_syscall3(
        libc::SYS_openat,
        libc::AT_FDCWD as usize,
        path.as_ptr() as usize,
        libc::O_RDONLY as usize,
    ) as i32
}

/// `read` into `buf` without glibc; negative on failure.
fn raw_read(fd: i32, buf: &mut [u8]) -> isize {
    raw_syscall3(
        libc::SYS_read,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

/// `write(2, buf)` without glibc; a failed write loses only these bytes.
pub fn raw_write_stderr(buf: &[u8]) {
    raw_syscall3(libc::SYS_write, 2, buf.as_ptr() as usize, buf.len());
}

/// `close` without glibc.
fn raw_close(fd: i32) {
    raw_syscall3(libc::SYS_close, fd as usize, 0, 0);
}

/// Guard below sp that `fault_report` treats as the stack (one 4 KiB page: the smallest guard a fault past sp lands in, on every supported host).
const PAGE: usize = 4096;

/// Stack words a report dumps: enough for a few frames of caller chain.
pub const STACK_WORDS: usize = 96;

/// Whether maps line `line` (`start-end perms ...`, hex) covers `addr`.
fn maps_line_covers(line: &[u8], addr: usize) -> bool {
    let hex = |s: &[u8]| {
        s.iter().try_fold(0usize, |v, c| {
            Some(v.checked_mul(16)? + (*c as char).to_digit(16)? as usize)
        })
    };
    let mut range = line
        .split(|c| *c == b' ')
        .next()
        .unwrap_or(&[])
        .split(|c| *c == b'-');
    match (range.next().and_then(hex), range.next().and_then(hex)) {
        (Some(start), Some(end)) => (start..end).contains(&addr),
        _ => false,
    }
}

/// Writes to stderr the `/proc/self/maps` lines covering `addrs`; streamed through a stack buffer.
pub fn emit_mappings(addrs: &[usize]) {
    let fd = raw_open(c"/proc/self/maps");
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 8192];
    let mut have = 0;
    loop {
        let n = raw_read(fd, &mut buf[have..]);
        if n <= 0 {
            break;
        }
        have += n as usize;
        let mut done = 0;
        while let Some(nl) = buf[done..have].iter().position(|c| *c == b'\n') {
            let line = &buf[done..=done + nl];
            if addrs.iter().any(|a| maps_line_covers(line, *a)) {
                raw_write_stderr(line);
            }
            done += nl + 1;
        }
        buf.copy_within(done..have, 0);
        have -= done;
    }
    raw_close(fd);
}

#[cfg(test)]
mod tests {
    use super::maps_line_covers;

    /// proc(5): the range is `start-end`, end exclusive; a line without one covers nothing.
    #[test]
    fn maps_range_is_half_open() {
        let line = b"7f00-7f10 r-xp 00000000 08:30 1 /lib/x.so\n";
        assert!(maps_line_covers(line, 0x7f00));
        assert!(maps_line_covers(line, 0x7f0f));
        assert!(!maps_line_covers(line, 0x7f10));
        assert!(!maps_line_covers(b"not a mapping\n", 0x7f00));
    }
}
