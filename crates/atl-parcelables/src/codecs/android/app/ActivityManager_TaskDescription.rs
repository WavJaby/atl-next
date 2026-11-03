// SPDX-License-Identifier: MIT
//! android.app.ActivityManager.TaskDescription (ActivityManager.java:1816, writeToParcel / readFromParcel): the values an activity sends with IActivityClientController.setTaskDescription,
//! which Activity.onApplyThemeResource sends for every themed activity. Written as Parcel.writeTypedObject: the presence int, then the fields in the order below.
//! Not modelled: the icon (an android.graphics.drawable.Icon). A held icon is refused here; the icon's own presence flag is written 0 by this codec, which never holds one.

use crate::parcel::*;
use crate::parts::*;

/// Java's own field order (writeToParcel): label (presence, String), icon (presence), colorPrimary, colorBackground, statusBarColor, navigationBarColor, systemBarsAppearance,
/// topOpaqueSystemBarsAppearance, ensureStatusBarContrastWhenTransparent, ensureNavigationBarContrastWhenTransparent, resizeMode, minWidth, minHeight, iconFilename (presence, String),
/// colorBackgroundFloating.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityManager_TaskDescription {
    pub label: Option<String>,
    pub color_primary: i32,
    pub color_background: i32,
    pub status_bar_color: i32,
    pub navigation_bar_color: i32,
    pub system_bars_appearance: i32,
    pub top_opaque_system_bars_appearance: i32,
    pub ensure_status_bar_contrast_when_transparent: bool,
    pub ensure_navigation_bar_contrast_when_transparent: bool,
    pub resize_mode: i32,
    pub min_width: i32,
    pub min_height: i32,
    pub icon_filename: Option<String>,
    pub color_background_floating: i32,
}

impl Part for ActivityManager_TaskDescription {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_present(p, self.label.is_some())?;
        if let Some(label) = &self.label {
            w_str16(p, &Some(label.clone()))?;
        }
        w_present(p, false)?; // icon: never held here
        w_i32(p, self.color_primary)?;
        w_i32(p, self.color_background)?;
        w_i32(p, self.status_bar_color)?;
        w_i32(p, self.navigation_bar_color)?;
        w_i32(p, self.system_bars_appearance)?;
        w_i32(p, self.top_opaque_system_bars_appearance)?;
        w_bool(p, self.ensure_status_bar_contrast_when_transparent)?;
        w_bool(p, self.ensure_navigation_bar_contrast_when_transparent)?;
        w_i32(p, self.resize_mode)?;
        w_i32(p, self.min_width)?;
        w_i32(p, self.min_height)?;
        w_present(p, self.icon_filename.is_some())?;
        if let Some(name) = &self.icon_filename {
            w_str16(p, &Some(name.clone()))?;
        }
        w_i32(p, self.color_background_floating)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let label = if r_present(p)? { r_str16(p)? } else { None };
        // the icon: an android.graphics.drawable.Icon, not modelled
        r_absent(p, "android.app.ActivityManager.TaskDescription.mIcon")?;
        let color_primary = r_i32(p)?;
        let color_background = r_i32(p)?;
        let status_bar_color = r_i32(p)?;
        let navigation_bar_color = r_i32(p)?;
        let system_bars_appearance = r_i32(p)?;
        let top_opaque_system_bars_appearance = r_i32(p)?;
        let ensure_status_bar_contrast_when_transparent = r_bool(p)?;
        let ensure_navigation_bar_contrast_when_transparent = r_bool(p)?;
        let resize_mode = r_i32(p)?;
        let min_width = r_i32(p)?;
        let min_height = r_i32(p)?;
        let icon_filename = if r_present(p)? { r_str16(p)? } else { None };
        Ok(ActivityManager_TaskDescription {
            label,
            color_primary,
            color_background,
            status_bar_color,
            navigation_bar_color,
            system_bars_appearance,
            top_opaque_system_bars_appearance,
            ensure_status_bar_contrast_when_transparent,
            ensure_navigation_bar_contrast_when_transparent,
            resize_mode,
            min_width,
            min_height,
            icon_filename,
            color_background_floating: r_i32(p)?,
        })
    }
}

crate::codec!(ActivityManager_TaskDescription);
