// SPDX-License-Identifier: MIT
//! The byte-level half of turning a libbinder Parcel into the core's [`WireParcel`](atl_compositor::parcel::WireParcel): finding the objects in the data, and naming a binder object by a key.
//! libbinder does not export its object table (`Parcel::ipcObjects` is private), only the data and the object count (patches/aosp/frameworks/native, `AParcel_getDataWithObjects`), so the offsets are found by their type words, as
//! atl-parcel-truth's compositor.rs does for the same parcels, and checked against the count.
//! Binder objects are renamed to canonical keys ([`set_binder_key`]): the object a process sends is the address of its own `IBinder`, and the core only needs a stable name for it (`BinderKey::Local`).

use atl_compositor::parcel::{
    BINDER_TYPE_BINDER, BINDER_TYPE_FD, BINDER_TYPE_HANDLE, FLAT_OBJECT_SIZE, FlatObject,
    StrongBinder,
};

/// The object scan disagreed with libbinder's own count: a data word looks like an object, or an object was missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    /// Objects the scan found.
    pub found: usize,
    /// Objects libbinder says the parcel holds.
    pub expected: usize,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "object scan found {} objects, libbinder says {}",
            self.found, self.expected
        )
    }
}

impl std::error::Error for ScanError {}

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn word64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        data[at],
        data[at + 1],
        data[at + 2],
        data[at + 3],
        data[at + 4],
        data[at + 5],
        data[at + 6],
        data[at + 7],
    ])
}

/// The offsets of the objects in `data`: a listed object (libbinder's table) is an fd, or a binder or handle with a non-zero `binder` word (`Parcel::writeObject` leaves a null binder out of the table).
/// Checked against `expected_count`, libbinder's own number.
pub fn scan_objects(data: &[u8], expected_count: usize) -> Result<Vec<usize>, ScanError> {
    let mut offsets = Vec::new();
    let mut at = 0usize;
    while at + FLAT_OBJECT_SIZE <= data.len() {
        let listed = match word(data, at) {
            BINDER_TYPE_FD => true,
            BINDER_TYPE_BINDER | BINDER_TYPE_HANDLE => word64(data, at + 8) != 0,
            _ => false,
        };
        if listed {
            offsets.push(at);
            at += FLAT_OBJECT_SIZE;
        } else {
            at += 4;
        }
    }
    if offsets.len() != expected_count {
        return Err(ScanError {
            found: offsets.len(),
            expected: expected_count,
        });
    }
    Ok(offsets)
}

/// What kind of object sits at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A binder or handle.
    Binder,
    /// A file descriptor.
    Fd,
}

/// The kind of the object at `offset` (an offset [`scan_objects`] returned).
pub fn kind_at(data: &[u8], offset: usize) -> ObjectKind {
    if word(data, offset) == BINDER_TYPE_FD {
        ObjectKind::Fd
    } else {
        ObjectKind::Binder
    }
}

/// The core's binder for `key`, as [`set_binder_key`] leaves one in a parcel (flags as libbinder writes every binder: the scheduling bits of [`StrongBinder::NULL`]).
pub fn canonical_binder(key: u64) -> StrongBinder {
    StrongBinder {
        obj: FlatObject {
            ty: BINDER_TYPE_BINDER,
            flags: StrongBinder::NULL.obj.flags,
            binder: key,
            cookie: key,
        },
        stability: 0,
    }
}

/// Renames the binder object at `offset` to `key`: a `BINDER_TYPE_BINDER` whose `binder` and `cookie` words are `key`, so the core's `BinderKey::Local(cookie)` is the key. Flags are kept.
pub fn set_binder_key(data: &mut [u8], offset: usize, key: u64) {
    data[offset..offset + 4].copy_from_slice(&BINDER_TYPE_BINDER.to_le_bytes());
    data[offset + 8..offset + 16].copy_from_slice(&key.to_le_bytes());
    data[offset + 16..offset + 24].copy_from_slice(&key.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat_binder_object: type, flags, binder/handle, cookie (24 bytes, little-endian; binder.h).
    fn flat(ty: u32, flags: u32, binder: u64, cookie: u64) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(&binder.to_le_bytes());
        v.extend_from_slice(&cookie.to_le_bytes());
        v
    }

    /// Words, a binder, an fd and a null binder (unlisted): the scan lists the binder and the fd by offset and skips the null one; a wrong count is refused.
    #[test]
    fn scan_lists_binders_and_fds_not_null_binders() {
        let mut data = vec![1, 0, 0, 0, 2, 0, 0, 0];
        data.extend(flat(BINDER_TYPE_BINDER, 0x13, 0x7000, 0x7000)); // at 8
        data.extend([0; 4]); // stability word
        data.extend(flat(BINDER_TYPE_FD, 0, 5, 0)); // at 36
        data.extend(flat(BINDER_TYPE_BINDER, 0x13, 0, 0)); // null, at 60
        assert_eq!(scan_objects(&data, 2), Ok(vec![8, 36]));
        assert_eq!(
            scan_objects(&data, 3),
            Err(ScanError {
                found: 2,
                expected: 3
            })
        );
    }

    /// Renaming rewrites type, binder and cookie words and nothing else: flags and the neighbours stay.
    #[test]
    fn set_binder_key_rewrites_three_words() {
        let mut data = vec![9, 9, 9, 9];
        data.extend(flat(BINDER_TYPE_HANDLE, 0x13, 4, 0));
        data.extend([7, 7, 7, 7]);
        set_binder_key(&mut data, 4, 0x1122_3344_5566_7788);
        let mut expected = vec![9, 9, 9, 9];
        expected.extend(flat(
            BINDER_TYPE_BINDER,
            0x13,
            0x1122_3344_5566_7788,
            0x1122_3344_5566_7788,
        ));
        expected.extend([7, 7, 7, 7]);
        assert_eq!(data, expected);
        assert_eq!(kind_at(&data, 4), ObjectKind::Binder);
    }

    /// The canonical binder of a key is a local binder named by the key and nothing else.
    #[test]
    fn canonical_binder_is_named_by_its_key() {
        let b = canonical_binder(42);
        assert_eq!(b.key(), Some(atl_compositor::parcel::BinderKey::Local(42)));
        assert_eq!(b.obj.binder, 42);
        assert!(!b.is_null());
    }

    /// An fd is told from a binder by its type word.
    #[test]
    fn kind_distinguishes_fd() {
        let data = flat(BINDER_TYPE_FD, 0, 3, 0);
        assert_eq!(kind_at(&data, 0), ObjectKind::Fd);
    }
}
