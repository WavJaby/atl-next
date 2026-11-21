// SPDX-License-Identifier: MIT
//! java.util.Map as framework AIDL's raw `Map` carries it (IApplicationThread.bindApplication's `services`): the aidl Java backend writes Parcel.writeMap -> writeMapInternal (Parcel.java:1448):
//! size (null: -1), then writeValue(key), writeValue(value) per entry; its Stub reads readHashMap (readMapInternal, N < 0 -> null). Each value is an int32 VAL_* tag and the value.
//! Modelled tags: VAL_NULL, VAL_STRING (writeString), VAL_IBINDER (writeStrongBinder) -- what AMS.getCommonServicesLocked's name -> binder map holds. Any other tag is refused and counted:
//! its length is not known without a codec for it. The generic `Map<K, V>` has other bytes: [`super::TypedMap`].
//! No parcelable null flag: a null Map is size -1, so Serialize / Deserialize and their Option forms are written here instead of `codec!`.

use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Serialize, SerializeOption,
};
use binder::{SpIBinder, StatusCode};

use crate::parcel::*;

/// writeMapInternal's size for a null map.
const NULL_SIZE: i32 = -1;
/// Parcel.java VAL_* tags (Parcel.java:317-333).
const VAL_NULL: i32 = -1;
const VAL_STRING: i32 = 0;
const VAL_IBINDER: i32 = 15;

/// One key or value as Parcel.writeValue tags it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    /// VAL_STRING; writeString(null) after the tag is length -1 (Java writes VAL_NULL for a null String, but readValue accepts both).
    String(Option<String>),
    /// VAL_IBINDER; readStrongBinder may read null.
    Binder(Option<SpIBinder>),
}

/// The entries in the order written (a HashMap's / ArrayMap's iteration order on the Java side).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map {
    pub entries: Vec<(Value, Value)>,
}

fn w_value(p: &mut BorrowedParcel<'_>, v: &Value) -> Result<()> {
    match v {
        Value::Null => w_i32(p, VAL_NULL),
        Value::String(s) => {
            w_i32(p, VAL_STRING)?;
            w_str16(p, s)
        }
        Value::Binder(b) => {
            w_i32(p, VAL_IBINDER)?;
            w_binder(p, b)
        }
    }
}

fn r_value(p: &BorrowedParcel<'_>) -> Result<Value> {
    Ok(match r_i32(p)? {
        VAL_NULL => Value::Null,
        VAL_STRING => Value::String(r_str16(p)?),
        VAL_IBINDER => Value::Binder(r_binder(p)?),
        _ => return Err(crate::refuse("java.util.Map (value type)")),
    })
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
    /// readHashMapInternal: any negative size is null.
    fn deserialize_option(p: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        let n = r_i32(p)?;
        if n < 0 {
            return Ok(None);
        }
        let mut entries = Vec::new();
        for _ in 0..n {
            let k = r_value(p)?;
            entries.push((k, r_value(p)?));
        }
        Ok(Some(Map { entries }))
    }
}
