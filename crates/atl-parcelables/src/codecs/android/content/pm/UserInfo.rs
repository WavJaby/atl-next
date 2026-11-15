// SPDX-License-Identifier: MIT
//! android.content.pm.UserInfo (frameworks/base/core/java/android/content/pm/UserInfo.java writeToParcel / UserInfo(Parcel)): id, name, icon path, flags, user type, serial number,
//! creation and last-login times, last-login fingerprint, partial, pre-created, profile group, guest-to-remove, restricted-profile parent, profile badge. IUserManager.getUsers / getUserInfo return it.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInfo {
    pub id: i32,
    pub name: Option<String>,
    pub icon_path: Option<String>,
    /// UserInfo.FLAG_* bits
    pub flags: i32,
    /// UserManager.USER_TYPE_* name
    pub user_type: Option<String>,
    pub serial_number: i32,
    pub creation_time: i64,
    pub last_logged_in_time: i64,
    pub last_logged_in_fingerprint: Option<String>,
    pub partial: bool,
    pub pre_created: bool,
    pub profile_group_id: i32,
    pub guest_to_remove: bool,
    pub restricted_profile_parent_id: i32,
    pub profile_badge: i32,
}

impl Part for UserInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.id)?;
        w_str8(p, &self.name)?;
        w_str8(p, &self.icon_path)?;
        w_i32(p, self.flags)?;
        w_str8(p, &self.user_type)?;
        w_i32(p, self.serial_number)?;
        w_i64(p, self.creation_time)?;
        w_i64(p, self.last_logged_in_time)?;
        w_str8(p, &self.last_logged_in_fingerprint)?;
        w_bool(p, self.partial)?;
        w_bool(p, self.pre_created)?;
        w_i32(p, self.profile_group_id)?;
        w_bool(p, self.guest_to_remove)?;
        w_i32(p, self.restricted_profile_parent_id)?;
        w_i32(p, self.profile_badge)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(UserInfo {
            id: r_i32(p)?,
            name: r_str8(p)?,
            icon_path: r_str8(p)?,
            flags: r_i32(p)?,
            user_type: r_str8(p)?,
            serial_number: r_i32(p)?,
            creation_time: r_i64(p)?,
            last_logged_in_time: r_i64(p)?,
            last_logged_in_fingerprint: r_str8(p)?,
            partial: r_bool(p)?,
            pre_created: r_bool(p)?,
            profile_group_id: r_i32(p)?,
            guest_to_remove: r_bool(p)?,
            restricted_profile_parent_id: r_i32(p)?,
            profile_badge: r_i32(p)?,
        })
    }
}

crate::codec!(UserInfo);
