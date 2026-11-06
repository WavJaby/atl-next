// SPDX-License-Identifier: MIT
//! Value types both platform halves share: the app side (atl-platform-ui-api: a window's monitor) and the host side (atl-platform-host-api: the desktop's monitors for atld). One definition, so the two sides cannot disagree on a monitor.
//! Sizes are physical pixels with an explicit [`Scale`]; [`Monitor::from_desktop`] is the one conversion from a desktop's report (logical size, milli-Hz, scale factor), used by every `-<os>` implementation.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// A length in physical (device) pixels: a window's content area, a buffer's extent, a monitor's mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysSize {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// Physical pixels per desktop logical pixel (Wayland buffer scale, possibly fractional). Always finite and positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale(f64);

impl Scale {
    /// One physical pixel per logical pixel.
    pub const ONE: Scale = Scale(1.0);

    /// `None` unless `factor` is finite and greater than zero.
    pub fn new(factor: f64) -> Option<Scale> {
        (factor.is_finite() && factor > 0.0).then_some(Scale(factor))
    }

    /// The factor.
    pub fn get(self) -> f64 {
        self.0
    }

    /// `physical` pixels expressed in logical pixels.
    pub fn to_logical(self, physical: f64) -> f64 {
        physical / self.0
    }

    /// `logical` pixels expressed in physical pixels.
    pub fn to_physical(self, logical: f64) -> f64 {
        logical * self.0
    }
}

/// How the desktop turns the drawn image on the panel, from the panel's natural orientation, clockwise: Android's `Surface.ROTATION_*` sense ("the rotation of the drawn graphics on the screen,
/// the opposite direction of the physical rotation of the device", Display.getRotation docs). A mirroring transform reports its rotation part (Android has no mirrored display).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    /// The natural orientation.
    #[default]
    Deg0,
    /// A quarter turn clockwise (Surface.ROTATION_90).
    Deg90,
    /// Upside down.
    Deg180,
    /// Three quarter turns clockwise (Surface.ROTATION_270).
    Deg270,
}

impl Rotation {
    /// Clockwise quarter turns, 0..=3: the value of `Surface.ROTATION_*`.
    pub fn quarter_turns(self) -> u8 {
        match self {
            Rotation::Deg0 => 0,
            Rotation::Deg90 => 1,
            Rotation::Deg180 => 2,
            Rotation::Deg270 => 3,
        }
    }
}

/// A monitor as the desktop reports it: its mode in physical pixels (as the desktop shows it, so a quarter-turned panel's sides are swapped), its refresh rate, its scale, its rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    /// The mode in physical pixels.
    pub size: PhysSize,
    /// Refresh rate in Hz; `None` where the monitor reports none.
    pub refresh_hz: Option<f32>,
    /// Physical pixels per desktop logical pixel.
    pub scale: Scale,
    /// [`Rotation::Deg0`] from [`Monitor::from_desktop`]: the implementation sets it where the desktop reports one.
    pub rotation: Rotation,
}

impl Monitor {
    /// A desktop's report as a monitor: `logical_width` x `logical_height` at `scale` (each side rounded to the nearest physical pixel; a negative side is 0), `millihz` (0 or negative: unknown). `None` for an unusable scale (see [`Scale::new`]).
    pub fn from_desktop(
        logical_width: i32,
        logical_height: i32,
        millihz: i32,
        scale: f64,
    ) -> Option<Monitor> {
        let scale = Scale::new(scale)?;
        let physical = |logical: i32| scale.to_physical(f64::from(logical.max(0))).round() as u32;
        Some(Monitor {
            size: PhysSize {
                width: physical(logical_width),
                height: physical(logical_height),
            },
            refresh_hz: (millihz > 0).then(|| millihz as f32 / 1000.0),
            scale,
            rotation: Rotation::Deg0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scale must be a usable divisor: zero, negative and non-finite factors are refused.
    #[test]
    fn scale_refuses_unusable_factors() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(Scale::new(bad), None, "{bad}");
        }
        assert_eq!(Scale::new(1.5).map(Scale::get), Some(1.5));
    }

    /// 1.5x: 300 physical pixels are 200 logical ones, and back (arithmetic, 300 / 1.5 = 200).
    #[test]
    fn scale_converts_both_ways() {
        let s = Scale::new(1.5).expect("valid");
        assert_eq!(s.to_logical(300.0), 200.0);
        assert_eq!(s.to_physical(200.0), 300.0);
        assert_eq!(Scale::ONE.to_logical(7.0), 7.0);
    }

    fn mode(m: Option<Monitor>) -> Option<(u32, u32)> {
        m.map(|m| (m.size.width, m.size.height))
    }

    /// The owner's laptop: 1600x1000 logical at 1.6 is the 2560x1600 panel; 1.25 of 1229 is 1536.25 -> 1536; a negative side is 0; an unusable scale is no monitor.
    #[test]
    fn logical_is_physical_at_scale() {
        assert_eq!(
            mode(Monitor::from_desktop(1600, 1000, 0, 1.6)),
            Some((2560, 1600))
        );
        assert_eq!(
            mode(Monitor::from_desktop(1229, -5, 0, 1.25)),
            Some((1536, 0))
        );
        assert_eq!(Monitor::from_desktop(1, 1, 0, 0.0), None);
    }

    /// GDK's and Wayland's refresh rate is milli-Hz (60000 is 60 Hz), 0 when unknown.
    #[test]
    fn millihz_is_hz() {
        let hz = |mhz| Monitor::from_desktop(1, 1, mhz, 1.0).and_then(|m| m.refresh_hz);
        assert_eq!(hz(60_000), Some(60.0));
        assert_eq!(hz(120_000), Some(120.0));
        assert!((hz(59_940).expect("59.94 Hz") - 59.94).abs() < 1e-4);
        assert_eq!(hz(0), None);
        assert_eq!(hz(-1), None);
    }
}
