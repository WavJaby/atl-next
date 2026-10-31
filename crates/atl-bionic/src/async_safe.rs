// SPDX-License-Identifier: MIT
//! bionic's async-signal-safe formatting and logging (libasync_safe, `libc/async_safe/async_safe_log.cpp`):
//! no allocation, no locks, fixed stack buffers, raw `writev`. Behaviour is that file's, quirks included:
//!
//! - Format subset: flags `0 - # space +`, width, `.prec`, lengths `hh h l ll z t`, conversions
//!   `s c p m b B d i o u x X %`. A sign flag or a precision, or another conversion, is fatal
//!   (bionic's `__assert`) after its argument is read.
//! - `o` is formatted signed; zero padding goes before a `-` and also after a left-aligned field; a
//!   number keeps its low digits when the 64-byte scratch buffer is too small (`%#b` of a 64-bit value).
//! - `%m`: the C library's `strerror` text for `errno` (here glibc's), `Unknown error N` without one;
//!   `%#m` its name (`ENOENT`), else the number (bionic enables it from API level 35; platform code is
//!   built at `__ANDROID_API_FUTURE__`).
//! - Buffers: always NUL-terminated when not empty; the return value is the untruncated length.
//! - Logging: bionic connects to logd's `/dev/socket/logdw` and, when that fails, writes `tag: msg\n`
//!   to stderr. This host has no logd, so the stderr path is the only one; the priority then has no
//!   effect, as in bionic's fallback.
//! - Fatal: `[prefix: ]msg` (1024 bytes) and `\n` to stderr, then logged as `libc` at fatal priority --
//!   with no logd that is stderr again, as in bionic. bionic also sets the abort message for debuggerd,
//!   which has no counterpart here.

use core::ffi::{c_int, c_long, c_longlong};

/// bionic's `char buffer[1024]` of `format_log` and `fatal`.
const MESSAGE_MAX: usize = 1024;
/// bionic's per-conversion scratch `char buffer[64]`.
const SCRATCH: usize = 64;
/// liblog's `ANDROID_LOG_FATAL` (`android_LogPriority`), the priority `fatal` logs at.
const ANDROID_LOG_FATAL: c_int = 7;
/// The tag `fatal` logs under.
const FATAL_TAG: &[u8] = b"libc";

/// A format's arguments in call order, as its conversions ask for them; `ffi` reads them from a C
/// `va_list`. Promoted `char` / `short` arrive as `int`.
pub trait Args<'s> {
    fn int(&mut self) -> c_int;
    fn u32(&mut self) -> u32;
    fn u64(&mut self) -> u64;
    fn ptr(&mut self) -> usize;
    /// `%s`: the string's bytes, none for a null pointer.
    fn str(&mut self) -> Option<&'s [u8]>;
}

/// Where formatted bytes go.
pub trait Out {
    fn send(&mut self, data: &[u8]);
}

/// bionic's `BufferOutputStream`: truncates, NUL-terminates, counts everything sent.
pub struct Buffer<'b> {
    buf: &'b mut [u8],
    pos: usize,
    pub total: usize,
}

impl<'b> Buffer<'b> {
    pub fn new(buf: &'b mut [u8]) -> Self {
        if let Some(b) = buf.first_mut() {
            *b = 0;
        }
        Self {
            buf,
            pos: 0,
            total: 0,
        }
    }

    /// What was kept, without the terminating NUL.
    pub fn text(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}

impl Out for Buffer<'_> {
    fn send(&mut self, data: &[u8]) {
        self.total += data.len();
        let avail = self.buf.len() - self.pos;
        if avail <= 1 {
            return;
        }
        let n = data.len().min(avail - 1);
        self.buf[self.pos..self.pos + n].copy_from_slice(&data[..n]);
        self.pos += n;
        self.buf[self.pos] = 0;
    }
}

/// The C library's error description and name for an errno value (`ffi`: glibc's tables).
pub struct Errors {
    pub describe: fn(c_int) -> Option<&'static [u8]>,
    pub name: fn(c_int) -> Option<&'static [u8]>,
}

/// `strlen` of a NUL-terminated scratch buffer.
fn until_nul(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// bionic `parse_decimal`: digits from `*pos`, unsigned wrap-around.
fn parse_decimal(format: &[u8], pos: &mut usize) -> u32 {
    let mut v: u32 = 0;
    while let Some(d) = format.get(*pos).filter(|c| c.is_ascii_digit()) {
        v = v.wrapping_mul(10).wrapping_add(u32::from(d - b'0'));
        *pos += 1;
    }
    v
}

/// bionic `format_unsigned`: digits of `value` in `base` into `buf`, at most `buf.len() - 1` of them
/// (the low ones), NUL-terminated.
fn format_unsigned(buf: &mut [u8], mut value: u64, base: u64, caps: bool) {
    let end = buf.len() - 1;
    let mut p = 0;
    while value != 0 {
        let d = (value % base) as u8;
        value /= base;
        if p != end {
            buf[p] = match d {
                0..=9 => b'0' + d,
                _ => (if caps { b'A' } else { b'a' }) + d - 10,
            };
            p += 1;
        }
    }
    if p == 0 && p != end {
        buf[0] = b'0';
        p = 1;
    }
    buf[p] = 0;
    buf[..p].reverse();
}

/// bionic `format_integer`: `-` and the magnitude for a negative signed conversion (`d i o`).
fn format_integer(buf: &mut [u8], value: u64, conversion: u8) {
    let base = match conversion.to_ascii_lowercase() {
        b'x' => 16,
        b'o' => 8,
        b'b' => 2,
        _ => 10,
    };
    let signed = matches!(conversion, b'd' | b'i' | b'o');
    if signed && (value as i64) < 0 {
        buf[0] = b'-';
        format_unsigned(
            &mut buf[1..],
            (value as i64).wrapping_neg() as u64,
            base,
            false,
        );
    } else {
        format_unsigned(buf, value, base, conversion == b'X');
    }
}

fn send_repeat(o: &mut impl Out, c: u8, mut count: usize) {
    let pad = [c; 8];
    while count > 0 {
        let n = count.min(pad.len());
        o.send(&pad[..n]);
        count -= n;
    }
}

/// bionic `out_vformat`: `format` (no NUL) with `args` into `o`.
pub fn vformat<'s>(o: &mut impl Out, format: &[u8], args: &mut impl Args<'s>, errors: &Errors) {
    // past the end reads as the C string's NUL
    let at = |i: usize| format.get(i).copied().unwrap_or(0);
    let mut nn = 0;
    loop {
        let run = format[nn..]
            .iter()
            .position(|&c| c == b'%')
            .map_or(format.len(), |p| nn + p);
        if run > nn {
            o.send(&format[nn..run]);
            nn = run;
        }
        if nn == format.len() {
            return;
        }
        nn += 1;

        let (mut pad_zero, mut pad_left, mut sign, mut alternate) = (false, false, false, false);
        let mut c;
        loop {
            c = at(nn);
            nn += 1;
            match c {
                // a single trailing '%'
                0 => {
                    o.send(b"%");
                    return;
                }
                b'0' => pad_zero = true,
                b'-' => pad_left = true,
                b' ' | b'+' => sign = true,
                b'#' => alternate = true,
                _ => break,
            }
        }
        // bionic's `int width = -1`: a wrapped-around width is negative and pads nothing
        let mut width: i64 = -1;
        if c.is_ascii_digit() {
            nn -= 1;
            width = i64::from(parse_decimal(format, &mut nn) as i32);
            c = at(nn);
            nn += 1;
        }
        let mut precision = false;
        if c == b'.' {
            parse_decimal(format, &mut nn);
            precision = true;
            c = at(nn);
            nn += 1;
        }
        let mut bytelen = size_of::<c_int>();
        let mut length = |len: usize, doubled: Option<usize>, c: &mut u8| {
            bytelen = len;
            if let Some(d) = doubled.filter(|_| at(nn) == *c) {
                bytelen = d;
                nn += 1;
            }
            *c = at(nn);
            nn += 1;
        };
        match c {
            b'h' => length(size_of::<i16>(), Some(size_of::<i8>()), &mut c),
            b'l' => length(size_of::<c_long>(), Some(size_of::<c_longlong>()), &mut c),
            b'z' => length(size_of::<usize>(), None, &mut c),
            b't' => length(size_of::<isize>(), None, &mut c),
            _ => {}
        }

        let mut scratch = [0u8; SCRATCH];
        let text: &[u8] = match c {
            b's' => args.str().unwrap_or(b"(null)"),
            b'c' => {
                scratch[0] = args.int() as u8;
                until_nul(&scratch)
            }
            b'p' => {
                scratch[..2].copy_from_slice(b"0x");
                format_integer(&mut scratch[2..], args.ptr() as u64, b'x');
                until_nul(&scratch)
            }
            b'm' => {
                let e = errno::errno().0;
                let found = if alternate {
                    (errors.name)(e)
                } else {
                    (errors.describe)(e)
                };
                match found {
                    Some(t) => {
                        let n = t.len().min(SCRATCH - 1);
                        scratch[..n].copy_from_slice(&t[..n]);
                    }
                    None if alternate => format_integer(&mut scratch, e as u64, b'd'),
                    // bionic strerror_r: "Unknown error %d", truncated to the buffer
                    None => {
                        let mut b = Buffer::new(&mut scratch);
                        b.send(b"Unknown error ");
                        let mut digits = [0u8; 24];
                        format_integer(&mut digits, i64::from(e) as u64, b'd');
                        b.send(until_nul(&digits));
                    }
                }
                until_nul(&scratch)
            }
            b'b' | b'B' | b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => {
                let raw = match bytelen {
                    1 => u64::from(args.int() as u8),
                    2 => u64::from(args.int() as u16),
                    4 => u64::from(args.u32()),
                    _ => args.u64(),
                };
                let value = if matches!(c, b'd' | b'i' | b'o') {
                    let shift = 64 - 8 * bytelen as u32;
                    (((raw << shift) as i64) >> shift) as u64
                } else {
                    raw
                };
                if alternate && value != 0 && matches!(c, b'x' | b'X' | b'o' | b'b' | b'B') {
                    scratch[0] = b'0';
                    if c == b'o' {
                        format_integer(&mut scratch[1..], value, c);
                    } else {
                        scratch[1] = c;
                        format_integer(&mut scratch[2..], value, c);
                    }
                } else {
                    format_integer(&mut scratch, value, c);
                }
                until_nul(&scratch)
            }
            b'%' => b"%",
            _ => unsupported(b"conversion specifier unsupported"),
        };
        if sign || precision {
            unsupported(b"sign/precision unsupported");
        }
        let text = until_nul(text);
        let pad = usize::try_from(width)
            .unwrap_or(0)
            .saturating_sub(text.len());
        let pad_char = if pad_zero { b'0' } else { b' ' };
        if !pad_left {
            send_repeat(o, pad_char, pad);
        }
        o.send(text);
        if pad_left {
            send_repeat(o, pad_char, pad);
        }
    }
}

/// bionic's `__assert` in the formatter: fatal.
fn unsupported(what: &[u8]) -> ! {
    fatal_text(b"async_safe_log: ", what);
    std::process::abort()
}

/// One `writev` to stderr, retried on `EINTR`: the byte count, or the error.
fn write_stderr(parts: &[&[u8]]) -> Result<usize, rustix::io::Errno> {
    let mut slices = [std::io::IoSlice::new(&[]); 4];
    for (s, p) in slices.iter_mut().zip(parts) {
        *s = std::io::IoSlice::new(p);
    }
    let slices = &slices[..parts.len().min(4)];
    loop {
        match rustix::io::writev(rustix::stdio::stderr(), slices) {
            Err(rustix::io::Errno::INTR) => continue,
            r => return r,
        }
    }
}

/// `async_safe_write_log`, logd absent: `tag: msg\n` to stderr; bytes written, or the errno.
pub fn write_log(_priority: c_int, tag: &[u8], msg: &[u8]) -> Result<usize, i32> {
    write_stderr(&[tag, b": ", msg, b"\n"]).map_err(|e| e.raw_os_error())
}

/// `async_safe_format_log`: up to 1024 bytes of the formatted message, logged; errno kept.
pub fn format_log<'s>(
    priority: c_int,
    tag: &[u8],
    format: &[u8],
    args: &mut impl Args<'s>,
    errors: &Errors,
) -> Result<usize, i32> {
    let saved = errno::errno();
    let mut buf = [0u8; MESSAGE_MAX];
    let mut out = Buffer::new(&mut buf);
    vformat(&mut out, format, args, errors);
    let r = write_log(priority, tag, out.text());
    errno::set_errno(saved);
    r
}

/// `async_safe_fatal_va_list` up to the abort (the caller's): `[prefix: ]msg` to stderr, then logged.
pub fn fatal<'s>(prefix: Option<&[u8]>, format: &[u8], args: &mut impl Args<'s>, errors: &Errors) {
    let mut buf = [0u8; MESSAGE_MAX];
    let mut out = Buffer::new(&mut buf);
    if let Some(p) = prefix {
        out.send(p);
        out.send(b": ");
    }
    vformat(&mut out, format, args, errors);
    log_fatal(out.text());
}

/// `msg` to stderr and to the log at fatal priority, as `fatal` ends.
fn log_fatal(msg: &[u8]) {
    let _ = write_stderr(&[msg, b"\n"]);
    let _ = write_log(ANDROID_LOG_FATAL, FATAL_TAG, msg);
}

/// `fatal` of a fixed text (no format).
fn fatal_text(prefix: &[u8], what: &[u8]) {
    let mut buf = [0u8; MESSAGE_MAX];
    let mut out = Buffer::new(&mut buf);
    out.send(prefix);
    out.send(what);
    log_fatal(out.text());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One argument of a test call.
    #[derive(Clone, Copy)]
    enum A<'s> {
        I(c_int),
        U(u64),
        P(usize),
        S(Option<&'s [u8]>),
    }

    /// Test arguments, read as C would: `va_arg(int)` of an `int`, `va_arg(uint64_t)` of a 64-bit value.
    struct List<'a, 's>(std::slice::Iter<'a, A<'s>>);

    impl<'s> Args<'s> for List<'_, 's> {
        fn int(&mut self) -> c_int {
            match self.0.next().copied() {
                Some(A::I(v)) => v,
                _ => panic!("not an int"),
            }
        }
        fn u32(&mut self) -> u32 {
            match self.0.next().copied() {
                Some(A::I(v)) => v as u32,
                _ => panic!("not an int"),
            }
        }
        fn u64(&mut self) -> u64 {
            match self.0.next().copied() {
                Some(A::U(v)) => v,
                _ => panic!("not a 64-bit value"),
            }
        }
        fn ptr(&mut self) -> usize {
            match self.0.next().copied() {
                Some(A::P(v)) => v,
                _ => panic!("not a pointer"),
            }
        }
        fn str(&mut self) -> Option<&'s [u8]> {
            match self.0.next().copied() {
                Some(A::S(v)) => v,
                _ => panic!("not a string"),
            }
        }
    }

    /// No error texts: the `%m` cases bring their own.
    const ERRORS: Errors = Errors {
        describe: |_| None,
        name: |_| None,
    };

    /// `async_safe_format_buffer` into `size` bytes: (text kept, returned length).
    fn format(size: usize, fmt: &str, args: &[A]) -> (String, usize) {
        let mut buf = vec![0xffu8; size];
        let mut out = Buffer::new(&mut buf);
        vformat(&mut out, fmt.as_bytes(), &mut List(args.iter()), &ERRORS);
        let total = out.total;
        let text = String::from_utf8(out.text().to_vec()).unwrap();
        if size > 0 {
            assert_eq!(buf[text.len()], 0, "NUL-terminated");
        }
        (text, total)
    }

    fn f(fmt: &str, args: &[A]) -> String {
        format(1024, fmt, args).0
    }

    /// Expected strings: bionic's own tests/async_safe_test.cpp at the pin, its `async_safe_format_buffer`
    /// cases in order (`smoke`, the `%d` / `%ld` / `%lld` limits; LP64 values).
    #[test]
    fn as_bionic_tests() {
        use A::*;
        let s = |v: &'static str| S(Some(v.as_bytes()));
        assert_eq!(f("a", &[]), "a");
        assert_eq!(f("%%", &[]), "%");
        assert_eq!(f("01234", &[]), "01234");
        assert_eq!(f("a%sb", &[s("01234")]), "a01234b");
        assert_eq!(f("a%sb", &[S(None)]), "a(null)b");
        assert_eq!(f("aa%scc", &[s("bb")]), "aabbcc");
        assert_eq!(f("a%bb", &[I(1234)]), "a10011010010b");
        assert_eq!(f("a%#bb", &[I(1234)]), "a0b10011010010b");
        assert_eq!(f("a%#Bb", &[I(1234)]), "a0B10011010010b");
        assert_eq!(f("a%cc", &[I(b'b'.into())]), "abc");
        assert_eq!(f("a%db", &[I(1234)]), "a1234b");
        assert_eq!(f("a%db", &[I(-8123)]), "a-8123b");
        // static_cast<short>(0x7fff0010) / static_cast<char>(0x7fffff10), promoted to int
        assert_eq!(f("a%hdb", &[I(0x10)]), "a16b");
        assert_eq!(f("a%hhdb", &[I(0x10)]), "a16b");
        assert_eq!(f("a%lldb", &[U(0x1000000000)]), "a68719476736b");
        assert_eq!(f("a%ldb", &[U(70000)]), "a70000b");
        // EINVAL = 22; the message is glibc's here, bionic's there: the same text for EINVAL
        let einval = Errors {
            describe: |e| (e == 22).then_some(b"Invalid argument".as_slice()),
            name: |e| (e == 22).then_some(b"EINVAL".as_slice()),
        };
        let with = |fmt: &str, e: i32| {
            errno::set_errno(errno::Errno(e));
            let mut buf = [0u8; 64];
            let mut out = Buffer::new(&mut buf);
            vformat(&mut out, fmt.as_bytes(), &mut List([].iter()), &einval);
            String::from_utf8(out.text().to_vec()).unwrap()
        };
        assert_eq!(with("a%mZ", 22), "aInvalid argumentZ");
        assert_eq!(with("a%#mZ", 22), "aEINVALZ");
        assert_eq!(with("a%#mZ", -1), "a-1Z");
        assert_eq!(f("a%pb", &[P(0xb0001234)]), "a0xb0001234b");
        assert_eq!(f("a%xz", &[I(0x12ab)]), "a12abz");
        assert_eq!(f("a%#xz", &[I(0x12ab)]), "a0x12abz");
        assert_eq!(f("a%Xz", &[I(0x12ab)]), "a12ABz");
        assert_eq!(f("a%#Xz", &[I(0x12ab)]), "a0X12ABz");
        assert_eq!(f("a%08xz", &[I(0x123456)]), "a00123456z");
        assert_eq!(f("a%5dz", &[I(1234)]), "a 1234z");
        assert_eq!(f("a%05dz", &[I(1234)]), "a01234z");
        assert_eq!(f("a%8dz", &[I(1234)]), "a    1234z");
        assert_eq!(f("a%-8dz", &[I(1234)]), "a1234    z");
        assert_eq!(f("A%-11sZ", &[s("abcdef")]), "Aabcdef     Z");
        assert_eq!(f("A%s:%dZ", &[s("hello"), I(1234)]), "Ahello:1234Z");
        assert_eq!(f("a%03d:%d:%02dz", &[I(5), I(5), I(5)]), "a005:5:05z");
        assert_eq!(f("a%#xZ", &[I(34)]), "a0x22Z");
        assert_eq!(f("a%#xZ", &[I(0)]), "a0Z");
        assert_eq!(f("a%#5xZ", &[I(20)]), "a 0x14Z");
        assert_eq!(f("a%#oZ", &[I(777)]), "a01411Z");
        assert_eq!(f("a%#oZ", &[I(0)]), "a0Z");
        assert_eq!(f("a%#6oZ", &[I(15)]), "a   017Z");
        assert_eq!(f("a%d,%pz", &[I(5), P(0)]), "a5,0x0z");
        assert_eq!(
            f("a%lld,%d,%d,%dz", &[U(0x1000000000), I(6), I(7), I(8)]),
            "a68719476736,6,7,8z"
        );
        assert_eq!(f("%d", &[I(i32::MAX)]), "2147483647");
        assert_eq!(f("%d", &[I(i32::MIN)]), "-2147483648");
        assert_eq!(f("%ld", &[U(i64::MAX as u64)]), "9223372036854775807");
        assert_eq!(f("%ld", &[U(i64::MIN as u64)]), "-9223372036854775808");
        assert_eq!(f("%lld", &[U(i64::MAX as u64)]), "9223372036854775807");
        assert_eq!(f("%lld", &[U(i64::MIN as u64)]), "-9223372036854775808");
    }

    /// bionic's test has no case for these: expected values traced by hand through async_safe_log.cpp
    /// (`out_vformat`, `format_integer`, `format_unsigned`), the code the module comment cites.
    #[test]
    fn quirks_of_the_implementation() {
        use A::*;
        // `o` is in format_integer's is_signed: -8 -> '-' + octal 10
        assert_eq!(f("%o", &[I(-8)]), "-10");
        // the pad is sent before the whole text ("-42"), and after it when left-aligned
        assert_eq!(f("%05d", &[I(-42)]), "00-42");
        assert_eq!(f("%-05d|", &[I(7)]), "70000|");
        // `%c` of 0: slen = strlen = 0
        assert_eq!(f("[%c]", &[I(0)]), "[]");
        // buffer + 2 leaves 62 bytes, 61 digits: format_unsigned keeps the first 61 generated (low)
        assert_eq!(f("%#llb", &[U(u64::MAX)]), format!("0b{}", "1".repeat(61)));
        // a single trailing '%' is sent as is
        assert_eq!(f("a%", &[]), "a%");
        // bionic strerror_r's text for a value without a message: "Unknown error %d"
        errno::set_errno(errno::Errno(9999));
        assert_eq!(f("%m", &[]), "Unknown error 9999");
    }

    /// async_safe_test.cpp `async_safe_format_buffer.buffer_size`-style cases: the untruncated length
    /// returned, `size - 1` bytes kept and NUL-terminated, a zero-size buffer untouched.
    #[test]
    fn truncation() {
        use A::*;
        let world = [S(Some(b"world".as_slice()))];
        assert_eq!(format(1024, "hello %s", &world), ("hello world".into(), 11));
        assert_eq!(format(8, "hello %s", &world), ("hello w".into(), 11));
        assert_eq!(format(6, "hello %s", &world), ("hello".into(), 11));
        assert_eq!(format(0, "xxxx", &[]), (String::new(), 4));
        assert_eq!(format(1, "xxxx", &[]), (String::new(), 4));
        assert_eq!(format(2, "xxxx", &[]), ("x".into(), 4));
    }
}
