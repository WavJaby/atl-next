// SPDX-License-Identifier: MIT
//! ILP32 record and word converters (phase4-32bit P4b s3): one interpreter over the field tables bridgegen derives from the guest and host views (`atl_bridgegen::ilp32::converters`, emitted per ILP32 table as `C_<record>` consts, each const-asserted [`Conv::ordered`] on both sides). No offset is written by hand: a table is the upstream header's layout as clang computed it per target.
//! Guest memory is host-addressable at the guest's own addresses (QEMU user mode, guest base 0; a 32-bit guest lives below 4 GiB), so a guest pointer zero-extended is a host pointer; a host pointer at or above 4 GiB cannot reach a 32-bit guest and is refused (written as NULL, counted in dispatch.rs `ILP32_REFUSED`).

use std::sync::atomic::Ordering;

/// How one field crosses (atl_bridgegen `ilp32::Op`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// the same bytes on both sides
    Copy(usize),
    /// an integer of `guest` / `host` bytes: widened by signedness to the host, truncated to the guest
    Int { guest: usize, host: usize, signed: bool },
    /// a data pointer: zero-extended to the host, refused at or above 4 GiB to a 32-bit guest
    Ptr { guest: usize, host: usize },
    /// a nested record by value
    Record(&'static Conv),
    /// `len` elements, `guest` / `host` bytes apart
    Array { len: usize, elem: &'static Op, guest: usize, host: usize },
}

impl Op {
    /// Bytes on the guest (`guest`) or host side.
    pub const fn bytes(&self, guest: bool) -> usize {
        match *self {
            Op::Copy(n) => n,
            Op::Int { guest: g, host: h, .. } | Op::Ptr { guest: g, host: h } => {
                if guest {
                    g
                } else {
                    h
                }
            }
            Op::Record(c) => {
                if guest {
                    c.guest_size
                } else {
                    c.host_size
                }
            }
            Op::Array { len, guest: g, host: h, .. } => len * if guest { g } else { h },
        }
    }
}

/// One field: offsets and alignments on both sides (the views'), and its op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub guest_offset: usize,
    pub host_offset: usize,
    pub guest_align: usize,
    pub host_align: usize,
    pub op: Op,
}

/// A record's converter: sizes and fields in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conv {
    pub name: &'static str,
    pub guest_size: usize,
    pub host_size: usize,
    pub fields: &'static [Field],
}

impl Conv {
    /// On one side (`guest` or host): every field at its alignment, after the end of the one before it (C11 6.7.2.1p15: members sit at increasing addresses in declaration order), and the last ending within the size. A generated table with two offsets swapped, or one hand-edited, fails this at build time (the generated const asserts).
    pub const fn ordered(&self, guest: bool) -> bool {
        let mut end = 0;
        let mut i = 0;
        while i < self.fields.len() {
            let f = &self.fields[i];
            let (off, align) = if guest {
                (f.guest_offset, f.guest_align)
            } else {
                (f.host_offset, f.host_align)
            };
            if align == 0 || off % align != 0 || off < end {
                return false;
            }
            end = off + f.op.bytes(guest);
            i += 1;
        }
        end <= if guest { self.guest_size } else { self.host_size }
    }
}

/// The word behind an out-word pointer (atl_bridgegen `ilp32::Word`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    Int { guest: usize, host: usize, signed: bool },
    Ptr,
}

impl Word {
    /// As a field op of a 32-bit guest's word on a 64-bit host.
    const fn op(self) -> Op {
        match self {
            Word::Int { guest, host, signed } => Op::Int { guest, host, signed },
            Word::Ptr => Op::Ptr { guest: 4, host: 8 },
        }
    }

    /// Host bytes of the temporary.
    pub const fn host_bytes(self) -> usize {
        self.op().bytes(false)
    }
}

/// Little-endian unsigned read of `n` bytes.
///
/// # Safety
/// `p` readable for `n` (<= 8) bytes.
unsafe fn load(p: *const u8, n: usize) -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: caller contract; `b` holds 8.
    unsafe { std::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), n) };
    u64::from_le_bytes(b)
}

/// Little-endian write of the low `n` bytes of `v`.
///
/// # Safety
/// `p` writable for `n` (<= 8) bytes.
unsafe fn store(p: *mut u8, n: usize, v: u64) {
    // SAFETY: caller contract.
    unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p, n) };
}

/// Field `op` from guest bytes `g` to host bytes `h`.
///
/// # Safety
/// `g` readable for `op.bytes(true)`, `h` writable for `op.bytes(false)`.
unsafe fn op_to_host(op: &Op, g: *const u8, h: *mut u8) {
    // SAFETY (whole body): every access is within the op's sizes, per the caller contract.
    unsafe {
        match *op {
            Op::Copy(n) => std::ptr::copy_nonoverlapping(g, h, n),
            Op::Int { guest, host, signed } => {
                let v = load(g, guest);
                let shift = 64 - 8 * guest as u32;
                let v = if signed { ((v << shift) as i64 >> shift) as u64 } else { v };
                store(h, host, v);
            }
            Op::Ptr { guest, host } => store(h, host, load(g, guest)),
            Op::Record(c) => to_host(c, g, h),
            Op::Array { len, elem, guest, host } => {
                for k in 0..len {
                    op_to_host(elem, g.add(k * guest), h.add(k * host));
                }
            }
        }
    }
}

/// Field `op` from host bytes `h` back to guest bytes `g`; false when a pointer was refused (written NULL).
///
/// # Safety
/// As [`op_to_host`], the directions swapped.
unsafe fn op_to_guest(op: &Op, h: *const u8, g: *mut u8) -> bool {
    // SAFETY (whole body): every access is within the op's sizes, per the caller contract.
    unsafe {
        match *op {
            Op::Copy(n) => {
                std::ptr::copy_nonoverlapping(h, g, n);
                true
            }
            Op::Int { guest, host, .. } => {
                store(g, guest, load(h, host));
                true
            }
            Op::Ptr { guest, host } => {
                let v = load(h, host);
                let fits = guest >= 8 || v >> (8 * guest) == 0;
                store(g, guest, if fits { v } else { 0 });
                if !fits {
                    super::dispatch::ILP32_REFUSED.fetch_add(1, Ordering::Relaxed);
                }
                fits
            }
            Op::Record(c) => to_guest(c, h, g),
            Op::Array { len, elem, guest, host } => {
                let mut ok = true;
                for k in 0..len {
                    ok &= op_to_guest(elem, h.add(k * host), g.add(k * guest));
                }
                ok
            }
        }
    }
}

/// Record `c` from the guest's layout at `g` into the host's at `h`.
///
/// # Safety
/// `g` readable for `c.guest_size`, `h` writable for `c.host_size`.
pub unsafe fn to_host(c: &Conv, g: *const u8, h: *mut u8) {
    for f in c.fields {
        // SAFETY: the offsets lie within the sizes (`Conv::ordered`, asserted at build time).
        unsafe { op_to_host(&f.op, g.add(f.guest_offset), h.add(f.host_offset)) };
    }
}

/// Record `c` from the host's layout at `h` into the guest's at `g`; false when a pointer field was refused (that field written NULL, the rest converted).
///
/// # Safety
/// `h` readable for `c.host_size`, `g` writable for `c.guest_size`.
pub unsafe fn to_guest(c: &Conv, h: *const u8, g: *mut u8) -> bool {
    let mut ok = true;
    for f in c.fields {
        // SAFETY: as [`to_host`].
        ok &= unsafe { op_to_guest(&f.op, h.add(f.host_offset), g.add(f.guest_offset)) };
    }
    ok
}

/// A host temporary for one converted lane: 8-aligned, the host side's bytes.
pub struct Temp(Vec<u64>);

impl Temp {
    fn new(bytes: usize) -> Self {
        Temp(vec![0; bytes.div_ceil(8).max(1)])
    }

    /// The host pointer the call receives.
    pub fn ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr().cast()
    }
}

/// The host temporary for guest pointer `guest` to record `c`, converted in.
///
/// # Safety
/// `guest` is a guest pointer to a record of the guest's layout, readable for `c.guest_size`.
pub unsafe fn record_in(c: &Conv, guest: usize) -> Temp {
    let mut t = Temp::new(c.host_size);
    // SAFETY: caller contract; `t` holds `host_size`.
    unsafe { to_host(c, guest as *const u8, t.ptr()) };
    t
}

/// The host temporary for guest pointer `guest` to one word `w`, read widened.
///
/// # Safety
/// `guest` readable for the guest word's bytes.
pub unsafe fn word_in(w: Word, guest: usize) -> Temp {
    let mut t = Temp::new(w.host_bytes());
    // SAFETY: caller contract; `t` holds the host word.
    unsafe { op_to_host(&w.op(), guest as *const u8, t.ptr()) };
    t
}

/// The word in `t` written back narrowed to guest pointer `guest`; false when a host pointer was refused.
///
/// # Safety
/// `guest` writable for the guest word's bytes.
pub unsafe fn word_out(w: Word, t: &mut Temp, guest: usize) -> bool {
    // SAFETY: caller contract; `t` holds the host word.
    unsafe { op_to_guest(&w.op(), t.ptr(), guest as *mut u8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ANativeWindow_Buffer` (nativewindow native_window.h: int32 width, height, stride, format; void* bits; uint32 reserved[6]) laid out by hand from the AAPCS32 and SysV x86_64 data models: guest offsets {0,4,8,12,16,20} size 44, host {0,4,8,12,16,24} size 48 -- the brief's numbers, independent of bridgegen.
    const WINDOW_BUFFER: Conv = Conv {
        name: "ANativeWindow_Buffer",
        guest_size: 44,
        host_size: 48,
        fields: &[
            f("width", 0, 0, 4, 4, Op::Copy(4)),
            f("height", 4, 4, 4, 4, Op::Copy(4)),
            f("stride", 8, 8, 4, 4, Op::Copy(4)),
            f("format", 12, 12, 4, 4, Op::Copy(4)),
            f("bits", 16, 16, 4, 8, Op::Ptr { guest: 4, host: 8 }),
            f("reserved", 20, 24, 4, 4, Op::Copy(24)),
        ],
    };

    const fn f(name: &'static str, g: usize, h: usize, ga: usize, ha: usize, op: Op) -> Field {
        Field {
            name,
            guest_offset: g,
            host_offset: h,
            guest_align: ga,
            host_align: ha,
            op,
        }
    }

    /// The arm table's generated converter is the hand layout above: the classifier and the data models agree.
    #[test]
    fn generated_window_buffer_is_the_data_model() {
        assert_eq!(super::super::bindings::qemu_arm::C_ANativeWindow_Buffer, WINDOW_BUFFER);
    }

    #[test]
    fn ordered_rejects_swapped_offsets() {
        assert!(WINDOW_BUFFER.ordered(true) && WINDOW_BUFFER.ordered(false));
        const SWAPPED: Conv = Conv {
            fields: &[
                f("width", 4, 0, 4, 4, Op::Copy(4)),
                f("height", 0, 4, 4, 4, Op::Copy(4)),
            ],
            ..WINDOW_BUFFER
        };
        assert!(!SWAPPED.ordered(true));
    }

    /// Guest bytes -> host -> guest: values survive; the pointer is zero-extended; a host pointer above 4 GiB comes back NULL and refused.
    #[test]
    fn window_buffer_round_trip() {
        let mut g = [0u8; 44];
        for (k, v) in [64u32, 32, 80, 1, 0x1234_5678].iter().enumerate() {
            g[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
        }
        g[40..44].copy_from_slice(&7u32.to_le_bytes());
        // SAFETY: `g` is 44 bytes, the guest size.
        let mut t = unsafe { record_in(&WINDOW_BUFFER, g.as_ptr() as usize) };
        let h = t.ptr();
        // SAFETY: `t` holds 48 bytes.
        unsafe {
            assert_eq!(load(h, 4), 64);
            assert_eq!(load(h.add(12), 4), 1);
            assert_eq!(load(h.add(16), 8), 0x1234_5678);
            assert_eq!(load(h.add(44), 4), 7);
        }
        let mut back = [0u8; 44];
        // SAFETY: sizes as above.
        assert!(unsafe { to_guest(&WINDOW_BUFFER, h, back.as_mut_ptr()) });
        assert_eq!(back, g);
        // SAFETY: as above.
        unsafe { store(h.add(16), 8, 0x7f00_0000_1000) };
        // SAFETY: as above.
        assert!(!unsafe { to_guest(&WINDOW_BUFFER, h, back.as_mut_ptr()) });
        assert_eq!(&back[16..20], &[0; 4]);
    }

    /// An out word: `size_t` 4 -> 8 unsigned, `long` sign-extended, written back truncated.
    #[test]
    fn out_word_widens_by_signedness() {
        let g = (-2i32).to_le_bytes();
        let signed = Word::Int { guest: 4, host: 8, signed: true };
        // SAFETY: `g` is one 4-byte word.
        let mut t = unsafe { word_in(signed, g.as_ptr() as usize) };
        // SAFETY: the temp holds 8.
        assert_eq!(unsafe { load(t.ptr(), 8) }, (-2i64) as u64);
        let unsigned = Word::Int { guest: 4, host: 8, signed: false };
        // SAFETY: as above.
        let t2 = unsafe { word_in(unsigned, g.as_ptr() as usize) };
        assert_eq!(t2.0[0], 0xFFFF_FFFE);
        let mut out = [0u8; 4];
        // SAFETY: the temp holds 8; `out` 4.
        unsafe { store(t.ptr(), 8, 0x1_0000_0005) };
        // SAFETY: as above.
        assert!(unsafe { word_out(signed, &mut t, out.as_mut_ptr() as usize) });
        assert_eq!(u32::from_le_bytes(out), 5);
    }
}
