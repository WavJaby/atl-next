// SPDX-License-Identifier: MIT
//! Java `android.os.Parcel` primitives over libbinder_rs's parcel (AOSP's C++ Parcel underneath), byte for byte as Parcel.java / Parcel.cpp write them.
//! - int / long / float: native-endian, 4-byte aligned (Parcel.cpp writeInt32 / writeInt64 / writeFloat); Java's writeBoolean is writeInt(0|1), readBoolean `!= 0`.
//! - String16 (Java writeString / writeString16): libbinder_rs `Option<String>` (AParcel_writeString -> Parcel::writeString16): int32 UTF-16 length or -1, the units, a 0 unit, padded to 4.
//! - String8 (Java writeString8 -> Parcel::writeString8): int32 byte length or -1, the UTF-8 bytes, a 0 byte, zero-padded to 4. libbinder_rs has no String8, so it goes as int32 words: same bytes.
//! - binder (writeStrongBinder): libbinder_rs `Option<SpIBinder>`; null writes no object, so the parcel stays marshallable.

use binder::SpIBinder;
use binder::StatusCode;
use binder::binder_impl::BorrowedParcel;

pub type Result<T> = std::result::Result<T, StatusCode>;
pub type In<'a> = BorrowedParcel<'a>;

pub fn w_i32(p: &mut In<'_>, v: i32) -> Result<()> {
    p.write(&v)
}
pub fn r_i32(p: &In<'_>) -> Result<i32> {
    p.read()
}
pub fn w_i64(p: &mut In<'_>, v: i64) -> Result<()> {
    p.write(&v)
}
pub fn r_i64(p: &In<'_>) -> Result<i64> {
    p.read()
}
pub fn w_f32(p: &mut In<'_>, v: f32) -> Result<()> {
    p.write(&v)
}
pub fn r_f32(p: &In<'_>) -> Result<f32> {
    p.read()
}
/// Java writeBoolean / `writeInt(b ? 1 : 0)`.
pub fn w_bool(p: &mut In<'_>, v: bool) -> Result<()> {
    w_i32(p, i32::from(v))
}
/// Java readBoolean / `readInt() != 0`.
pub fn r_bool(p: &In<'_>) -> Result<bool> {
    Ok(r_i32(p)? != 0)
}

/// Java writeString / writeString16.
pub fn w_str16(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    p.write(v)
}
pub fn r_str16(p: &In<'_>) -> Result<Option<String>> {
    p.read()
}

/// Java writeString8.
pub fn w_str8(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    let Some(s) = v else { return w_i32(p, -1) };
    let len = i32::try_from(s.len()).map_err(|_| StatusCode::BAD_VALUE)?;
    w_i32(p, len)?;
    let mut bytes = s.as_bytes().to_vec();
    // the terminating 0, then zero padding to a 4-byte boundary
    bytes.resize((s.len() + 1).div_ceil(4) * 4, 0);
    for w in bytes.as_chunks::<4>().0 {
        w_i32(p, i32::from_ne_bytes(*w))?;
    }
    Ok(())
}
/// Java readString8: refuses a missing terminator or non-UTF-8 bytes (Java would decode them lossily; nothing round-trips them).
pub fn r_str8(p: &In<'_>) -> Result<Option<String>> {
    let len = r_i32(p)?;
    if len == -1 {
        return Ok(None);
    }
    let len = usize::try_from(len).map_err(|_| StatusCode::BAD_VALUE)?;
    let mut bytes = Vec::with_capacity((len + 1).div_ceil(4) * 4);
    for _ in 0..(len + 1).div_ceil(4) {
        bytes.extend_from_slice(&r_i32(p)?.to_ne_bytes());
    }
    if bytes[len] != 0 {
        return Err(StatusCode::BAD_VALUE);
    }
    bytes.truncate(len);
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| StatusCode::BAD_VALUE)
}

/// Java writeString8Array: -1 for null, else the count and each String8.
pub fn w_str8_array(p: &mut In<'_>, v: &Option<Vec<Option<String>>>) -> Result<()> {
    let Some(v) = v else { return w_i32(p, -1) };
    w_i32(p, len_i32(v.len())?)?;
    v.iter().try_for_each(|s| w_str8(p, s))
}
/// Java createString8Array.
pub fn r_str8_array(p: &In<'_>) -> Result<Option<Vec<Option<String>>>> {
    let n = r_i32(p)?;
    if n < 0 {
        return Ok(None);
    }
    (0..n)
        .map(|_| r_str8(p))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Java writeIntArray: -1 for null, else the count and each int.
pub fn w_i32_array(p: &mut In<'_>, v: &Option<Vec<i32>>) -> Result<()> {
    let Some(v) = v else { return w_i32(p, -1) };
    w_i32(p, len_i32(v.len())?)?;
    v.iter().try_for_each(|x| w_i32(p, *x))
}
/// Java createIntArray.
pub fn r_i32_array(p: &In<'_>) -> Result<Option<Vec<i32>>> {
    let n = r_i32(p)?;
    if n < 0 {
        return Ok(None);
    }
    (0..n)
        .map(|_| r_i32(p))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Java writeStrongBinder / writeStrongInterface.
pub fn w_binder(p: &mut In<'_>, v: &Option<SpIBinder>) -> Result<()> {
    p.write(v)
}
pub fn r_binder(p: &In<'_>) -> Result<Option<SpIBinder>> {
    p.read()
}

/// A value written as `-1` when null (writeBundle, writePersistableBundle, writeTypedList, writeTypedArray, writeSparseArray, writeParcelableList) of a type no codec models: only null.
pub fn w_null(p: &mut In<'_>) -> Result<()> {
    w_i32(p, -1)
}
/// Reads such a value: anything but null is refused as `what`.
pub fn r_null(p: &In<'_>, what: &'static str) -> Result<()> {
    if r_i32(p)? == -1 {
        Ok(())
    } else {
        Err(crate::refuse(what))
    }
}

/// writeTypedObject's presence flag (1 present, 0 null); reading, `!= 0` is present as Java's readTypedObject.
pub fn w_present(p: &mut In<'_>, present: bool) -> Result<()> {
    w_bool(p, present)
}
pub fn r_present(p: &In<'_>) -> Result<bool> {
    r_bool(p)
}

/// writeTypedObject of a type no codec models: only null (flag 0).
pub fn r_absent(p: &In<'_>, what: &'static str) -> Result<()> {
    if r_present(p)? {
        Err(crate::refuse(what))
    } else {
        Ok(())
    }
}

pub fn len_i32(n: usize) -> Result<i32> {
    i32::try_from(n).map_err(|_| StatusCode::BAD_VALUE)
}
