// SPDX-License-Identifier: MIT
//! android.view.DisplayInfo (frameworks/base/core/java/android/view/DisplayInfo.java writeToParcel:683-756 / readFromParcel:600-680), the record IDisplayManager.getDisplayInfo returns.
//! The write order is the Java order; every field is modelled. Not modelled, null only: the nested parcelables DisplayAddress, DeviceProductInfo, DisplayCutout, FrameRateCategoryRate,
//! Display.HdrCapabilities, RoundedCorners, DisplayShape and SurfaceControl.RefreshRateRange (a non-null one is refused: its layout is not written here), and the thermal
//! refresh-rate SparseArray, which is modelled only empty (a count of 0). Display.Mode is modelled (parts.rs DisplayMode).
//! The writer's nullable kinds: writeParcelable(null) is the String16 null -1 (`w_null`), writeTypedObject(null) the presence flag 0 (`w_present`), writeCutoutToParcel(null) -1 (`w_null`).

use crate::parcel::*;
use crate::parts::*;

/// Field names are the Java ones in snake case; `type` is `kind`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayInfo {
    pub layer_stack: i32,
    pub flags: i32,
    pub kind: i32,
    pub display_id: i32,
    pub display_group_id: i32,
    pub name: Option<String>,
    pub unique_id: Option<String>,
    pub app_width: i32,
    pub app_height: i32,
    pub smallest_nominal_app_width: i32,
    pub smallest_nominal_app_height: i32,
    pub largest_nominal_app_width: i32,
    pub largest_nominal_app_height: i32,
    pub logical_width: i32,
    pub logical_height: i32,
    pub rotation: i32,
    pub mode_id: i32,
    pub render_frame_rate: f32,
    pub has_arr_support: bool,
    pub supported_refresh_rates: Vec<f32>,
    pub default_mode_id: i32,
    pub user_preferred_mode_id: i32,
    pub supported_modes: Vec<DisplayMode>,
    pub apps_supported_modes: Vec<DisplayMode>,
    pub color_mode: i32,
    pub supported_color_modes: Vec<i32>,
    pub is_force_sdr: bool,
    pub minimal_post_processing_supported: bool,
    pub logical_density_dpi: i32,
    pub physical_x_dpi: f32,
    pub physical_y_dpi: f32,
    pub app_vsync_offset_nanos: i64,
    pub presentation_deadline_nanos: i64,
    pub state: i32,
    pub committed_state: i32,
    pub owner_uid: i32,
    pub owner_package_name: Option<String>,
    pub remove_mode: i32,
    pub refresh_rate_override: f32,
    pub brightness_minimum: f32,
    pub brightness_maximum: f32,
    pub brightness_default: f32,
    pub brightness_dim: f32,
    pub user_disabled_hdr_types: Vec<i32>,
    pub install_orientation: i32,
    pub hdr_sdr_ratio: f32,
    pub thermal_brightness_throttling_data_id: Option<String>,
    pub can_host_tasks: bool,
}

impl Part for DisplayInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.layer_stack)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.kind)?;
        w_i32(p, self.display_id)?;
        w_i32(p, self.display_group_id)?;
        w_null(p)?; // address
        w_null(p)?; // deviceProductInfo
        w_str8(p, &self.name)?;
        w_i32(p, self.app_width)?;
        w_i32(p, self.app_height)?;
        w_i32(p, self.smallest_nominal_app_width)?;
        w_i32(p, self.smallest_nominal_app_height)?;
        w_i32(p, self.largest_nominal_app_width)?;
        w_i32(p, self.largest_nominal_app_height)?;
        w_i32(p, self.logical_width)?;
        w_i32(p, self.logical_height)?;
        w_null(p)?; // displayCutout: writeCutoutToParcel(null)
        w_i32(p, self.rotation)?;
        w_i32(p, self.mode_id)?;
        w_f32(p, self.render_frame_rate)?;
        w_bool(p, self.has_arr_support)?;
        w_null(p)?; // frameRateCategoryRate
        w_f32_array(p, &Some(self.supported_refresh_rates.clone()))?;
        w_i32(p, self.default_mode_id)?;
        w_i32(p, self.user_preferred_mode_id)?;
        write_modes(p, &self.supported_modes)?;
        write_modes(p, &self.apps_supported_modes)?;
        w_i32(p, self.color_mode)?;
        w_i32_array(p, &Some(self.supported_color_modes.clone()))?;
        w_null(p)?; // hdrCapabilities
        w_bool(p, self.is_force_sdr)?;
        w_bool(p, self.minimal_post_processing_supported)?;
        w_i32(p, self.logical_density_dpi)?;
        w_f32(p, self.physical_x_dpi)?;
        w_f32(p, self.physical_y_dpi)?;
        w_i64(p, self.app_vsync_offset_nanos)?;
        w_i64(p, self.presentation_deadline_nanos)?;
        w_i32(p, self.state)?;
        w_i32(p, self.committed_state)?;
        w_i32(p, self.owner_uid)?;
        w_str8(p, &self.owner_package_name)?;
        w_str8(p, &self.unique_id)?;
        w_i32(p, self.remove_mode)?;
        w_f32(p, self.refresh_rate_override)?;
        w_f32(p, self.brightness_minimum)?;
        w_f32(p, self.brightness_maximum)?;
        w_f32(p, self.brightness_default)?;
        w_f32(p, self.brightness_dim)?;
        w_present(p, false)?; // roundedCorners
        w_i32_array(p, &Some(self.user_disabled_hdr_types.clone()))?;
        w_i32(p, self.install_orientation)?;
        w_present(p, false)?; // displayShape
        w_present(p, false)?; // layoutLimitedRefreshRate
        w_f32(p, self.hdr_sdr_ratio)?;
        w_i32(p, 0)?; // thermalRefreshRateThrottling: an empty SparseArray (writeSparseArray writes its size)
        w_str8(p, &self.thermal_brightness_throttling_data_id)?;
        w_bool(p, self.can_host_tasks)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let layer_stack = r_i32(p)?;
        let flags = r_i32(p)?;
        let kind = r_i32(p)?;
        let display_id = r_i32(p)?;
        let display_group_id = r_i32(p)?;
        r_null(p, "android.view.DisplayAddress")?;
        r_null(p, "android.hardware.display.DeviceProductInfo")?;
        let name = r_str8(p)?;
        let app_width = r_i32(p)?;
        let app_height = r_i32(p)?;
        let smallest_nominal_app_width = r_i32(p)?;
        let smallest_nominal_app_height = r_i32(p)?;
        let largest_nominal_app_width = r_i32(p)?;
        let largest_nominal_app_height = r_i32(p)?;
        let logical_width = r_i32(p)?;
        let logical_height = r_i32(p)?;
        r_null(p, "android.view.DisplayCutout")?;
        let rotation = r_i32(p)?;
        let mode_id = r_i32(p)?;
        let render_frame_rate = r_f32(p)?;
        let has_arr_support = r_bool(p)?;
        r_null(p, "android.view.FrameRateCategoryRate")?;
        let supported_refresh_rates = r_f32_array(p)?.ok_or(binder::StatusCode::BAD_VALUE)?;
        let default_mode_id = r_i32(p)?;
        let user_preferred_mode_id = r_i32(p)?;
        let supported_modes = read_modes(p)?;
        let apps_supported_modes = read_modes(p)?;
        let color_mode = r_i32(p)?;
        let supported_color_modes = r_i32_array(p)?.ok_or(binder::StatusCode::BAD_VALUE)?;
        r_null(p, "android.view.Display.HdrCapabilities")?;
        let is_force_sdr = r_bool(p)?;
        let minimal_post_processing_supported = r_bool(p)?;
        let logical_density_dpi = r_i32(p)?;
        let physical_x_dpi = r_f32(p)?;
        let physical_y_dpi = r_f32(p)?;
        let app_vsync_offset_nanos = r_i64(p)?;
        let presentation_deadline_nanos = r_i64(p)?;
        let state = r_i32(p)?;
        let committed_state = r_i32(p)?;
        let owner_uid = r_i32(p)?;
        let owner_package_name = r_str8(p)?;
        let unique_id = r_str8(p)?;
        let remove_mode = r_i32(p)?;
        let refresh_rate_override = r_f32(p)?;
        let brightness_minimum = r_f32(p)?;
        let brightness_maximum = r_f32(p)?;
        let brightness_default = r_f32(p)?;
        let brightness_dim = r_f32(p)?;
        r_absent(p, "android.view.RoundedCorners")?;
        let user_disabled_hdr_types = r_i32_array(p)?.ok_or(binder::StatusCode::BAD_VALUE)?;
        let install_orientation = r_i32(p)?;
        r_absent(p, "android.view.DisplayShape")?;
        r_absent(p, "android.view.SurfaceControl.RefreshRateRange")?;
        let hdr_sdr_ratio = r_f32(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse(
                "android.view.DisplayInfo.thermalRefreshRateThrottling (entries)",
            ));
        }
        let thermal_brightness_throttling_data_id = r_str8(p)?;
        Ok(DisplayInfo {
            layer_stack,
            flags,
            kind,
            display_id,
            display_group_id,
            name,
            unique_id,
            app_width,
            app_height,
            smallest_nominal_app_width,
            smallest_nominal_app_height,
            largest_nominal_app_width,
            largest_nominal_app_height,
            logical_width,
            logical_height,
            rotation,
            mode_id,
            render_frame_rate,
            has_arr_support,
            supported_refresh_rates,
            default_mode_id,
            user_preferred_mode_id,
            supported_modes,
            apps_supported_modes,
            color_mode,
            supported_color_modes,
            is_force_sdr,
            minimal_post_processing_supported,
            logical_density_dpi,
            physical_x_dpi,
            physical_y_dpi,
            app_vsync_offset_nanos,
            presentation_deadline_nanos,
            state,
            committed_state,
            owner_uid,
            owner_package_name,
            remove_mode,
            refresh_rate_override,
            brightness_minimum,
            brightness_maximum,
            brightness_default,
            brightness_dim,
            user_disabled_hdr_types,
            install_orientation,
            hdr_sdr_ratio,
            thermal_brightness_throttling_data_id,
            can_host_tasks: r_bool(p)?,
        })
    }
}

/// Parcel.writeInt(count) then each Display.Mode (DisplayInfo.writeToParcel's supportedModes / appsSupportedModes).
fn write_modes(p: &mut In<'_>, modes: &[DisplayMode]) -> Result<()> {
    w_i32(p, len_i32(modes.len())?)?;
    modes.iter().try_for_each(|m| m.write(p))
}

/// The counted Display.Mode array of readFromParcel: a negative count is refused (Java's `new Display.Mode[n]` fails).
fn read_modes(p: &In<'_>) -> Result<Vec<DisplayMode>> {
    let n = r_i32(p)?;
    let n = usize::try_from(n).map_err(|_| binder::StatusCode::BAD_VALUE)?;
    (0..n).map(|_| DisplayMode::read(p)).collect()
}

crate::codec!(DisplayInfo);
