// SPDX-License-Identifier: MIT
//! Reader of bionic's property-area format (`bionic/libc/system_properties/prop_area.cpp`,
//! `system_properties.cpp`), over the area's words. Every offset comes from the generated layout.
//!
//! Format: a header (`prop_area`), then data: the root trie node at data offset 0, the dirty-backup
//! value buffer after it, then trie nodes, `prop_info` records and long values in allocation order.
//! A name is split at `.`; each segment is a trie node, siblings form a binary tree ordered by length,
//! then bytes; `children` is the next level, `prop` the record of the full name. Links are data
//! offsets, 0 = none. The writer publishes a link with a release store after filling what it names.
//!
//! Everything is read through atomics: the writer (atld, or bionic's init on Android) changes the area
//! while it is mapped. A corrupt area yields "not found", never a panic or an unbounded walk.

use core::cmp::Ordering;
use core::sync::atomic::Ordering::{Acquire, Relaxed};
use core::sync::atomic::{AtomicU32, fence};

use rustix::thread::futex;

use crate::generated::prop_format as fmt;
use fmt::{
    prop_area as area_l, prop_info as info_l, prop_info_anon0 as union_l,
    prop_info_anon0_long_property as long_l, prop_trie_node as node_l,
};

/// Value buffer size of a non-long property, its NUL included.
pub const VALUE_MAX: usize = fmt::PROP_VALUE_MAX as usize;
/// bionic `SERIAL_VALUE_LEN`: the value's length is the serial's top byte.
const SERIAL_LEN_SHIFT: u32 = 24;
/// bionic `SERIAL_DIRTY`: set while the writer replaces the value; readers copy the dirty backup.
pub(crate) const SERIAL_DIRTY: u32 = 1;
/// A `prop_info` holds a long (read-only) value elsewhere; its `value` holds `kLongLegacyError`.
const LONG_FLAG: u32 = fmt::prop_info_kLongFlag;
/// The `value` buffer inside a record.
const VALUE: usize = info_l::anon0 + union_l::value;
/// The long value's offset, relative to the record.
const LONG_OFFSET: usize = info_l::anon0 + union_l::long_property + long_l::offset;
/// bionic `prop_area::dirty_backup_area()`: right after the root node.
const DIRTY_BACKUP: usize = area_l::data_ + node_l::SIZE;
const WORD: usize = core::mem::size_of::<AtomicU32>();

/// A property record, as a byte offset from the area's start; its whole record lies inside the area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropInfo(usize);

impl PropInfo {
    /// Byte offset of the record from the area's start.
    pub fn offset(self) -> usize {
        self.0
    }

    /// Byte offset of its NUL-terminated name.
    pub fn name_offset(self) -> usize {
        self.0 + info_l::name
    }

    /// Byte offset of its (short) value buffer.
    pub fn value_offset(self) -> usize {
        self.0 + VALUE
    }
}

/// Why a file is not a property area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// Smaller than the header (bytes).
    TooSmall(usize),
    Magic(u32),
    Version(u32),
}

/// A link field pointing outside the area.
struct Corrupt;

/// A property area: `words` is the whole file.
#[derive(Clone, Copy)]
pub struct Area<'a> {
    words: &'a [AtomicU32],
}

impl<'a> Area<'a> {
    /// The area in `words`, after bionic's checks (`prop_area::map_fd_ro`): big enough for the header,
    /// bionic's magic and version.
    pub fn new(words: &'a [AtomicU32]) -> Result<Self, Invalid> {
        let area = Area { words };
        if area.len() < area_l::SIZE {
            return Err(Invalid::TooSmall(area.len()));
        }
        let magic = area.load(area_l::magic_, Relaxed).unwrap_or_default();
        if magic != fmt::PROP_AREA_MAGIC {
            return Err(Invalid::Magic(magic));
        }
        let version = area.load(area_l::version_, Relaxed).unwrap_or_default();
        if version != fmt::PROP_AREA_VERSION {
            return Err(Invalid::Version(version));
        }
        Ok(area)
    }

    /// Size in bytes.
    pub fn len(&self) -> usize {
        self.words.len() * WORD
    }

    /// Never true for a valid area (the header is not empty).
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn word(&self, off: usize) -> Option<&'a AtomicU32> {
        if !off.is_multiple_of(WORD) {
            return None;
        }
        self.words.get(off / WORD)
    }

    fn load(&self, off: usize, order: core::sync::atomic::Ordering) -> Option<u32> {
        Some(self.word(off)?.load(order))
    }

    /// The byte at `off`; the writer stores bytes into words this reader loads whole.
    pub fn byte(&self, off: usize) -> Option<u8> {
        let w = self.words.get(off / WORD)?.load(Relaxed);
        Some(w.to_ne_bytes()[off % WORD])
    }

    /// Length of the NUL-terminated string at `off`, if its NUL lies inside the area.
    pub fn c_str_len(&self, off: usize) -> Option<usize> {
        (off..self.len()).position(|i| self.byte(i) == Some(0))
    }

    /// The area serial: bumped by the writer after every addition (and update), futex-woken.
    pub fn serial(&self) -> &'a AtomicU32 {
        // `new` checked the header is inside the area
        &self.words[area_l::serial_ / WORD]
    }

    /// Follows a link field: `Ok(None)` for none; bionic `to_prop_obj` refuses offsets past the data.
    /// Acquire pairs with the writer's release store of the link (bionic loads it `consume`).
    fn link(&self, field: usize) -> Result<Option<usize>, Corrupt> {
        let rel = self.load(field, Acquire).ok_or(Corrupt)?;
        if rel == 0 {
            return Ok(None);
        }
        let off = area_l::data_ + rel as usize;
        if off >= self.len() {
            return Err(Corrupt);
        }
        Ok(Some(off))
    }

    /// The trie nodes and records a well-formed area can hold: a walk longer than this is a cycle.
    fn budget(&self) -> usize {
        self.len() / node_l::SIZE + 1
    }

    /// bionic `cmp_prop_name(segment, node)`: shorter names first, then bytes.
    fn cmp_name(&self, segment: &[u8], node: usize) -> Option<Ordering> {
        let len = self.load(node + node_l::namelen, Relaxed)? as usize;
        let name = node + node_l::name;
        Some(segment.len().cmp(&len).then_with(|| {
            segment
                .iter()
                .enumerate()
                .map(|(i, b)| b.cmp(&self.byte(name + i).unwrap_or(0)))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        }))
    }

    /// bionic `find_prop_trie_node` without allocation: the sibling named `segment`.
    fn find_sibling(&self, mut node: usize, segment: &[u8], budget: &mut usize) -> Option<usize> {
        loop {
            *budget = budget.checked_sub(1)?;
            node = match self.cmp_name(segment, node)? {
                Ordering::Equal => return Some(node),
                Ordering::Less => self.link(node + node_l::left).ok()??,
                Ordering::Greater => self.link(node + node_l::right).ok()??,
            };
        }
    }

    /// The record of `name` (bionic `prop_area::find`).
    pub fn find(&self, name: &[u8]) -> Option<PropInfo> {
        let mut budget = self.budget();
        let mut node = area_l::data_;
        for segment in name.split(|&b| b == b'.') {
            if segment.is_empty() {
                return None;
            }
            let first = self.link(node + node_l::children).ok()??;
            node = self.find_sibling(first, segment, &mut budget)?;
        }
        self.info(self.link(node + node_l::prop).ok()??)
    }

    /// Every record, in bionic's order (left siblings, the node's own record, its children, right
    /// siblings); false if the walk met a corrupt link (bionic `foreach_property`).
    /// Iterative, so a deep or cyclic corrupt area cannot overflow the stack; links are loaded in the
    /// order bionic's recursion loads them.
    pub fn for_each(&self, f: &mut dyn FnMut(PropInfo)) -> bool {
        enum Step {
            Enter(usize),
            Prop(usize),
            Children(usize),
            Right(usize),
        }
        let mut budget = self.budget();
        let mut todo = vec![Step::Enter(area_l::data_)];
        while let Some(step) = todo.pop() {
            let (node, field, then) = match step {
                Step::Enter(n) => {
                    let Some(left) = budget.checked_sub(1) else {
                        return false;
                    };
                    budget = left;
                    (n, node_l::left, Step::Prop(n))
                }
                Step::Prop(n) => {
                    match self.link(n + node_l::prop) {
                        Ok(Some(p)) => match self.info(p) {
                            Some(pi) => f(pi),
                            None => return false,
                        },
                        Ok(None) => {}
                        Err(Corrupt) => return false,
                    }
                    todo.push(Step::Children(n));
                    continue;
                }
                Step::Children(n) => (n, node_l::children, Step::Right(n)),
                Step::Right(n) => match self.link(n + node_l::right) {
                    Ok(Some(r)) => {
                        todo.push(Step::Enter(r));
                        continue;
                    }
                    Ok(None) => continue,
                    Err(Corrupt) => return false,
                },
            };
            todo.push(then);
            match self.link(node + field) {
                Ok(Some(next)) => todo.push(Step::Enter(next)),
                Ok(None) => {}
                Err(Corrupt) => return false,
            }
        }
        true
    }

    /// The record at byte offset `off`, if a whole record fits there.
    pub fn info(&self, off: usize) -> Option<PropInfo> {
        (off >= area_l::data_ && off.is_multiple_of(WORD) && off + info_l::SIZE <= self.len())
            .then_some(PropInfo(off))
    }

    /// The record's serial word.
    pub fn info_serial(&self, pi: PropInfo) -> &'a AtomicU32 {
        // `info` checked the whole record is inside the area
        &self.words[(pi.0 + info_l::serial) / WORD]
    }

    /// bionic `is_read_only`: the name starts with `ro.`; such values never change.
    pub fn is_read_only(&self, pi: PropInfo) -> bool {
        let name = pi.name_offset();
        b"ro."
            .iter()
            .enumerate()
            .all(|(i, b)| self.byte(name + i) == Some(*b))
    }

    /// The long value's byte offset, for a record holding one (`prop_info::long_value`).
    pub fn long_value(&self, pi: PropInfo) -> Option<usize> {
        if self.info_serial(pi).load(Relaxed) & LONG_FLAG == 0 {
            return None;
        }
        let rel = self.load(pi.0 + LONG_OFFSET, Relaxed)? as usize;
        let off = pi.0.checked_add(rel)?;
        (off < self.len()).then_some(off)
    }

    /// The value and the serial it belongs to, consistent with each other: bionic
    /// `ReadMutablePropertyValue`. `out` receives the value and its NUL; the value's length is
    /// `value_len(serial)`.
    pub fn read(&self, pi: PropInfo, out: &mut [u8; VALUE_MAX]) -> u32 {
        let word = self.info_serial(pi);
        let mut new = word.load(Acquire);
        loop {
            let serial = new;
            let src = if serial & SERIAL_DIRTY != 0 {
                DIRTY_BACKUP
            } else {
                pi.value_offset()
            };
            for (i, b) in out[..=value_len(serial)].iter_mut().enumerate() {
                *b = self.byte(src + i).unwrap_or_default();
            }
            fence(Acquire);
            new = word.load(Relaxed);
            if serial == new {
                return serial;
            }
            fence(Acquire);
        }
    }

    /// What bionic's `ReadCallback` hands its callback: a read-only property's name and value in place
    /// (they never change), else a consistent copy of the value. None when a string in the area lacks
    /// its NUL (a pointer to it would let C read past the area).
    pub fn snapshot(&self, pi: PropInfo) -> Option<Snapshot> {
        let name = pi.name_offset();
        self.c_str_len(name)?;
        if self.is_read_only(pi) {
            let value = self.long_value(pi).unwrap_or(pi.value_offset());
            self.c_str_len(value)?;
            return Some(Snapshot {
                name,
                value: Value::InArea(value),
                serial: self.info_serial(pi).load(Relaxed),
            });
        }
        let mut copy = [0u8; VALUE_MAX];
        let serial = self.read(pi, &mut copy);
        Some(Snapshot {
            name,
            value: Value::Copied(copy),
            serial,
        })
    }

    /// C `strlcpy` from the area: as much of the string at `off` as fits `out` with a NUL; the
    /// string's length.
    pub fn strlcpy(&self, off: usize, out: &mut [u8]) -> usize {
        let len = self.c_str_len(off).unwrap_or_default();
        if let Some(room) = out.len().checked_sub(1) {
            let n = len.min(room);
            for (i, b) in out[..n].iter_mut().enumerate() {
                *b = self.byte(off + i).unwrap_or_default();
            }
            out[n] = 0;
        }
        len
    }

    /// Address of byte `off` of the area, for C callers; `info_at` maps it back.
    pub fn ptr(&self, off: usize) -> *const u8 {
        self.words.as_ptr().cast::<u8>().wrapping_add(off)
    }

    /// The record at an address `ptr` handed out, if it is one.
    pub fn info_at(&self, ptr: *const u8) -> Option<PropInfo> {
        self.info((ptr as usize).checked_sub(self.ptr(0) as usize)?)
    }

    /// The record's serial once no update is in progress (bionic `Serial`: futex-waits out a dirty one).
    pub fn stable_serial(&self, pi: PropInfo) -> u32 {
        let word = self.info_serial(pi);
        let mut serial = word.load(Acquire);
        while serial & SERIAL_DIRTY != 0 {
            // shared, not private: the writer is another process; a changed value returns at once
            let _ = futex::wait(word, futex::Flags::empty(), serial, None);
            serial = word.load(Acquire);
        }
        serial
    }
}

/// A property as a read callback sees it; offsets are into the area.
pub struct Snapshot {
    pub name: usize,
    pub value: Value,
    pub serial: u32,
}

pub enum Value {
    /// NUL-terminated at this offset of the area.
    InArea(usize),
    /// NUL-terminated copy.
    Copied([u8; VALUE_MAX]),
}

/// The value length a serial carries, capped to what fits the value buffer with its NUL (a corrupt
/// serial must not overrun the caller's `PROP_VALUE_MAX` buffer).
pub fn value_len(serial: u32) -> usize {
    ((serial >> SERIAL_LEN_SHIFT) as usize).min(VALUE_MAX - 1)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Bytes -> the words an area is read through.
    pub(crate) fn words(bytes: &[u8]) -> Vec<AtomicU32> {
        bytes
            .as_chunks::<WORD>()
            .0
            .iter()
            .map(|c| AtomicU32::new(u32::from_ne_bytes(*c)))
            .collect()
    }

    fn put(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn put_str(b: &mut [u8], off: usize, s: &str) {
        b[off..off + s.len()].copy_from_slice(s.as_bytes());
        b[off + s.len()] = 0;
    }

    /// Header (128 bytes: 4 words + 28 reserved, `prop_area`), data offsets relative to it.
    const DATA: usize = 128;
    /// `prop_trie_node`: namelen, prop, left, right, children (5 words), then the name.
    const NODE: usize = 20;
    /// `prop_info`: serial, then a 92-byte value union, then the name (bionic static_asserts 96).
    const INFO: usize = 96;

    /// An area written by hand from bionic's allocation rules (`prop_area.cpp`), not by `image`:
    /// `ro.a=x`, `ro.b=yy`, `net.c=` added in that order to a fresh 128 KiB area.
    /// Allocations are 4-aligned; the root node (20) and the dirty backup (92) come first, so the first
    /// allocation is at data 112:
    ///   "ro"  node 20+2+1=23 -> 24 at 112, root.children = 112
    ///   "a"   node 24 at 136, ro.children = 136;  ro.a record 96+4+1=101 -> 104 at 160, a.prop = 160
    ///   "b"   node 24 at 264, a.right = 264 ("b" > "a");  ro.b record 104 at 288, b.prop = 288
    ///   "net" node 24 at 392, ro.right = 392 (longer than "ro");  "c" node 24 at 416, net.children = 416
    ///   net.c record 96+5+1=102 -> 104 at 440, c.prop = 440; bytes used 544; area serial 3 (one per add)
    pub(crate) fn hand_area() -> Vec<u8> {
        let mut b = vec![0u8; 128 * 1024];
        put(&mut b, 0, 544);
        put(&mut b, 4, 3);
        put(&mut b, 8, 0x504f_5250);
        put(&mut b, 12, 0xfc6e_d0ab);
        let node = |b: &mut Vec<u8>, at: usize, name: &str| {
            put(b, DATA + at, name.len() as u32);
            put_str(b, DATA + at + NODE, name);
        };
        let info = |b: &mut Vec<u8>, at: usize, name: &str, value: &str| {
            put(b, DATA + at, (value.len() as u32) << 24);
            put_str(b, DATA + at + 4, value);
            put_str(b, DATA + at + INFO, name);
        };
        put(&mut b, DATA + 16, 112);
        node(&mut b, 112, "ro");
        put(&mut b, DATA + 112 + 16, 136);
        node(&mut b, 136, "a");
        info(&mut b, 160, "ro.a", "x");
        put(&mut b, DATA + 136 + 4, 160);
        node(&mut b, 264, "b");
        put(&mut b, DATA + 136 + 12, 264);
        info(&mut b, 288, "ro.b", "yy");
        put(&mut b, DATA + 264 + 4, 288);
        node(&mut b, 392, "net");
        put(&mut b, DATA + 112 + 12, 392);
        node(&mut b, 416, "c");
        put(&mut b, DATA + 392 + 16, 416);
        info(&mut b, 440, "net.c", "");
        put(&mut b, DATA + 416 + 4, 440);
        b
    }

    fn value(area: &Area, pi: PropInfo) -> (u32, Vec<u8>) {
        let mut out = [0xffu8; VALUE_MAX];
        let serial = area.read(pi, &mut out);
        (serial, out[..value_len(serial)].to_vec())
    }

    #[test]
    fn layout_is_bionics() {
        // the hand-written constants above are what clang computed from bionic's headers
        assert_eq!(
            (area_l::SIZE, node_l::SIZE, info_l::SIZE),
            (DATA, NODE, INFO)
        );
        assert_eq!(VALUE, 4);
        assert_eq!(VALUE_MAX, 92);
    }

    #[test]
    fn reads_a_hand_written_area() {
        let w = words(&hand_area());
        let area = Area::new(&w).unwrap();
        assert_eq!(area.serial().load(Relaxed), 3);
        let a = area.find(b"ro.a").unwrap();
        assert_eq!(a.offset(), DATA + 160);
        assert_eq!(value(&area, a), (1 << 24, b"x".to_vec()));
        assert_eq!(value(&area, area.find(b"ro.b").unwrap()).1, b"yy");
        assert_eq!(value(&area, area.find(b"net.c").unwrap()), (0, Vec::new()));
        assert!(area.is_read_only(a));
        assert!(!area.is_read_only(area.find(b"net.c").unwrap()));
        for absent in [
            &b"ro"[..],
            b"ro.c",
            b"net",
            b"ro.a.b",
            b"",
            b"ro.",
            b".ro.a",
            b"ro..a",
        ] {
            assert_eq!(area.find(absent), None, "{absent:?}");
        }
        let mut seen = Vec::new();
        assert!(area.for_each(&mut |pi| seen.push(pi.offset() - DATA)));
        assert_eq!(seen, [160, 288, 440]);
    }

    #[test]
    fn dirty_value_comes_from_the_backup() {
        let mut b = hand_area();
        // the writer mid-update: old value "x" copied to the backup, serial marked dirty
        put_str(&mut b, DATA + NODE, "x");
        put(&mut b, DATA + 160, (1 << 24) | SERIAL_DIRTY);
        put_str(&mut b, DATA + 160 + 4, "?");
        let w = words(&b);
        let area = Area::new(&w).unwrap();
        let (serial, v) = value(&area, area.find(b"ro.a").unwrap());
        assert_eq!((serial & SERIAL_DIRTY, v), (SERIAL_DIRTY, b"x".to_vec()));
    }

    #[test]
    fn rejects_foreign_and_corrupt_files() {
        let small = words(&[0u8; 64]);
        assert_eq!(Area::new(&small).err(), Some(Invalid::TooSmall(64)));
        let mut b = hand_area();
        put(&mut b, 8, 0);
        assert_eq!(Area::new(&words(&b)).err(), Some(Invalid::Magic(0)));
        // a sibling cycle (a.right -> a) ends the walk instead of looping
        let mut b = hand_area();
        put(&mut b, DATA + 136 + 12, 136);
        let w = words(&b);
        let area = Area::new(&w).unwrap();
        assert_eq!(area.find(b"ro.z"), None);
        assert!(!area.for_each(&mut |_| {}));
        // a link past the end
        let mut b = hand_area();
        put(&mut b, DATA + 16, 0x7fff_ffff);
        let w = words(&b);
        let area = Area::new(&w).unwrap();
        assert_eq!(area.find(b"ro.a"), None);
        assert!(!area.for_each(&mut |_| {}));
    }
}
