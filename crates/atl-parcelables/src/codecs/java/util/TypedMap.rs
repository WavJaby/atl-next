// SPDX-License-Identifier: MIT
//! java.util.Map as framework AIDL's generic `Map<K, V>` carries it (aidl `--rust_java_type Map<>`, bp2ninja aidlJavaOnly), modelled for `Map<String, String>` (IPackageManager.notifyDexLoad's
//! classLoaderContextMap): the aidl Java backend's generated Proxy writes size then writeString(key) / writeString(value) per entry, its Stub reads them back the same way (N < 0 -> null).
//! No VAL_* tags: the raw `Map` is Parcel.writeMap's tagged form ([`super::Map`]) -- decoding these bytes as that form read a string length as a tag and refused every app's dex-load report.
//! Other K, V (IJobScheduler.getAllPendingJobs' `Map<String, ParceledListSlice<JobInfo>>`) are not modelled: no service sends or reads one.
//! No parcelable null flag: a null Map is size -1, so Serialize / Deserialize and their Option forms are written here instead of `codec!`.

use binder::StatusCode;
use binder::binder_impl::{
    BorrowedParcel, Deserialize, DeserializeOption, Serialize, SerializeOption,
};

use crate::parcel::*;

/// The generated Proxy's size for a null map.
const NULL_SIZE: i32 = -1;

/// The entries in the order written (a HashMap's iteration order on the Java side); keys and values are nullable Strings (writeString(null) is length -1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypedMap {
    pub entries: Vec<(Option<String>, Option<String>)>,
}

impl Serialize for TypedMap {
    fn serialize(&self, p: &mut BorrowedParcel<'_>) -> Result<()> {
        w_i32(p, len_i32(self.entries.len())?)?;
        for (k, v) in &self.entries {
            w_str16(p, k)?;
            w_str16(p, v)?;
        }
        Ok(())
    }
}

impl SerializeOption for TypedMap {
    fn serialize_option(this: Option<&Self>, p: &mut BorrowedParcel<'_>) -> Result<()> {
        match this {
            Some(m) => m.serialize(p),
            None => w_i32(p, NULL_SIZE),
        }
    }
}

impl Deserialize for TypedMap {
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

impl DeserializeOption for TypedMap {
    /// Stub side: any negative size is null (`N < 0 ? null : new HashMap<>()`).
    fn deserialize_option(p: &BorrowedParcel<'_>) -> Result<Option<Self>> {
        let n = r_i32(p)?;
        if n < 0 {
            return Ok(None);
        }
        let mut entries = Vec::new();
        for _ in 0..n {
            let k = r_str16(p)?;
            entries.push((k, r_str16(p)?));
        }
        Ok(Some(TypedMap { entries }))
    }
}
