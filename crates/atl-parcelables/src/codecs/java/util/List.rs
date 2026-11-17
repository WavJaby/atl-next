// SPDX-License-Identifier: MIT
//! java.util.List as framework AIDL's raw `in List` carries it (IPackageManager.requestPackageChecksums' trustedInstallers): the aidl Java backend's Proxy calls Parcel.writeList -- size
//! (-1 = null), then writeValue per element = a VAL_* tag and the value; its Stub reads it back with readArrayList. The one element type modelled is what that caller sends:
//! ApplicationPackageManager.encodeCertificates' `byte[]` (VAL_BYTEARRAY, then writeByteArray) and null (VAL_NULL, tag only). Any other tag is refused and counted.
//! No parcelable null flag: a null List is size -1, so Serialize / Deserialize and their Option forms are written here instead of `codec!`.

use binder::StatusCode;
use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Serialize, SerializeOption,
};

use crate::parcel::*;

/// Parcel.writeList's size for a null list.
const NULL_SIZE: i32 = -1;
/// Parcel.VAL_NULL: writeValue(null) writes only this tag.
const VAL_NULL: i32 = -1;
/// Parcel.VAL_BYTEARRAY: writeValue(byte[]) = this tag, then writeByteArray (not length-prefixed: Parcel.isLengthPrefixed).
const VAL_BYTEARRAY: i32 = 13;

/// The elements in order; `None` is a null element.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct List {
    pub byte_arrays: Vec<Option<Vec<u8>>>,
}

impl Serialize for List {
    fn serialize(&self, p: &mut BorrowedParcel<'_>) -> Result<()> {
        w_i32(p, len_i32(self.byte_arrays.len())?)?;
        for e in &self.byte_arrays {
            match e {
                Some(b) => {
                    w_i32(p, VAL_BYTEARRAY)?;
                    w_bytes(p, Some(b))?;
                }
                None => w_i32(p, VAL_NULL)?,
            }
        }
        Ok(())
    }
}

impl SerializeOption for List {
    fn serialize_option(this: Option<&Self>, p: &mut BorrowedParcel<'_>) -> Result<()> {
        match this {
            Some(l) => l.serialize(p),
            None => w_i32(p, NULL_SIZE),
        }
    }
}

impl Deserialize for List {
    type UninitType = Option<Self>;
    fn uninit() -> Self::UninitType {
        None
    }
    fn from_init(value: Self) -> Self::UninitType {
        Some(value)
    }
    fn deserialize(p: &BorrowedParcel<'_>) -> Result<Self> {
        <Self as DeserializeOption>::deserialize_option(p)?.ok_or(StatusCode::UNEXPECTED_NULL)
    }
}

impl DeserializeOption for List {
    /// Parcel.readArrayList: any negative size is null.
    fn deserialize_option(p: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        let n = r_i32(p)?;
        if n < 0 {
            return Ok(None);
        }
        let mut byte_arrays = Vec::new();
        for _ in 0..n {
            byte_arrays.push(match r_i32(p)? {
                VAL_NULL => None,
                VAL_BYTEARRAY => Some(r_bytes(p)?.unwrap_or_default()),
                _ => return Err(crate::refuse("java.util.List element")),
            });
        }
        Ok(Some(List { byte_arrays }))
    }
}
