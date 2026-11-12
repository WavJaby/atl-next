// SPDX-License-Identifier: MIT
//! android.content.pm.PermissionInfo: content/pm/PermissionInfo.java writeToParcel (:737) / PermissionInfo(Parcel) (:798), after PackageItemInfo.
//! nonLocalizedDescription: TextUtils.writeToParcel (plain text, `w_char_sequence`); knownCerts: Parcelling.BuiltIn.ForStringSet.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct PermissionInfo {
    pub item: PackageItemInfo,
    pub protection_level: i32,
    pub flags: i32,
    pub group: Option<String>,
    pub background_permission: Option<String>,
    pub description_res: i32,
    pub request_res: i32,
    pub non_localized_description: Option<String>,
    /// None = null (-1); Java's field defaults to an empty set (count 0, PermissionInfo.java:509), and unparcel turns -1 into an empty set too, so a re-parcelled value writes 0
    pub known_certs: Option<Vec<Option<String>>>,
}

impl Part for PermissionInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.item.write(p)?;
        w_i32(p, self.protection_level)?;
        w_i32(p, self.flags)?;
        w_str8(p, &self.group)?;
        w_str8(p, &self.background_permission)?;
        w_i32(p, self.description_res)?;
        w_i32(p, self.request_res)?;
        w_char_sequence(p, &self.non_localized_description)?;
        // ForStringSet.parcel: -1 for null, else the count and each String16 (w_str16_array's layout)
        w_str16_array(p, &self.known_certs)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(PermissionInfo {
            item: PackageItemInfo::read(p)?,
            protection_level: r_i32(p)?,
            flags: r_i32(p)?,
            group: r_str8(p)?,
            background_permission: r_str8(p)?,
            description_res: r_i32(p)?,
            request_res: r_i32(p)?,
            non_localized_description: r_char_sequence(p)?,
            known_certs: r_str16_array(p)?,
        })
    }
}

crate::codec!(PermissionInfo);
