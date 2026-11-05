// SPDX-License-Identifier: MIT
//! android.app.ContentProviderHolder: app/ContentProviderHolder.java writeToParcel / ContentProviderHolder(Parcel): the ProviderInfo in place, then two binders and two booleans.

use binder::SpIBinder;

use crate::codecs::android::content::pm::ProviderInfo::ProviderInfo;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct ContentProviderHolder {
    pub info: ProviderInfo,
    /// the IContentProvider's binder
    pub provider: Option<SpIBinder>,
    pub connection: Option<SpIBinder>,
    pub no_release_needed: bool,
    pub local: bool,
}

impl Part for ContentProviderHolder {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.info.write(p)?;
        w_binder(p, &self.provider)?;
        w_binder(p, &self.connection)?;
        w_bool(p, self.no_release_needed)?;
        w_bool(p, self.local)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ContentProviderHolder {
            info: ProviderInfo::read(p)?,
            provider: r_binder(p)?,
            connection: r_binder(p)?,
            no_release_needed: r_bool(p)?,
            local: r_bool(p)?,
        })
    }
}

crate::codec!(ContentProviderHolder);
