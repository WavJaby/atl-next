// SPDX-License-Identifier: MIT
//! android.content.pm.ProviderInfoList: content/pm/ProviderInfoList.java writeToParcel (writeTypedList with squashing allowed) / ProviderInfoList(Parcel) (readTypedList).
//! Encoded without squashing (ApplicationInfo); a squashed back-reference is refused on read (ApplicationInfo.rs). A null list reads as empty, as readTypedList leaves the list empty.

use super::ProviderInfo::ProviderInfo;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderInfoList {
    /// entries; None = a null entry (writeTypedObject 0)
    pub list: Vec<Option<ProviderInfo>>,
}

impl Part for ProviderInfoList {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, len_i32(self.list.len())?)?;
        self.list.iter().try_for_each(|e| w_typed(p, e))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let n = r_i32(p)?;
        Ok(ProviderInfoList {
            list: (0..n.max(0)).map(|_| r_typed(p)).collect::<Result<_>>()?,
        })
    }
}

crate::codec!(ProviderInfoList);
