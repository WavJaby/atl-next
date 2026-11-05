// SPDX-License-Identifier: MIT
//! com.android.internal.inputmethod.InputMethodInfoSafeList (InputMethodInfoSafeList.java CREATOR: readBlob; writeToParcel: writeBlob of the marshalled InputMethodInfo array).
//! Modelled: the empty list only (InputMethodInfoSafeList.empty(), a null buffer), which writeBlob(null) writes as length -1 (android_os_Parcel writeBlob). A non-empty list is refused:
//! IMMS-lite has no input methods to list. Callers need it non-null: extractFrom dereferences its argument.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputMethodInfoSafeList;

/// writeBlob(null)'s length word.
const NULL_BLOB: i32 = -1;

impl Part for InputMethodInfoSafeList {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, NULL_BLOB)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        if r_i32(p)? != NULL_BLOB {
            return Err(crate::refuse(
                "com.android.internal.inputmethod.InputMethodInfoSafeList (non-empty)",
            ));
        }
        Ok(InputMethodInfoSafeList)
    }
}

crate::codec!(InputMethodInfoSafeList);
