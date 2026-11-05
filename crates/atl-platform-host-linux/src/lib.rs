// SPDX-License-Identifier: MIT
//! GDK, the host side's implementation of atl-platform-host-api: the desktop's monitors through gtk4-rs.
//! GDK reports the refresh rate in milli-Hz, 0 when the monitor does not know it ([`millihz_to_hz`]).
#![forbid(unsafe_code)]

use std::marker::PhantomData;

use atl_platform_host_api::{Desktop, HostError, Monitor};
use gtk4::gdk;
use gtk4::prelude::*;

/// GTK connected by [`Desktop::connect`] on its thread. Not `Send`: GTK stays on that thread.
pub struct GdkDesktop {
    _not_send: PhantomData<*const ()>,
}

impl Desktop for GdkDesktop {
    fn connect() -> Result<Self, HostError> {
        gtk4::init().map_err(|e| HostError(e.to_string()))?;
        Ok(GdkDesktop {
            _not_send: PhantomData,
        })
    }

    /// The first monitor of the default display: Wayland has no primary monitor. GDK's geometry is logical pixels; the mode is that times the monitor's (fractional) scale.
    fn monitor(&self) -> Option<Monitor> {
        let display = gdk::Display::default()?;
        let monitor = display
            .monitors()
            .item(0)?
            .downcast::<gdk::Monitor>()
            .ok()?;
        let geometry = monitor.geometry();
        let scale = Some(monitor.scale()).filter(|s| s.is_finite() && *s > 0.0)?;
        Some(Monitor {
            width: logical_to_physical(geometry.width(), scale),
            height: logical_to_physical(geometry.height(), scale),
            refresh_hz: millihz_to_hz(monitor.refresh_rate()),
            scale,
        })
    }
}

/// A logical length at `scale` in physical pixels, rounded to the nearest (a 1600 px wide output at 1.6 is a 2560 px mode).
pub fn logical_to_physical(logical: i32, scale: f64) -> u32 {
    (f64::from(logical.max(0)) * scale).round() as u32
}

/// GDK's milli-Hz as Hz; `None` for 0 (unknown) and for a negative value.
pub fn millihz_to_hz(millihz: i32) -> Option<f32> {
    (millihz > 0).then(|| millihz as f32 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::{logical_to_physical, millihz_to_hz};

    /// GDK's `refresh_rate` is milli-Hz (60000 is 60 Hz), 0 when unknown.
    #[test]
    fn millihz_is_hz() {
        assert_eq!(millihz_to_hz(60_000), Some(60.0));
        assert_eq!(millihz_to_hz(120_000), Some(120.0));
        assert!((millihz_to_hz(59_940).expect("59.94 Hz") - 59.94).abs() < 1e-4);
        assert_eq!(millihz_to_hz(0), None);
        assert_eq!(millihz_to_hz(-1), None);
    }

    /// The owner's laptop: 1600x1000 logical at 1.6 is the 2560x1600 panel; 1.25 of 1229 is 1536.25 -> 1536; a negative length is none.
    #[test]
    fn logical_is_physical_at_scale() {
        assert_eq!(logical_to_physical(1600, 1.6), 2560);
        assert_eq!(logical_to_physical(1000, 1.6), 1600);
        assert_eq!(logical_to_physical(1229, 1.25), 1536);
        assert_eq!(logical_to_physical(-5, 2.0), 0);
    }
}
