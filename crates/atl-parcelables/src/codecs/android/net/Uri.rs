// SPDX-License-Identifier: MIT
//! android.net.Uri as an AIDL parcelable (net/Uri.java writeToParcel / CREATOR): the type int, then toString() as String8. StringUri (1), OpaqueUri (2) and HierarchicalUri (3) all write
//! that layout (Uri.java:520, :899, :1225), so reading keeps the string. Writing uses StringUri's type, which reads back as a Uri equal to the original (Uri.equals compares toString()).
//! The NULL flag is the caller's (libbinder_rs writes it for an Option): a non-null slot that carries type 0 is refused.

use crate::parcel::*;
use crate::parts::*;

/// A URI as its toString().
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uri(pub String);

impl Part for Uri {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_uri(p, &Some(self.0.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        r_uri(p)?
            .map(Uri)
            .ok_or_else(|| crate::refuse("android.net.Uri (null)"))
    }
}

crate::codec!(Uri);
