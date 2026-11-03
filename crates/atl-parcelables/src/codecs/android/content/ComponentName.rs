// SPDX-License-Identifier: MIT
//! android.content.ComponentName as a Parcelable (content/ComponentName.java writeToParcel / ComponentName(Parcel)): two String16s, package then class; both are non-null (the reading constructor checks).
//! The type is `parts::ComponentName`, which Intent and the launch items already use through the static `writeToParcel(ComponentName, Parcel)` helper (a null name is a null package); AIDL parcelable
//! arguments (IActivityManager.startInstrumentation's className, IPackageManager.getInstrumentationInfoAsUser's) take this form, with the null flag libbinder_rs adds.

pub use crate::parts::ComponentName;

use crate::parcel::*;
use crate::parts::*;

impl Part for ComponentName {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &Some(self.package.clone()))?;
        w_str16(p, &Some(self.class.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        match (r_str16(p)?, r_str16(p)?) {
            (Some(package), Some(class)) => Ok(ComponentName { package, class }),
            _ => Err(crate::refuse("android.content.ComponentName (null name)")),
        }
    }
}

crate::codec!(ComponentName);
