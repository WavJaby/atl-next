// SPDX-License-Identifier: MIT
//! android.content.pm.InstallSourceInfo (InstallSourceInfo.java, writeToParcel / Parcel constructor): IPackageManager.getInstallSourceInfo's answer.
//! The initiating package's SigningInfo (writeParcelable) is modelled only as null: ATL records no installer, so the services never send one; a non-null one is refused and counted.

use crate::parcel::*;
use crate::parts::*;

const SIGNING_INFO: &str = "android.content.pm.SigningInfo";

/// Field order: initiatingPackageName (String16), initiatingPackageSigningInfo (writeParcelable: class name, null = null name), originatingPackageName, installingPackageName (String16),
/// updateOwnerPackageName (String8), packageSource (int, PackageInstaller.PACKAGE_SOURCE_*).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstallSourceInfo {
    pub initiating_package_name: Option<String>,
    pub originating_package_name: Option<String>,
    pub installing_package_name: Option<String>,
    pub update_owner_package_name: Option<String>,
    pub package_source: i32,
}

impl Part for InstallSourceInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.initiating_package_name)?;
        w_class(p, None)?;
        w_str16(p, &self.originating_package_name)?;
        w_str16(p, &self.installing_package_name)?;
        w_str8(p, &self.update_owner_package_name)?;
        w_i32(p, self.package_source)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let initiating_package_name = r_str16(p)?;
        if r_class(
            p,
            SIGNING_INFO,
            "android.content.pm.InstallSourceInfo.mInitiatingPackageSigningInfo",
        )?
        .is_some()
        {
            return Err(crate::refuse(
                "android.content.pm.InstallSourceInfo.mInitiatingPackageSigningInfo",
            ));
        }
        Ok(Self {
            initiating_package_name,
            originating_package_name: r_str16(p)?,
            installing_package_name: r_str16(p)?,
            update_owner_package_name: r_str8(p)?,
            package_source: r_i32(p)?,
        })
    }
}

crate::codec!(InstallSourceInfo);
