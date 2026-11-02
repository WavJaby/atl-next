// SPDX-License-Identifier: MIT
//! java.util.Map, the Java-only builtin (`in Map` in framework AIDL): os/Parcel.java writeMap / writeMapInternal and readHashMap / readMapInternal. Modelled: String or null keys and values
//! (writeValue VAL_STRING / VAL_NULL), what IPackageManager.notifyDexLoad's classLoaderContextMap carries; any other value type is refused.
//! No parcelable null flag: a null Map is size -1 (writeMapInternal), so Serialize / Deserialize and their Option forms are written here instead of `codec!`.

use binder::StatusCode;
use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Serialize, SerializeOption,
};

use crate::parcel::*;

/// Parcel.java VAL_NULL / VAL_STRING (writeValue's type tags).
const VAL_NULL: i32 = -1;
const VAL_STRING: i32 = 0;
/// writeMapInternal's size for a null map.
const NULL_SIZE: i32 = -1;

/// The entries in the order written (a HashMap's iteration order on the Java side).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map {
    pub entries: Vec<(Option<String>, Option<String>)>,
}

fn w_value(p: &mut In<'_>, v: &Option<String>) -> Result<()> {
    match v {
        None => w_i32(p, VAL_NULL),
        Some(_) => {
            w_i32(p, VAL_STRING)?;
            w_str16(p, v)
        }
    }
}

fn r_value(p: &In<'_>) -> Result<Option<String>> {
    match r_i32(p)? {
        VAL_NULL => Ok(None),
        VAL_STRING => r_str16(p),
        _ => Err(crate::refuse(
            "java.util.Map value (only String and null modelled)",
        )),
    }
}

impl Serialize for Map {
    fn serialize(&self, p: &mut BorrowedParcel<'_>) -> Result<()> {
        w_i32(p, len_i32(self.entries.len())?)?;
        for (k, v) in &self.entries {
            w_value(p, k)?;
            w_value(p, v)?;
        }
        Ok(())
    }
}

impl SerializeOption for Map {
    fn serialize_option(this: Option<&Self>, p: &mut BorrowedParcel<'_>) -> Result<()> {
        match this {
            Some(m) => m.serialize(p),
            None => w_i32(p, NULL_SIZE),
        }
    }
}

impl Deserialize for Map {
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

impl DeserializeOption for Map {
    fn deserialize_option(p: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        let n = r_i32(p)?;
        if n == NULL_SIZE {
            return Ok(None);
        }
        let mut entries = Vec::new();
        for _ in 0..n.max(0) {
            let k = r_value(p)?;
            entries.push((k, r_value(p)?));
        }
        Ok(Some(Map { entries }))
    }
}
