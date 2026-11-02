// SPDX-License-Identifier: MIT
//! android.os.Bundle: os/Bundle.java writeToParcel -> BaseBundle.writeToParcelInner / Bundle(Parcel) -> BaseBundle.readFromParcelInner. Modelled: the empty Bundle only, a length of 0
//! ("Special case for empty bundles"), what IApplicationThread.bindApplication's coreSettings carries in M2 (ActivityThread reads it unconditionally: updateDebugViewAttributeState).
//! A non-empty Bundle (length, magic, the map's parcelled entries) is refused.

use crate::parcel::*;
use crate::parts::*;

/// The keys are not modelled: a Bundle here is always empty.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle;

impl Part for Bundle {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, 0)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        match r_i32(p)? {
            0 => Ok(Bundle),
            _ => Err(crate::refuse(
                "android.os.Bundle (only the empty Bundle modelled)",
            )),
        }
    }
}

crate::codec!(Bundle);
