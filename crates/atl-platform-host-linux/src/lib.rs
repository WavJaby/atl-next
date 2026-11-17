// SPDX-License-Identifier: MIT
//! Linux, the host side's implementation of atl-platform-host-api: the desktop's monitors through gtk4-rs (GDK), the accelerometers through IIO (iio/: the kernel buffer or sysfs), the fonts through fontconfig (fonts.rs),
//! the network through the kernel's route tables, resolv.conf and GIO's network monitor (network.rs), the UI language from the locale variables (locale.rs), the time zone from `TZ` and `/etc/localtime` (timezone.rs), notifications through the session bus's freedesktop notification service (notifications.rs, zbus).
//! GDK reports logical geometry and the refresh rate in milli-Hz (0: unknown); `Monitor::from_desktop` (atl-platform-types) converts both.
#![forbid(unsafe_code)]

pub mod app_access;
mod data_dir;
mod fonts;
mod iio;
mod locale;
mod network;
mod notifications;
mod timezone;
pub use data_dir::user_device;
pub use fonts::FcFonts;
pub use iio::IioSensors;
pub use locale::ui_locale;
pub use network::HostNetwork;
pub use notifications::FdoNotifications;
pub use timezone::time_zone;

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
        Monitor::from_desktop(
            geometry.width(),
            geometry.height(),
            monitor.refresh_rate(),
            monitor.scale(),
        )
    }
}
