// SPDX-License-Identifier: MIT
//! android.window.OnBackInvokedCallbackInfo (window/OnBackInvokedCallbackInfo.java writeToParcel:59-63, the Parcel constructor :46-50): strong interface (IOnBackInvokedCallback), int
//! priority, boolean isAnimationCallback (writeBoolean = one int), int overrideBehavior. Passed by IWindowSession.setOnBackInvokedCallbackInfo: the system's copy of the window's top back callback.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OnBackInvokedCallbackInfo {
    /// The IOnBackInvokedCallback binder; None = null.
    pub callback: Option<SpIBinder>,
    pub priority: i32,
    pub is_animation_callback: bool,
    pub override_behavior: i32,
}

impl Part for OnBackInvokedCallbackInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.callback)?;
        w_i32(p, self.priority)?;
        w_bool(p, self.is_animation_callback)?;
        w_i32(p, self.override_behavior)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(OnBackInvokedCallbackInfo {
            callback: r_binder(p)?,
            priority: r_i32(p)?,
            is_animation_callback: r_bool(p)?,
            override_behavior: r_i32(p)?,
        })
    }
}

crate::codec!(OnBackInvokedCallbackInfo);
