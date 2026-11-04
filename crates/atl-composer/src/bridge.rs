// SPDX-License-Identifier: MIT
//! libbinder Parcel <-> the core's `WireParcel`, through libbinder_rs's safe accessors only (patches/aosp/frameworks/native: `Parcel::data_with_objects`).
//! In: the data with the binder objects renamed to keys ([`read_parcel`]; [`crate::wire`] for the byte work). Out: a `WireParcel`'s words written after the interface token `prepare_transact` already wrote,
//! each binder object as the real binder its key stands for ([`write_body`]). File descriptor objects are refused both ways until buffers come (fds need an ownership rule: libbinder closes the ones in a Parcel it frees).

use std::collections::HashMap;

use atl_compositor::parcel::{BINDER_TYPE_BINDER, FLAT_OBJECT_SIZE, WireParcel};
use binder::binder_impl::{BorrowedParcel, Parcel};
use binder::{SpIBinder, StatusCode, WpIBinder};

use crate::objects::ObjectMap;
use crate::wire::{ObjectKind, kind_at, scan_objects, set_binder_key};

/// `data` as a `WireParcel` from its start (whatever the read position), the binder objects named by `objects`' keys (a binder not met before gets the next key).
/// A binder met for the first time is a client's own (a listener: libgui's TransactionCompletedListener), not one of the compositor's (a layer handle is known from its creation): it is kept in `foreign`,
/// strongly, because libbinder_rs wraps a binder it did not create in an object that dies with its last reference, and the weak reference in `objects` would die with it.
/// A parcel with a file descriptor is refused with `INVALID_OPERATION`; the read position is restored.
pub fn read_parcel(
    data: &BorrowedParcel<'_>,
    objects: &mut ObjectMap<WpIBinder>,
    foreign: &mut HashMap<u64, SpIBinder>,
) -> Result<WireParcel, StatusCode> {
    let (mut bytes, count) = data.data_with_objects();
    let offsets = scan_objects(&bytes, count).map_err(|_| StatusCode::BAD_VALUE)?;
    let saved = data.get_data_position();
    let renamed = (|| {
        for &offset in &offsets {
            if kind_at(&bytes, offset) == ObjectKind::Fd {
                return Err(StatusCode::INVALID_OPERATION);
            }
            data.set_data_position_checked(
                i32::try_from(offset).map_err(|_| StatusCode::BAD_VALUE)?,
            )?;
            let mut binder: SpIBinder = data.read()?;
            let weak = binder.downgrade();
            let key = match objects.find(&weak) {
                Some(key) => key,
                None => {
                    let key = objects.key_for(&weak);
                    foreign.insert(key, binder);
                    key
                }
            };
            set_binder_key(&mut bytes, offset, key);
        }
        Ok(())
    })();
    data.set_data_position_checked(saved)?;
    renamed?;
    WireParcel::from_wire(bytes, offsets).map_err(|_| StatusCode::BAD_VALUE)
}

/// Writes `wire` from byte `skip` on (past the interface token the transaction's parcel already holds) into `parcel`: words as int32s, each binder object (its 24 bytes and the stability word
/// that follows) as `resolve(key)`'s binder. `DEAD_OBJECT` when a key resolves to nothing; `INVALID_OPERATION` for a file descriptor.
pub fn write_body(
    parcel: &mut Parcel,
    wire: &WireParcel,
    skip: usize,
    resolve: &dyn Fn(u64) -> Option<SpIBinder>,
) -> Result<(), StatusCode> {
    let data = wire.data();
    let mut objects = wire.objects().iter().copied().peekable();
    let mut at = skip;
    while at < data.len() {
        while objects.next_if(|&o| o < at).is_some() {}
        if objects.next_if(|&o| o == at).is_some() {
            if kind_at(data, at) == ObjectKind::Fd {
                return Err(StatusCode::INVALID_OPERATION);
            }
            let key = u64::from_le_bytes(
                data[at + 16..at + 24]
                    .try_into()
                    .map_err(|_| StatusCode::BAD_VALUE)?,
            );
            debug_assert_eq!(
                u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]),
                BINDER_TYPE_BINDER
            );
            parcel.write(&resolve(key).ok_or(StatusCode::DEAD_OBJECT)?)?;
            // the object and its stability word
            at += FLAT_OBJECT_SIZE + 4;
        } else {
            let word = data
                .get(at..at + 4)
                .ok_or(StatusCode::BAD_VALUE)?
                .try_into()
                .map_err(|_| StatusCode::BAD_VALUE)?;
            parcel.write(&i32::from_le_bytes(word))?;
            at += 4;
        }
    }
    Ok(())
}
