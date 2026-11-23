// SPDX-License-Identifier: MIT
//! Linux, the host side's implementation of atl-platform-host-api: the desktop's monitors through gtk4-rs (GDK), the accelerometers through IIO (iio/: the kernel buffer or sysfs), the fonts through fontconfig (fonts.rs),
//! the memory through /proc/meminfo (memory.rs), the network through the kernel's route tables, resolv.conf and GIO's network monitor, its change events through rtnetlink (network.rs), the UI language from the locale variables (locale.rs), the time zone from `TZ` and `/etc/localtime` (timezone.rs), notifications through the session bus's freedesktop notification service (notifications.rs, zbus), per-app Wayland security contexts (wayland_context.rs), per-app PipeWire security contexts + atld as their permission manager (pipewire_context.rs).
//! GDK reports logical geometry and the refresh rate in milli-Hz (0: unknown); `Monitor::from_desktop` (atl-platform-types) converts both.
// `unsafe` only in pipewire_context.rs `security_context_create`: the SecurityContext extension has no pipewire-rs / pipewire-sys binding, so its bind + create go through the raw method tables
#![deny(unsafe_code)]

pub mod app_access;
mod data_dir;
mod fonts;
mod iio;
mod locale;
mod memory;
mod network;
mod notifications;
pub mod pipewire_context;
mod timezone;
pub mod wayland_context;
pub use data_dir::user_data_root;
pub use fonts::FcFonts;
pub use iio::IioSensors;
pub use locale::ui_locale;
pub use memory::HostMemoryInfo;
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
