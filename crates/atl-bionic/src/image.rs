// SPDX-License-Identifier: MIT
//! Builds property-area files in bionic's format, as bionic's own writer lays them out
//! (`prop_area.cpp` `add` / `allocate_obj`, `prop_info.cpp`, `SystemProperties::Add`): the same
//! allocation order and alignment, so guest bionic and `props::Area` read the result. An image is
//! built whole, before any reader maps it; live updates are atld's (dirty-backup protocol), later.

use crate::generated::prop_format as fmt;
use crate::props::VALUE_MAX;
use fmt::{
    prop_area as area_l, prop_info as info_l, prop_info_anon0 as union_l,
    prop_info_anon0_long_property as long_l, prop_trie_node as node_l,
};

/// bionic `allocate_obj`: allocations are aligned to `sizeof(uint_least32_t)`.
const OBJ_ALIGN: usize = core::mem::size_of::<u32>();

/// Why a property was not added (bionic `SystemProperties::Add` / `prop_area::add` failures).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    EmptyName,
    /// A mutable property's value must fit `PROP_VALUE_MAX` with its NUL.
    ValueTooLong,
    /// A segment between dots is empty.
    EmptySegment,
    /// Already present; bionic's add keeps the old value, init updates instead.
    Exists,
    /// The area is full.
    Full,
}

/// A property area being built.
pub struct Image {
    bytes: Vec<u8>,
    /// bionic `bytes_used_`: data bytes allocated so far.
    used: usize,
}

impl Default for Image {
    fn default() -> Self {
        Self::new()
    }
}

impl Image {
    /// An empty area of bionic's size (`PA_SIZE`): header, root node, dirty backup.
    pub fn new() -> Self {
        let size = fmt::PA_SIZE as usize;
        let mut image = Image {
            bytes: vec![0; size],
            // the prop_area constructor: root node, then the dirty backup sized for a value
            used: node_l::SIZE + VALUE_MAX.next_multiple_of(OBJ_ALIGN),
        };
        image.put(area_l::magic_, fmt::PROP_AREA_MAGIC);
        image.put(area_l::version_, fmt::PROP_AREA_VERSION);
        image.sync_used();
        image
    }

    /// The file's bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn put(&mut self, off: usize, v: u32) {
        self.bytes[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn get(&self, off: usize) -> u32 {
        u32::from_ne_bytes([
            self.bytes[off],
            self.bytes[off + 1],
            self.bytes[off + 2],
            self.bytes[off + 3],
        ])
    }

    /// `s` and a NUL at `off`.
    fn put_c_str(&mut self, off: usize, s: &[u8]) {
        self.bytes[off..off + s.len()].copy_from_slice(s);
        self.bytes[off + s.len()] = 0;
    }

    fn sync_used(&mut self) {
        self.put(area_l::bytes_used_, self.used as u32);
    }

    /// bionic `allocate_obj`: a data offset for `size` bytes.
    fn allocate(&mut self, size: usize) -> Result<usize, AddError> {
        let aligned = size.next_multiple_of(OBJ_ALIGN);
        if self.used + aligned > self.bytes.len() - area_l::SIZE {
            return Err(AddError::Full);
        }
        let off = self.used;
        self.used += aligned;
        self.sync_used();
        Ok(off)
    }

    /// Absolute byte offset of a data offset.
    fn abs(rel: usize) -> usize {
        area_l::data_ + rel
    }

    /// bionic `new_prop_trie_node`.
    fn node(&mut self, name: &[u8]) -> Result<usize, AddError> {
        let off = self.allocate(node_l::SIZE + name.len() + 1)?;
        self.put(Self::abs(off) + node_l::namelen, name.len() as u32);
        self.put_c_str(Self::abs(off) + node_l::name, name);
        Ok(off)
    }

    /// The node `field` of `node` links to, allocating `name` there when none (a release store in
    /// bionic; here nobody reads yet).
    fn child(&mut self, node: usize, field: usize, name: &[u8]) -> Result<usize, AddError> {
        let link = Self::abs(node) + field;
        match self.get(link) {
            0 => {
                let new = self.node(name)?;
                self.put(link, new as u32);
                Ok(new)
            }
            rel => Ok(rel as usize),
        }
    }

    /// bionic `find_prop_trie_node` with allocation.
    fn sibling(&mut self, mut node: usize, segment: &[u8]) -> Result<usize, AddError> {
        loop {
            let at = Self::abs(node);
            let len = self.get(at + node_l::namelen) as usize;
            let name = &self.bytes[at + node_l::name..at + node_l::name + len];
            let field = match segment.len().cmp(&len).then_with(|| segment.cmp(name)) {
                core::cmp::Ordering::Equal => return Ok(node),
                core::cmp::Ordering::Less => node_l::left,
                core::cmp::Ordering::Greater => node_l::right,
            };
            node = self.child(node, field, segment)?;
        }
    }

    /// Adds `name=value` as bionic's `__system_property_add` does.
    pub fn add(&mut self, name: &str, value: &str) -> Result<(), AddError> {
        let (name, value) = (name.as_bytes(), value.as_bytes());
        if name.is_empty() {
            return Err(AddError::EmptyName);
        }
        let read_only = name.starts_with(b"ro.");
        if value.len() >= VALUE_MAX && !read_only {
            return Err(AddError::ValueTooLong);
        }
        // bionic `find_property` with allocation: one trie level per segment
        let mut node = 0;
        for segment in name.split(|&b| b == b'.') {
            if segment.is_empty() {
                return Err(AddError::EmptySegment);
            }
            let first = self.child(node, node_l::children, segment)?;
            node = self.sibling(first, segment)?;
        }
        let prop = Self::abs(node) + node_l::prop;
        if self.get(prop) != 0 {
            return Err(AddError::Exists);
        }
        let info = self.info(name, value)?;
        self.put(prop, info as u32);
        // `SystemProperties::Add`: the area serial counts additions
        let serial = self.get(area_l::serial_);
        self.put(area_l::serial_, serial.wrapping_add(1));
        Ok(())
    }

    /// bionic `new_prop_info` and the two `prop_info` constructors.
    fn info(&mut self, name: &[u8], value: &[u8]) -> Result<usize, AddError> {
        let off = self.allocate(info_l::SIZE + name.len() + 1)?;
        let at = Self::abs(off);
        self.put_c_str(at + info_l::name, name);
        let serial = info_l::serial;
        if value.len() >= VALUE_MAX {
            // the value goes after the record; the record holds its offset from itself and, for
            // readers that cannot follow it, an error message as value
            let long = self.allocate(value.len() + 1)?;
            self.put_c_str(Self::abs(long), value);
            let error = fmt::kLongLegacyError.as_bytes();
            self.put(
                at + serial,
                ((error.len() as u32) << 24) | fmt::prop_info_kLongFlag,
            );
            let message = at + info_l::anon0 + union_l::long_property + long_l::error_message;
            self.put_c_str(message, error);
            let offset = at + info_l::anon0 + union_l::long_property + long_l::offset;
            self.put(offset, (long - off) as u32);
        } else {
            self.put(at + serial, (value.len() as u32) << 24);
            self.put_c_str(at + info_l::anon0 + union_l::value, value);
        }
        Ok(off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::Area;
    use crate::props::tests::{hand_area, words};

    #[test]
    fn same_bytes_as_bionics_allocation() {
        let mut image = Image::new();
        image.add("ro.a", "x").unwrap();
        image.add("ro.b", "yy").unwrap();
        image.add("net.c", "").unwrap();
        assert!(image.into_bytes() == hand_area());
    }

    #[test]
    fn long_read_only_values() {
        let long = "v".repeat(200);
        let mut image = Image::new();
        image.add("ro.long", &long).unwrap();
        // a mutable property must fit the value buffer
        assert_eq!(image.add("sys.long", &long), Err(AddError::ValueTooLong));
        assert_eq!(image.add("ro.long", "x"), Err(AddError::Exists));
        assert_eq!(image.add("a..b", "x"), Err(AddError::EmptySegment));
        assert_eq!(image.add("", "x"), Err(AddError::EmptyName));
        let w = words(&image.into_bytes());
        let area = Area::new(&w).unwrap();
        let pi = area.find(b"ro.long").unwrap();
        // nodes "ro" (24) and "long" (20+4+1 -> 28) from data 112, the record at 164 (96+7+1 -> 104
        // bytes), the value allocated right after it
        let value = area.long_value(pi).unwrap();
        assert_eq!(value - pi.offset(), 104);
        assert_eq!(area.c_str_len(value), Some(200));
        // what legacy readers get: bionic's message, as the value
        let mut out = [0u8; VALUE_MAX];
        let serial = area.read(pi, &mut out);
        let msg = b"Must use __system_property_read_callback() to read";
        assert_eq!(&out[..crate::props::value_len(serial)], msg);
    }

    #[test]
    fn fills_up() {
        let mut image = Image::new();
        let mut n = 0;
        let err = loop {
            match image.add(&format!("p.{n}"), "v") {
                Ok(()) => n += 1,
                Err(e) => break e,
            }
        };
        assert_eq!(err, AddError::Full);
        assert!(n > 100, "{n}");
        // everything added before it filled up is readable
        let w = words(&image.into_bytes());
        let area = Area::new(&w).unwrap();
        assert!(area.find(format!("p.{}", n - 1).as_bytes()).is_some());
    }
}
