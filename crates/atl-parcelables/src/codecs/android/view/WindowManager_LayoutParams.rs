// SPDX-License-Identifier: MIT
//! android.view.WindowManager.LayoutParams (frameworks/base/core/java/android/view/WindowManager.java writeToParcel:5302-5365, LayoutParams(Parcel):5379-5441): the window attributes
//! IWindowSession.addToDisplayAsUser and relayout take. The write order is the Java order, read in the same order (struct literal fields evaluate in the order written).
//! Not modelled, empty or null only: providedInsets and paramsForRotation (writeTypedArray), kept as their count (-1 null, 0 empty) so the bytes come back as they were.
//! Flag-dependent: frameRateBoostOnTouch and isFrameRatePowerSavingsBalanced follow toolkit_set_frame_rate_read_only (WindowManager.java:4684), the READ_ONLY value framework.jar compiles.
//! TextUtils (title, accessibilityTitle) is plain text only (parts.rs); spanned text is refused.

use binder::SpIBinder;

use crate::aconfig::TOOLKIT_SET_FRAME_RATE_READ_ONLY;
use crate::parcel::*;
use crate::parts::*;

/// Field names are the Java ones in snake case; `type` is `r#type`.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default)]
pub struct WindowManager_LayoutParams {
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    pub r#type: i32,
    pub flags: i32,
    pub private_flags: i32,
    pub soft_input_mode: i32,
    pub layout_in_display_cutout_mode: i32,
    pub gravity: i32,
    pub horizontal_margin: f32,
    pub vertical_margin: f32,
    pub format: i32,
    pub window_animations: i32,
    pub alpha: f32,
    pub dim_amount: f32,
    pub screen_brightness: f32,
    pub button_brightness: f32,
    pub rotation_animation: i32,
    pub token: Option<SpIBinder>,
    pub window_context_token: Option<SpIBinder>,
    pub package_name: Option<String>,
    pub title: Option<String>,
    pub screen_orientation: i32,
    pub preferred_refresh_rate: f32,
    pub preferred_display_mode_id: i32,
    pub preferred_min_display_refresh_rate: f32,
    pub preferred_max_display_refresh_rate: f32,
    pub system_ui_visibility: i32,
    pub subtree_system_ui_visibility: i32,
    pub has_system_ui_listeners: bool,
    pub input_features: i32,
    pub user_activity_timeout: i64,
    pub surface_insets: Rect,
    pub has_manual_surface_insets: bool,
    pub receive_insets_ignoring_z_order: bool,
    pub preserve_previous_surface_insets: bool,
    pub accessibility_id_of_anchor: i64,
    pub accessibility_title: Option<String>,
    pub color_mode: i32,
    pub hide_timeout_milliseconds: i64,
    /// InsetsFlags.appearance / behavior (WindowManager.java InsetsFlags, written as two ints).
    pub insets_appearance: i32,
    pub insets_behavior: i32,
    pub fit_insets_types: i32,
    pub fit_insets_sides: i32,
    pub fit_insets_ignoring_visibility: bool,
    pub prefer_minimal_post_processing: bool,
    pub blur_behind_radius: i32,
    pub wallpaper_touch_events_enabled: bool,
    /// The count of providedInsets as written: -1 null, 0 empty (non-empty refused).
    pub provided_insets_count: i32,
    pub forcibly_shown_types: i32,
    /// The count of paramsForRotation as written: -1 null, 0 empty (non-empty refused).
    pub params_for_rotation_count: i32,
    pub display_flags: i32,
    pub desired_hdr_headroom: f32,
    /// Written and read only when TOOLKIT_SET_FRAME_RATE_READ_ONLY; otherwise the Java object has no such fields and these stay false.
    pub frame_rate_boost_on_touch: bool,
    pub is_frame_rate_power_savings_balanced: bool,
}

impl Part for WindowManager_LayoutParams {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.width)?;
        w_i32(p, self.height)?;
        w_i32(p, self.x)?;
        w_i32(p, self.y)?;
        w_i32(p, self.r#type)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.private_flags)?;
        w_i32(p, self.soft_input_mode)?;
        w_i32(p, self.layout_in_display_cutout_mode)?;
        w_i32(p, self.gravity)?;
        w_f32(p, self.horizontal_margin)?;
        w_f32(p, self.vertical_margin)?;
        w_i32(p, self.format)?;
        w_i32(p, self.window_animations)?;
        w_f32(p, self.alpha)?;
        w_f32(p, self.dim_amount)?;
        w_f32(p, self.screen_brightness)?;
        w_f32(p, self.button_brightness)?;
        w_i32(p, self.rotation_animation)?;
        w_binder(p, &self.token)?;
        w_binder(p, &self.window_context_token)?;
        w_str16(p, &self.package_name)?;
        w_char_sequence(p, &self.title)?;
        w_i32(p, self.screen_orientation)?;
        w_f32(p, self.preferred_refresh_rate)?;
        w_i32(p, self.preferred_display_mode_id)?;
        w_f32(p, self.preferred_min_display_refresh_rate)?;
        w_f32(p, self.preferred_max_display_refresh_rate)?;
        w_i32(p, self.system_ui_visibility)?;
        w_i32(p, self.subtree_system_ui_visibility)?;
        w_bool(p, self.has_system_ui_listeners)?;
        w_i32(p, self.input_features)?;
        w_i64(p, self.user_activity_timeout)?;
        self.surface_insets.write(p)?;
        w_bool(p, self.has_manual_surface_insets)?;
        w_bool(p, self.receive_insets_ignoring_z_order)?;
        w_bool(p, self.preserve_previous_surface_insets)?;
        w_i64(p, self.accessibility_id_of_anchor)?;
        w_char_sequence(p, &self.accessibility_title)?;
        w_i32(p, self.color_mode)?;
        w_i64(p, self.hide_timeout_milliseconds)?;
        w_i32(p, self.insets_appearance)?;
        w_i32(p, self.insets_behavior)?;
        w_i32(p, self.fit_insets_types)?;
        w_i32(p, self.fit_insets_sides)?;
        w_bool(p, self.fit_insets_ignoring_visibility)?;
        w_bool(p, self.prefer_minimal_post_processing)?;
        w_i32(p, self.blur_behind_radius)?;
        w_bool(p, self.wallpaper_touch_events_enabled)?;
        w_i32(p, self.provided_insets_count)?;
        w_i32(p, self.forcibly_shown_types)?;
        w_i32(p, self.params_for_rotation_count)?;
        w_i32(p, self.display_flags)?;
        w_f32(p, self.desired_hdr_headroom)?;
        if TOOLKIT_SET_FRAME_RATE_READ_ONLY {
            w_bool(p, self.frame_rate_boost_on_touch)?;
            w_bool(p, self.is_frame_rate_power_savings_balanced)?;
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(WindowManager_LayoutParams {
            width: r_i32(p)?,
            height: r_i32(p)?,
            x: r_i32(p)?,
            y: r_i32(p)?,
            r#type: r_i32(p)?,
            flags: r_i32(p)?,
            private_flags: r_i32(p)?,
            soft_input_mode: r_i32(p)?,
            layout_in_display_cutout_mode: r_i32(p)?,
            gravity: r_i32(p)?,
            horizontal_margin: r_f32(p)?,
            vertical_margin: r_f32(p)?,
            format: r_i32(p)?,
            window_animations: r_i32(p)?,
            alpha: r_f32(p)?,
            dim_amount: r_f32(p)?,
            screen_brightness: r_f32(p)?,
            button_brightness: r_f32(p)?,
            rotation_animation: r_i32(p)?,
            token: r_binder(p)?,
            window_context_token: r_binder(p)?,
            package_name: r_str16(p)?,
            title: r_char_sequence(p)?,
            screen_orientation: r_i32(p)?,
            preferred_refresh_rate: r_f32(p)?,
            preferred_display_mode_id: r_i32(p)?,
            preferred_min_display_refresh_rate: r_f32(p)?,
            preferred_max_display_refresh_rate: r_f32(p)?,
            system_ui_visibility: r_i32(p)?,
            subtree_system_ui_visibility: r_i32(p)?,
            has_system_ui_listeners: r_bool(p)?,
            input_features: r_i32(p)?,
            user_activity_timeout: r_i64(p)?,
            surface_insets: Rect::read(p)?,
            has_manual_surface_insets: r_bool(p)?,
            receive_insets_ignoring_z_order: r_bool(p)?,
            preserve_previous_surface_insets: r_bool(p)?,
            accessibility_id_of_anchor: r_i64(p)?,
            accessibility_title: r_char_sequence(p)?,
            color_mode: r_i32(p)?,
            hide_timeout_milliseconds: r_i64(p)?,
            insets_appearance: r_i32(p)?,
            insets_behavior: r_i32(p)?,
            fit_insets_types: r_i32(p)?,
            fit_insets_sides: r_i32(p)?,
            fit_insets_ignoring_visibility: r_bool(p)?,
            prefer_minimal_post_processing: r_bool(p)?,
            blur_behind_radius: r_i32(p)?,
            wallpaper_touch_events_enabled: r_bool(p)?,
            provided_insets_count: r_absent_array(p, "android.view.InsetsFrameProvider[]")?,
            forcibly_shown_types: r_i32(p)?,
            params_for_rotation_count: r_absent_array(p, "android.view.WindowManager.LayoutParams[]")?,
            display_flags: r_i32(p)?,
            desired_hdr_headroom: r_f32(p)?,
            frame_rate_boost_on_touch: TOOLKIT_SET_FRAME_RATE_READ_ONLY && r_bool(p)?,
            is_frame_rate_power_savings_balanced: TOOLKIT_SET_FRAME_RATE_READ_ONLY && r_bool(p)?,
        })
    }
}

crate::codec!(WindowManager_LayoutParams);
