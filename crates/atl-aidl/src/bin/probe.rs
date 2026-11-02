// SPDX-License-Identifier: MIT
//! Proof that a cargo-built binary links the converter's AOSP binder: `ServiceDebugInfo` (a structured parcelable of IServiceManager's AIDL, stub from atl-aidl) round-trips through AOSP's C++ Parcel (libbinder_rs -> libbinder_ndk AParcel -> libbinder Parcel).
//! No binder driver: a Parcel needs none (`ProcessState` would open /dev/binder and abort). Exit 0 = equal value back and the byte count Parcel's encoding gives; otherwise the error.

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use anyhow::{Result, ensure};
use atl_aidl::aidl::android::os::ServiceDebugInfo::ServiceDebugInfo;
use binder::binder_impl::Parcel;

/// The service name written: 14 UTF-16 code units.
const NAME: &str = "servicemanager";
/// `NAME` in Parcel's encoding (Parcel.cpp writeString16): int32 length 14, then (14 + 1 terminator) * 2 = 30 bytes padded to 4 -> 32; 36 in all.
const NAME_BYTES: i32 = 4 + 32;
/// Bytes of the parcel: the non-null parcelable marker (libbinder_rs `SerializeOption`, int32), the size header of the structured parcelable (`sized_write`, int32), the name, `debugPid` (int32).
const PARCEL_BYTES: i32 = 4 + 4 + NAME_BYTES + 4;

fn main() -> Result<()> {
    let sent = ServiceDebugInfo {
        name: NAME.to_owned(),
        // any value; distinct from the default 0 so a dropped field shows
        debugPid: 4242,
    };
    let mut parcel = Parcel::new();
    parcel.write(&sent)?;
    ensure!(
        parcel.get_data_size() == PARCEL_BYTES,
        "parcel holds {} bytes, Parcel's encoding gives {PARCEL_BYTES}",
        parcel.get_data_size()
    );
    // SAFETY: position 0 lies inside every parcel (libbinder_rs's own ParcelableHolder relies on the same); the reads below stay within the bytes written
    unsafe { parcel.set_data_position(0)? };
    let got: ServiceDebugInfo = parcel.read()?;
    // AIDL parcelables derive no PartialEq without @JavaDerive(equals=true): field by field
    ensure!(
        got.name == sent.name && got.debugPid == sent.debugPid,
        "read back {got:?}, wrote {sent:?}"
    );
    ensure!(
        parcel.get_data_position() == PARCEL_BYTES,
        "read stopped at {}, not at the end ({PARCEL_BYTES})",
        parcel.get_data_position()
    );
    Ok(())
}
