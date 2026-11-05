// SPDX-License-Identifier: MIT
//! android.content.pm.ProviderInfo: content/pm/ProviderInfo.java writeToParcel / ProviderInfo(Parcel), after ComponentInfo. Not modelled, null only: uriPermissionPatterns, pathPermissions.

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
        w_null(p)?; // uriPermissionPatterns
        w_null(p)?; // pathPermissions
        w_bool(p, self.multiprocess)?;
        w_i32(p, self.init_order)?;
        w_i32(p, self.flags)?;
        w_bool(p, self.is_syncable)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let component = ComponentInfo::read(p)?;
        let authority = r_str8(p)?;
        let read_permission = r_str8(p)?;
        let write_permission = r_str8(p)?;
        let grant_uri_permissions = r_bool(p)?;
        let force_uri_permissions = r_bool(p)?;
        r_null(p, "android.content.pm.ProviderInfo.uriPermissionPatterns")?;
        r_null(p, "android.content.pm.ProviderInfo.pathPermissions")?;
        Ok(ProviderInfo {
            component,
            authority,
            read_permission,
            write_permission,
            grant_uri_permissions,
            force_uri_permissions,
            multiprocess: r_bool(p)?,
            init_order: r_i32(p)?,
            flags: r_i32(p)?,
            is_syncable: r_bool(p)?,
        })
    }
}

crate::codec!(ProviderInfo);
