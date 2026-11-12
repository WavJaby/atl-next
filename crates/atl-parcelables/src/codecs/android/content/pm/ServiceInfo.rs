// SPDX-License-Identifier: MIT
//! android.content.pm.ServiceInfo: content/pm/ServiceInfo.java writeToParcel (:728) / ServiceInfo(Parcel) (:745), after ComponentInfo.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct ServiceInfo {
    pub component: ComponentInfo,
    pub permission: Option<String>,
    pub flags: i32,
    /// mForegroundServiceType
    pub foreground_service_type: i32,
}

impl Part for ServiceInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.component.write(p)?;
        w_str8(p, &self.permission)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.foreground_service_type)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ServiceInfo {
            component: ComponentInfo::read(p)?,
            permission: r_str8(p)?,
            flags: r_i32(p)?,
            foreground_service_type: r_i32(p)?,
        })
    }
}

crate::codec!(ServiceInfo);
