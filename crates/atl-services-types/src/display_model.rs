// SPDX-License-Identifier: MIT
//! The display's values, one struct for every reader: DisplayInfo (display.rs, IDisplayManager.getDisplayInfo) and the global Configuration (activity.rs). atld passes it to the services
//! The services own it as [`DisplayModel::of`] of two inputs: the monitor (atld `--monitor` in the [`DisplayModel::parse_arg`] form, else [`DisplayModel::from_monitor`] of the desktop's first monitor; then each window's report, registry REPORT_MONITOR) and the declaration (`--declare-display`, [`Declaration`]; none by default).
//! tasks.rs `Tasks::display` holds it; the app's compositor keeps the copy its REPORT_MONITOR reply carries (atl-composer `Composer::set_display`).
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

/// The compositor's latch offset from vsync (DisplayMode.sfVsyncOffset): it latches at the vsync tick itself.
pub const SF_VSYNC_OFFSET_NS: i64 = 0;
/// The app vsync's offset from the display's (DisplayMode.appVsyncOffset, Display.getAppVsyncOffsetNanos): app events go out at the tick itself.
pub const APP_VSYNC_OFFSET_NS: i64 = 0;
/// SurfaceFlinger's processing slack in presentationDeadline (SurfaceFlinger.cpp `getDynamicDisplayInfoInternal`: "an additional 1ms").
const PRESENTATION_DEADLINE_SLACK_NS: i64 = 1_000_000;

/// The period of `refresh_hz`, ns, rounded to the nearest.
pub fn period_ns(refresh_hz: f32) -> i64 {
    (1e9 / f64::from(refresh_hz)).round() as i64
}

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
    /// The monitor as Android's display: its mode in physical pixels, its rate (`None`: the default's), and density `160 x scale` (maintainer, 2026-11-05: 1 dp = 1 desktop logical pixel; the panel's
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

    /// The vsync period, ns: [`period_ns`] of the refresh rate.
    pub fn vsync_period_ns(&self) -> i64 {
        period_ns(self.refresh_hz)
    }

    /// How far before a vsync a buffer must be queued to be shown at it (DisplayMode.presentationDeadline, Display.getPresentationDeadlineNanos): SurfaceFlinger.cpp `getDynamicDisplayInfoInternal`,
    /// period - sfVsyncOffset + 1 ms.
    pub fn presentation_deadline_ns(&self) -> i64 {
        self.vsync_period_ns() - SF_VSYNC_OFFSET_NS + PRESENTATION_DEADLINE_SLACK_NS
    }

    /// The panel's size in its natural orientation (DisplayInfo.getNaturalWidth/Height: the sides swap back for ROTATION_90 / 270): what a display mode reports (DisplayMode is the panel's, not the rotated view's).
    pub fn natural_size(&self) -> (i32, i32) {
        if self.rotation % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// The display shown at quarter turns `rotation` from the panel's natural size: the odd turns swap the sides (DisplayContent.updateDisplayAndOrientation:2276-2290), `rotation` is set.
    /// Built from the natural size, so a turned display turns to any rotation (the owner turns the monitor's `of` by its own rotation, tasks.rs).
    pub fn turned(self, rotation: i32) -> Self {
        let (width, height) = self.natural_size();
        let (width, height) = if rotation % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };
        DisplayModel {
            width,
            height,
            rotation,
            ..self
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

    /// The `WxH@hz,dpi,rotation` form of this model: what atld passes on as `--monitor`, and what the registry carries.
    pub fn to_arg(&self) -> String {
        format!(
            "{}x{}@{},{},{}",
            self.width, self.height, self.refresh_hz, self.dpi, self.rotation
        )
    }
}

/// A display declared in dp: the app's whole device display, `411x914` is a 1080x2400 panel at 420 dpi. `--declare-display WxH`; none by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declaration {
    pub width_dp: i32,
    pub height_dp: i32,
}

/// The shortest and longest side of a display, px: AOSP's OverlayDisplayAdapter MIN_WIDTH..MAX_WIDTH and MIN_HEIGHT..MAX_HEIGHT.
pub const MIN_PX: i32 = 100;
pub const MAX_PX: i32 = 4096;

impl Declaration {
    /// `WxH` in dp, both positive: `411x914`.
    pub fn parse_arg(s: &str) -> Result<Self, String> {
        let bad = || format!("{s}: not WxH in dp");
        let (w, h) = s.split_once('x').ok_or_else(bad)?;
        let declared = Declaration {
            width_dp: w.parse().map_err(|_| bad())?,
            height_dp: h.parse().map_err(|_| bad())?,
        };
        if declared.width_dp > 0 && declared.height_dp > 0 {
            Ok(declared)
        } else {
            Err(bad())
        }
    }

    /// The `WxH` form this parses from.
    pub fn to_arg(&self) -> String {
        format!("{}x{}", self.width_dp, self.height_dp)
    }
}

impl DisplayModel {
    /// The display every reader takes: the monitor itself with no declaration. Declared: the dp size at the monitor's density (px = round(dp x dpi / 160)), the monitor's dpi and refresh,
    /// rotation 0 (the monitor's turn is how the panel is mounted, invisible to the app). Refused when a side falls outside MIN_PX..=MAX_PX.
    pub fn of(monitor: DisplayModel, declared: Option<Declaration>) -> Result<Self, String> {
        let Some(declared) = declared else {
            return Ok(monitor);
        };
        let px = |dp: i32| {
            (f64::from(dp) * f64::from(monitor.dpi) / f64::from(DENSITY_DEFAULT)).round() as i32
        };
        let (width, height) = (px(declared.width_dp), px(declared.height_dp));
        if ![width, height]
            .into_iter()
            .all(|side| (MIN_PX..=MAX_PX).contains(&side))
        {
            return Err(format!(
                "{} at {} dpi is {width}x{height} px, outside {MIN_PX}..={MAX_PX}",
                declared.to_arg(),
                monitor.dpi
            ));
        }
        Ok(DisplayModel {
            width,
            height,
            refresh_hz: monitor.refresh_hz,
            dpi: monitor.dpi,
            rotation: ROTATION_0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1e9 / 60 = 16 666 666.67 ns, which rounds up; 1e9 / 50 is exact. The deadline at 60 Hz is SurfaceFlinger's period - 0 + 1 ms = 17 666 667 ns.
    #[test]
    fn period_and_deadline() {
        assert_eq!(period_ns(60.0), 16_666_667);
        assert_eq!(period_ns(50.0), 20_000_000);
        assert_eq!(
            DisplayModel::default().presentation_deadline_ns(),
            17_666_667
        );
    }

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

    /// The maintainer's laptop (2560x1600, 120 Hz, scale 1.6): 1 dp = 1 logical px, so 160 x 1.6 = 256 dpi; scale 1 is mdpi (160); 1.25 is 200; a rate the monitor does not report keeps the default's.
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

    /// Hand values: 658x1462 at 120 Hz, 256 dpi turned a quarter is 1462x658 with rotation 1, whose natural size is 658x1462 again. Turned to 0 or to its own rotation it is the same model;
    /// a half turn keeps the sides.
    #[test]
    fn turned_swaps_the_sides_of_the_natural_size() {
        let unturned = DisplayModel {
            width: 658,
            height: 1462,
            refresh_hz: 120.0,
            dpi: 256,
            rotation: 0,
        };
        let turned = unturned.turned(1);
        assert_eq!(
            turned,
            DisplayModel {
                width: 1462,
                height: 658,
                refresh_hz: 120.0,
                dpi: 256,
                rotation: 1,
            }
        );
        assert_eq!(turned.natural_size(), (658, 1462));
        assert_eq!(turned.turned(0), unturned);
        assert_eq!(unturned.turned(0), unturned);
        assert_eq!(turned.turned(1), turned);
        assert_eq!(
            (unturned.turned(2).width, unturned.turned(2).height),
            (658, 1462)
        );
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

    /// The laptop (2560x1600 at scale 1.6: 256 dpi, 120 Hz) declared 411x914 dp: 411 x 256 / 160 = 657.6 -> 658 px, 914 x 256 / 160 = 1462.4 -> 1462 px. Back by the DisplayContent
    /// formula dp = px x 160 / dpi: 658 -> 411.25 -> 411, 1462 -> 913.75 -> 914.
    #[test]
    fn declared_size_at_the_monitor_density() {
        let monitor = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6, 0);
        let d = DisplayModel::of(
            monitor,
            Some(Declaration {
                width_dp: 411,
                height_dp: 914,
            }),
        )
        .expect("in range");
        assert_eq!(
            (d.width, d.height, d.dpi, d.refresh_hz, d.rotation),
            (658, 1462, 256, 120.0, 0)
        );
        let back = |px: i32| (f64::from(px) * 160.0 / f64::from(d.dpi)).round() as i32;
        assert_eq!((back(d.width), back(d.height)), (411, 914));
    }

    /// At scale 1 (160 dpi) the declared dp are the px. A monitor turned a quarter turn keeps the declared sides and gets rotation 0.
    #[test]
    fn declared_size_at_mdpi_ignores_the_monitor_turn() {
        let monitor = DisplayModel::from_monitor(1080, 1920, None, 1.0, 1);
        let d = DisplayModel::of(
            monitor,
            Some(Declaration {
                width_dp: 411,
                height_dp: 914,
            }),
        )
        .expect("in range");
        assert_eq!((d.width, d.height, d.dpi, d.rotation), (411, 914, 160, 0));
    }

    /// No declaration: the display is the monitor itself, turn and refresh included.
    #[test]
    fn no_declaration_is_the_monitor() {
        let monitor = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6, 1);
        assert_eq!(DisplayModel::of(monitor, None), Ok(monitor));
    }

    /// The overlay bounds: 100 and 4096 px are accepted on each side, one px beyond either bound is refused. At mdpi px = dp.
    #[test]
    fn declared_sides_are_bounded_as_overlay_displays() {
        let mdpi = DisplayModel::from_monitor(1920, 1080, None, 1.0, 0);
        let at = |width_dp, height_dp| {
            DisplayModel::of(
                mdpi,
                Some(Declaration {
                    width_dp,
                    height_dp,
                }),
            )
        };
        assert!(at(100, 914).is_ok());
        assert!(at(4096, 4096).is_ok());
        assert!(at(99, 914).is_err());
        assert!(at(411, 4097).is_err());
    }

    /// `411x914` parses in dp; a missing, malformed or non-positive side is refused with the text given.
    #[test]
    fn declaration_parses_and_refuses() {
        let d = Declaration::parse_arg("411x914").expect("parse");
        assert_eq!((d.width_dp, d.height_dp), (411, 914));
        assert_eq!(d.to_arg(), "411x914");
        for bad in ["", "411", "411x", "0x914", "411x-1", "411x914x1", "axb"] {
            let e = Declaration::parse_arg(bad).expect_err(bad);
            assert!(e.starts_with(bad), "{e}");
        }
    }
}
