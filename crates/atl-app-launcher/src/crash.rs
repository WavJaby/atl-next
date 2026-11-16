// SPDX-License-Identifier: MIT
//! The launcher's last word on a fatal signal: one line on stderr, the process's log, before the signal is resent.
//!
//! Why: with no ART handler ahead of the resend, a crash left no line at all (Alto's Odyssey, 2026-11-17: exit 139, nothing logged; `on_crash` had reset the disposition). debuggerd's tombstone is the device's; this is the host's minimum: signal, fault address, thread, and each frame as `path+offset`, which `nm` or `addr2line` resolve on that file.
//! Async-signal-safe: raw syscalls, stack buffers, `/proc/self/maps` read in chunks. Unwinding (libgcc) takes the dl lock, so a crash while another thread is inside dlopen waits there instead of dying; ART's handler carries the same risk.

use core::ffi::{c_int, c_long, c_void};

use crate::generated::{signal, syscall as sys};

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn _Unwind_Backtrace(
        step: extern "C" fn(context: *mut c_void, arg: *mut c_void) -> c_int,
        arg: *mut c_void,
    ) -> c_int;
    fn _Unwind_GetIP(context: *mut c_void) -> usize;
}

/// Frames the report keeps from the unwinder.
const MAX_FRAMES: usize = 32;
/// One report line: fixed text plus a path.
const LINE_CAP: usize = 512;
/// A mapping's path, cut at this many bytes.
const PATH_CAP: usize = 256;
/// `/proc/self/maps` is read this many bytes at a time; a maps line never exceeds it.
const CHUNK: usize = 4096;
/// glibc's `siginfo_t` on LP64 (x86_64, arm64): `si_code` at byte 8; the union from byte 16, whose first field is `_sigfault.si_addr`.
const SI_CODE_OFFSET: usize = 8;
const SI_ADDR_OFFSET: usize = 16;
/// `prctl(PR_GET_NAME)` (linux/prctl.h) writes the thread name into 16 bytes, NUL included.
const PR_GET_NAME: c_long = 16;
const THREAD_NAME_CAP: usize = 16;
/// `CLOCK_REALTIME` (time.h).
const CLOCK_REALTIME: c_long = 0;
/// `AT_FDCWD` (fcntl.h).
const AT_FDCWD: c_long = -100;
/// `O_RDONLY | O_CLOEXEC`: `O_CLOEXEC` is 02000000 on every Linux arch.
const O_RDONLY_CLOEXEC: c_long = 0o2000000;
/// stderr's descriptor.
const STDERR: c_long = 2;

#[repr(C)]
struct Timespec {
    tv_sec: c_long,
    tv_nsec: c_long,
}

struct Frames {
    pcs: [usize; MAX_FRAMES],
    len: usize,
}

/// One report line, built in a stack buffer and written with one `write(2)`.
struct Line {
    buf: [u8; LINE_CAP],
    len: usize,
}

impl Line {
    const fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
        }
    }

    /// Appends `s`; the rest is cut when the line is full.
    fn bytes(&mut self, s: &[u8]) {
        for &b in s {
            if self.len == LINE_CAP {
                return;
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn dec(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.bytes(&digits[i..]);
    }

    fn signed(&mut self, v: i64) {
        if v < 0 {
            self.bytes(b"-");
        }
        self.dec(v.unsigned_abs());
    }

    fn hex(&mut self, v: usize) {
        let mut digits = [0u8; 16];
        let mut i = digits.len();
        let mut rest = v;
        loop {
            i -= 1;
            digits[i] = b"0123456789abcdef"[rest & 0xf];
            rest >>= 4;
            if rest == 0 {
                break;
            }
        }
        self.bytes(b"0x");
        self.bytes(&digits[i..]);
    }

    /// `<seconds>.<millis>` then the fixed prefix of every line.
    fn prefix(&mut self, now: &Timespec) {
        self.dec(now.tv_sec as u64);
        self.bytes(b".");
        let millis = now.tv_nsec as u64 / 1_000_000;
        if millis < 100 {
            self.bytes(b"0");
        }
        if millis < 10 {
            self.bytes(b"0");
        }
        self.dec(millis);
        self.bytes(b" FATAL atl-crash:");
    }

    fn emit(&mut self) {
        // SAFETY: write(2) on stderr from this buffer; the kernel reads at most `len` bytes
        unsafe {
            syscall(
                sys::SYS_write.into(),
                STDERR,
                self.buf.as_ptr(),
                self.len as c_long,
            )
        };
        self.len = 0;
    }
}

/// Writes the report for `sig` delivered with `info` (SA_SIGINFO), before the caller resends it.
///
/// # Safety
/// `info` is the kernel's siginfo for this delivery, valid for the call.
pub unsafe fn report(sig: c_int, info: *const c_void) {
    // SAFETY: the kernel's siginfo is at least SI_ADDR_OFFSET + one word; unaligned reads
    let (code, addr) = unsafe {
        (
            info.byte_add(SI_CODE_OFFSET)
                .cast::<c_int>()
                .read_unaligned(),
            info.byte_add(SI_ADDR_OFFSET)
                .cast::<usize>()
                .read_unaligned(),
        )
    };
    let now = now();
    let mut thread_name = [0u8; THREAD_NAME_CAP];
    // SAFETY: prctl(PR_GET_NAME) writes at most THREAD_NAME_CAP bytes into the buffer
    unsafe { syscall(sys::SYS_prctl.into(), PR_GET_NAME, thread_name.as_mut_ptr()) };
    let tid = rustix::thread::gettid().as_raw_nonzero().get();

    let mut line = Line::new();
    line.prefix(&now);
    line.bytes(b" signal ");
    line.dec(sig as u64);
    line.bytes(b" (");
    line.bytes(signal_name(sig));
    line.bytes(b") code ");
    line.signed(i64::from(code));
    line.bytes(b" fault addr ");
    line.hex(addr);
    line.bytes(b" thread ");
    line.dec(tid as u64);
    line.bytes(b" \"");
    line.bytes(thread_name.split(|&b| b == 0).next().unwrap_or_default());
    line.bytes(b"\"");
    line.emit();

    let mut frames = Frames {
        pcs: [0; MAX_FRAMES],
        len: 0,
    };
    // SAFETY: the unwinder calls `collect` with its context and `arg`, both live for this call
    unsafe { _Unwind_Backtrace(collect, (&raw mut frames).cast()) };
    for (i, &pc) in frames.pcs[..frames.len].iter().enumerate() {
        let mut path = [0u8; PATH_CAP];
        let mut line = Line::new();
        line.prefix(&now);
        line.bytes(b"   #");
        line.dec(i as u64);
        line.bytes(b" ");
        match locate(pc, &mut path) {
            Some((len, offset)) => {
                line.bytes(&path[..len]);
                line.bytes(b"+");
                line.hex(offset);
            }
            None => {
                line.bytes(b"?+");
                line.hex(pc);
            }
        }
        line.emit();
    }
}

fn signal_name(sig: c_int) -> &'static [u8] {
    match sig {
        signal::SIGABRT => b"SIGABRT",
        signal::SIGBUS => b"SIGBUS",
        signal::SIGFPE => b"SIGFPE",
        signal::SIGILL => b"SIGILL",
        signal::SIGSEGV => b"SIGSEGV",
        signal::SIGSTKFLT => b"SIGSTKFLT",
        signal::SIGSYS => b"SIGSYS",
        signal::SIGTRAP => b"SIGTRAP",
        _ => b"unknown",
    }
}

fn now() -> Timespec {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one timespec into `ts`
    unsafe { syscall(sys::SYS_clock_gettime.into(), CLOCK_REALTIME, &raw mut ts) };
    ts
}

/// The unwinder's step: records the frame's pc, continues (`_URC_NO_REASON`).
extern "C" fn collect(context: *mut c_void, arg: *mut c_void) -> c_int {
    // SAFETY: `arg` is the `Frames` the caller passed to `_Unwind_Backtrace`, live for the call
    let frames = unsafe { &mut *arg.cast::<Frames>() };
    // SAFETY: `context` is the unwinder's context for the frame being visited
    let pc = unsafe { _Unwind_GetIP(context) };
    if pc != 0 && frames.len < MAX_FRAMES {
        frames.pcs[frames.len] = pc;
        frames.len += 1;
    }
    0
}

/// The mapping holding `pc` (from `/proc/self/maps`): its path copied into `path`, its length, and `pc`'s offset in the file.
/// None when no mapping holds it or the maps cannot be read.
fn locate(pc: usize, path: &mut [u8; PATH_CAP]) -> Option<(usize, usize)> {
    // SAFETY: openat with a NUL-terminated path; the result is checked below
    let fd = unsafe {
        syscall(
            sys::SYS_openat.into(),
            AT_FDCWD,
            c"/proc/self/maps".as_ptr(),
            O_RDONLY_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut found = None;
    let mut buf = [0u8; CHUNK];
    let mut filled = 0;
    'read: loop {
        // SAFETY: read(2) into the free tail of `buf`; the kernel writes at most its length
        let n = unsafe {
            syscall(
                sys::SYS_read.into(),
                fd,
                buf[filled..].as_mut_ptr(),
                (buf.len() - filled) as c_long,
            )
        };
        if n <= 0 {
            break;
        }
        filled += n as usize;
        let mut start = 0;
        while let Some(rel) = buf[start..filled].iter().position(|&b| b == b'\n') {
            if let Some(hit) = match_line(&buf[start..start + rel], pc, path) {
                found = Some(hit);
                break 'read;
            }
            start += rel + 1;
        }
        buf.copy_within(start..filled, 0);
        filled -= start;
    }
    // SAFETY: close(2) on the descriptor opened above
    unsafe { syscall(sys::SYS_close.into(), fd) };
    found
}

/// One maps line (`start-end perms offset dev inode path`): whether it holds `pc`, and if so the path and `pc`'s file offset.
fn match_line(line: &[u8], pc: usize, path: &mut [u8; PATH_CAP]) -> Option<(usize, usize)> {
    let mut rest = line;
    let range = token(&mut rest);
    let _perms = token(&mut rest);
    let offset = token(&mut rest);
    let _dev = token(&mut rest);
    let _inode = token(&mut rest);
    let dash = range.iter().position(|&b| b == b'-')?;
    let start = hex(&range[..dash])?;
    let end = hex(&range[dash + 1..])?;
    if !(start..end).contains(&pc) {
        return None;
    }
    let name = rest.trim_ascii();
    let len = name.len().min(PATH_CAP);
    path[..len].copy_from_slice(&name[..len]);
    Some((len, hex(offset)? + (pc - start)))
}

/// The next space-separated field of `s`, advancing `s` past it.
fn token<'a>(s: &mut &'a [u8]) -> &'a [u8] {
    let skip = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    *s = &s[skip..];
    let end = s.iter().position(|&b| b == b' ').unwrap_or(s.len());
    let (field, rest) = s.split_at(end);
    *s = rest;
    field
}

fn hex(s: &[u8]) -> Option<usize> {
    usize::from_str_radix(core::str::from_utf8(s).ok()?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crash run's libc text mapping (dmesg: segfault ip 0x74a4b320834d in `libc.so.6[74a4b31a1000+163000]`); the file offset of its segment is 0x28000 (`readelf -lW`).
    const LIBC_MAPS_LINE: &[u8] =
        b"74a4b31a1000-74a4b3304000 r-xp 00028000 08:01 1310720                    /usr/lib/x86_64-linux-gnu/libc.so.6";

    #[test]
    fn a_pc_in_a_mapping_resolves_to_its_file_offset() {
        let mut path = [0u8; PATH_CAP];
        let (len, offset) = match_line(LIBC_MAPS_LINE, 0x74a4_b320_834d, &mut path)
            .expect("the pc is in the mapping");
        assert_eq!(&path[..len], b"/usr/lib/x86_64-linux-gnu/libc.so.6");
        // 0x28000 + (0x74a4b320834d - 0x74a4b31a1000) = 0x8f34d, the instruction `readelf`/`objdump` show
        assert_eq!(offset, 0x8f34d);
    }

    #[test]
    fn a_pc_outside_the_mapping_is_not_resolved() {
        let mut path = [0u8; PATH_CAP];
        assert!(match_line(LIBC_MAPS_LINE, 0x74a4_b330_4000, &mut path).is_none());
        assert!(match_line(LIBC_MAPS_LINE, 0x74a4_b31a_0fff, &mut path).is_none());
    }

    #[test]
    fn numbers_and_signal_names_render_in_the_report_format() {
        let mut line = Line::new();
        line.hex(0x308);
        line.bytes(b" ");
        line.signed(-6);
        line.bytes(b" ");
        line.dec(0);
        assert_eq!(&line.buf[..line.len], b"0x308 -6 0");
        assert_eq!(signal_name(signal::SIGSEGV), b"SIGSEGV");
    }
}
