// SPDX-License-Identifier: MIT
//! The display's values, one struct for every reader: DisplayInfo (display.rs, IDisplayManager.getDisplayInfo) and the global Configuration (activity.rs). atld passes it to the services
//! as `--display WxH@hz,dpi` ([`DisplayModel::parse_arg`], [`DisplayModel::to_arg`]); without the flag the services use [`DisplayModel::default`]. Nothing else holds display values.
//! The default is a test value (1280x800 at 60 Hz, 160 dpi) until the platform layer supplies the desktop's display (M3 plan slice 6).

/// The default display: size in pixels, refresh rate in Hz, density in dots per inch (DisplayMetrics.densityDpi).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayModel {
    pub width: i32,
    pub height: i32,
    pub refresh_hz: f32,
    pub dpi: i32,
}

impl Default for DisplayModel {
    /// 1280x800 at 60 Hz, 160 dpi (density 1): a test value, not the desktop's.
    fn default() -> Self {
        DisplayModel {
            width: 1280,
            height: 800,
            refresh_hz: 60.0,
            dpi: 160,
        }
    }
}

impl DisplayModel {
    /// `WxH@hz,dpi`, e.g. `1920x1080@59.94,240`. Every number must be positive.
    pub fn parse_arg(s: &str) -> Result<Self, String> {
        let bad = || format!("{s}: not WxH@hz,dpi");
        let (size, rest) = s.split_once('@').ok_or_else(bad)?;
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let (hz, dpi) = rest.split_once(',').ok_or_else(bad)?;
        let model = DisplayModel {
            width: w.parse().map_err(|_| bad())?,
            height: h.parse().map_err(|_| bad())?,
            refresh_hz: hz.parse().map_err(|_| bad())?,
            dpi: dpi.parse().map_err(|_| bad())?,
        };
        // NaN fails the refresh-rate test too
        if model.width > 0 && model.height > 0 && model.refresh_hz > 0.0 && model.dpi > 0 {
            Ok(model)
        } else {
            Err(bad())
        }
    }

    /// The `--display` form of this model: what atld passes on.
    pub fn to_arg(&self) -> String {
        format!(
            "{}x{}@{},{}",
            self.width, self.height, self.refresh_hz, self.dpi
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
        assert_eq!(d.to_arg(), "1280x800@60,160");
    }

    /// Fractional rates and other sizes parse; the form round-trips.
    #[test]
    fn parse_and_round_trip() {
        let m = DisplayModel::parse_arg("1920x1080@59.94,240").expect("parse");
        assert_eq!((m.width, m.height, m.dpi), (1920, 1080, 240));
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
            "1280@60,160",
            "1280x800@60,",
            "0x800@60,160",
            "1280x0@60,160",
            "1280x800@0,160",
            "1280x800@-1,160",
            "1280x800@60,0",
            "1280x800@nan,160",
            "axb@60,160",
        ] {
            let e = DisplayModel::parse_arg(bad).expect_err(bad);
            assert!(e.starts_with(bad), "{e}");
        }
    }
}
