// SPDX-License-Identifier: MIT
//! android.os.LocaleList (os/LocaleList.java writeToParcel :143 / CREATOR :264): one String8, mStringRepresentation (the comma-joined language tags; "" for the empty list, never null).
//! The CREATOR reads it back through forLanguageTags, which maps null and "" to the empty list: a null String8 reads as "". The tags are kept as sent; forLanguageTags' re-normalisation (case, `_`) is not modelled.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocaleList {
    /// language tags joined by ',' ("zh-TW,en-US"); "" = LocaleList.getEmptyLocaleList()
    pub tags: String,
}

impl Part for LocaleList {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &Some(self.tags.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(LocaleList { tags: r_str8(p)?.unwrap_or_default() })
    }
}

crate::codec!(LocaleList);
