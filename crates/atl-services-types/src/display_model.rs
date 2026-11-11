// SPDX-License-Identifier: MIT
//! The display's values, one struct for every reader: DisplayInfo (display.rs, IDisplayManager.getDisplayInfo) and the global Configuration (activity.rs). atld passes it to the services
//! as `--display WxH@hz,dpi` ([`DisplayModel::parse_arg`], [`DisplayModel::to_arg`]); without the flag atld takes [`DisplayModel::from_monitor`] of the desktop's monitor. That is the start value: the services keep the live one (tasks.rs `Tasks::display`, changed by registry SET_DISPLAY when the window's monitor changes), the app's compositor its SurfaceFlinger copy of the same monitor (atl-composer `Composer::set_display`).
//! The default (1280x800, 60 Hz, 160 dpi) is a test value: the fallback when no desktop answers, and the base of the gates' fixed display.

/// `DisplayMetrics.DENSITY_DEFAULT`: the dpi of density 1 (mdpi), where 1 dp is 1 px.
pub const DENSITY_DEFAULT: i32 = 160;

/// How the desktop composes the display: 8-bit RGBA in sRGB, no wide gamut (the window's buffer becomes a GSK texture). HAL values from system/core/libsystem/include/system/graphics-base-v1.0.h:
/// HAL_PIXEL_FORMAT_RGBA_8888, HAL_DATASPACE_STANDARD_BT709 (1 << 16), HAL_DATASPACE_TRANSFER_SRGB (2 << 22), HAL_DATASPACE_RANGE_FULL (1 << 27), and their sum HAL_DATASPACE_V0_SRGB.
/// Readers: IDisplayManager.getOverlaySupport (display.rs) and ISurfaceComposer.getCompositionPreference (atl-composer).
pub const COMPOSITION_PIXEL_FORMAT: i32 = 1;
pub const DATASPACE_STANDARD_BT709: i32 = 1 << 16;
pub const DATASPACE_TRANSFER_SRGB: i32 = 2 << 22;
pub const DATASPACE_RANGE_FULL: i32 = 1 << 27;
pub const COMPOSITION_DATASPACE: i32 =
    DATASPACE_STANDARD_BT709 | DATASPACE_TRANSFER_SRGB | DATASPACE_RANGE_FULL;

/// Surface.ROTATION_0..ROTATION_270 (Surface.java): clockwise quarter turns of the drawn image from the panel's natural orientation.
pub const ROTATION_0: i32 = 0;
const ROTATION_270: i32 = 3;

/// The default display: size in pixels as shown now (a quarter-turned panel's sides swapped: DisplayInfo.logicalWidth/Height), refresh rate in Hz, density in dots per inch (DisplayMetrics.densityDpi),
/// rotation (Surface.ROTATION_*, the compositor's output transform: platform layer `Monitor::rotation`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayModel {
    pub width: i32,
    pub height: i32,
    pub refresh_hz: f32,
    pub dpi: i32,
    pub rotation: i32,
}

impl Default for DisplayModel {
    /// 1280x800 at 60 Hz, 160 dpi (density 1), unrotated: a test value, not the desktop's.
    fn default() -> Self {
        DisplayModel {
            width: 1280,
            height: 800,
            refresh_hz: 60.0,
            dpi: DENSITY_DEFAULT,
            rotation: ROTATION_0,
        }
    }
}

impl DisplayModel {
    /// The monitor as Android's display: its mode in physical pixels, its rate (`None`: the default's), and density `160 x scale` (owner, 2026-11-05: 1 dp = 1 desktop logical pixel; the panel's
    /// physical DPI is not used). The scale is physical per logical pixel (Wayland `wp_fractional_scale_v1` / GDK monitor scale). `quarter_turns` is the rotation (clockwise, Surface.ROTATION_*), taken mod 4.
    pub fn from_monitor(
        width: u32,
        height: u32,
        refresh_hz: Option<f32>,
        scale: f64,
        quarter_turns: u8,
    ) -> Self {
        let clamp = |v: u32| i32::try_from(v).unwrap_or(i32::MAX).max(1);
        DisplayModel {
            width: clamp(width),
            height: clamp(height),
            refresh_hz: refresh_hz.unwrap_or(DisplayModel::default().refresh_hz),
            dpi: (f64::from(DENSITY_DEFAULT) * scale).round().max(1.0) as i32,
            rotation: i32::from(quarter_turns % 4),
        }
    }

    /// The panel's size in its natural orientation (DisplayInfo.getNaturalWidth/Height: the sides swap back for ROTATION_90 / 270): what a display mode reports (DisplayMode is the panel's, not the rotated view's).
    pub fn natural_size(&self) -> (i32, i32) {
        if self.rotation % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// `WxH@hz,dpi,rotation`, e.g. `1920x1080@59.94,240,1`. Sizes, rate and dpi must be positive, the rotation a Surface.ROTATION_* value (0..=3).
    pub fn parse_arg(s: &str) -> Result<Self, String> {
        let bad = || format!("{s}: not WxH@hz,dpi,rotation");
        let (size, rest) = s.split_once('@').ok_or_else(bad)?;
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let (hz, rest) = rest.split_once(',').ok_or_else(bad)?;
        let (dpi, rotation) = rest.split_once(',').ok_or_else(bad)?;
        let model = DisplayModel {
            width: w.parse().map_err(|_| bad())?,
            height: h.parse().map_err(|_| bad())?,
            refresh_hz: hz.parse().map_err(|_| bad())?,
            dpi: dpi.parse().map_err(|_| bad())?,
            rotation: rotation.parse().map_err(|_| bad())?,
        };
        // NaN fails the refresh-rate test too
        if model.width > 0
            && model.height > 0
            && model.refresh_hz > 0.0
            && model.dpi > 0
            && (ROTATION_0..=ROTATION_270).contains(&model.rotation)
        {
            Ok(model)
        } else {
            Err(bad())
        }
    }

    /// The `--display` form of this model: what atld passes on.
    pub fn to_arg(&self) -> String {
        format!(
            "{}x{}@{},{},{}",
            self.width, self.height, self.refresh_hz, self.dpi, self.rotation
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is the stated test value; the flag form of it is what atld forwards.
    #[test]
    fn default_is_the_stated_test_display() {
        let d = DisplayModel::default();
        assert_eq!(
            (d.width, d.height, d.refresh_hz, d.dpi),
            (1280, 800, 60.0, 160)
        );
        assert_eq!(d.to_arg(), "1280x800@60,160,0");
    }

    /// The owner's laptop (2560x1600, 120 Hz, scale 1.6): 1 dp = 1 logical px, so 160 x 1.6 = 256 dpi; scale 1 is mdpi (160); 1.25 is 200; a rate the monitor does not report keeps the default's.
    #[test]
    fn monitor_values_and_density_from_scale() {
        let m = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6, 0);
        assert_eq!(
            (m.width, m.height, m.refresh_hz, m.dpi),
            (2560, 1600, 120.0, 256)
        );
        assert_eq!(
            DisplayModel::from_monitor(1920, 1080, None, 1.0, 0).dpi,
            160
        );
        assert_eq!(
            DisplayModel::from_monitor(1920, 1080, None, 1.25, 0).dpi,
            200
        );
        assert_eq!(
            DisplayModel::from_monitor(1920, 1080, None, 1.0, 0).refresh_hz,
            DisplayModel::default().refresh_hz
        );
    }

    /// The laptop in portrait (a quarter turn): the shown size is 1600x2560, the panel's natural size stays 2560x1600; a half turn keeps the sides.
    #[test]
    fn rotation_and_natural_size() {
        let p = DisplayModel::from_monitor(1600, 2560, None, 1.6, 1);
        assert_eq!((p.rotation, p.natural_size()), (1, (2560, 1600)));
        let p = DisplayModel::from_monitor(1600, 2560, None, 1.6, 3);
        assert_eq!(p.natural_size(), (2560, 1600));
        let f = DisplayModel::from_monitor(2560, 1600, None, 1.6, 2);
        assert_eq!((f.rotation, f.natural_size()), (2, (2560, 1600)));
    }

    /// Fractional rates and other sizes parse; the form round-trips.
    #[test]
    fn parse_and_round_trip() {
        let m = DisplayModel::parse_arg("1920x1080@59.94,240,3").expect("parse");
        assert_eq!((m.width, m.height, m.dpi, m.rotation), (1920, 1080, 240, 3));
        assert!((m.refresh_hz - 59.94).abs() < 1e-4);
        assert_eq!(DisplayModel::parse_arg(&m.to_arg()).expect("parse"), m);
        assert_eq!(
            DisplayModel::parse_arg(&DisplayModel::default().to_arg()).expect("parse"),
            DisplayModel::default()
        );
    }

    /// Every malformed or non-positive value is refused, with the text it was given.
    #[test]
    fn refuses_malformed_and_non_positive() {
        for bad in [
            "",
            "1280x800",
            "1280x800@60",
            "1280x800@60,160",
            "1280@60,160,0",
            "1280x800@60,,0",
            "0x800@60,160,0",
            "1280x0@60,160,0",
            "1280x800@0,160,0",
            "1280x800@-1,160,0",
            "1280x800@60,0,0",
            "1280x800@nan,160,0",
            "1280x800@60,160,4",
            "1280x800@60,160,-1",
            "axb@60,160,0",
        ] {
            let e = DisplayModel::parse_arg(bad).expect_err(bad);
            assert!(e.starts_with(bad), "{e}");
        }
    }
}
