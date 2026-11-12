// SPDX-License-Identifier: MIT
//! android.content.pm.ProviderInfo: content/pm/ProviderInfo.java writeToParcel (:163) / ProviderInfo(Parcel) (:193), after ComponentInfo. uriPermissionPatterns and pathPermissions are writeTypedArrays
//! (generate makes them non-null, PackageInfoCommonUtils.java:441-442).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct ProviderInfo {
    pub component: ComponentInfo,
    pub authority: Option<String>,
    pub read_permission: Option<String>,
    pub write_permission: Option<String>,
    pub grant_uri_permissions: bool,
    pub force_uri_permissions: bool,
    pub uri_permission_patterns: Option<Vec<Option<PatternMatcher>>>,
    pub path_permissions: Option<Vec<Option<PathPermission>>>,
    pub multiprocess: bool,
    pub init_order: i32,
    pub flags: i32,
    pub is_syncable: bool,
}

impl Part for ProviderInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.component.write(p)?;
        w_str8(p, &self.authority)?;
        w_str8(p, &self.read_permission)?;
        w_str8(p, &self.write_permission)?;
        w_bool(p, self.grant_uri_permissions)?;
        w_bool(p, self.force_uri_permissions)?;
        w_typed_list(p, &self.uri_permission_patterns)?;
        w_typed_list(p, &self.path_permissions)?;
        w_bool(p, self.multiprocess)?;
        w_i32(p, self.init_order)?;
        w_i32(p, self.flags)?;
        w_bool(p, self.is_syncable)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ProviderInfo {
            component: ComponentInfo::read(p)?,
            authority: r_str8(p)?,
            read_permission: r_str8(p)?,
            write_permission: r_str8(p)?,
            grant_uri_permissions: r_bool(p)?,
            force_uri_permissions: r_bool(p)?,
            uri_permission_patterns: r_typed_list(p)?,
            path_permissions: r_typed_list(p)?,
            multiprocess: r_bool(p)?,
            init_order: r_i32(p)?,
            flags: r_i32(p)?,
            is_syncable: r_bool(p)?,
        })
    }
}

crate::codec!(ProviderInfo);
