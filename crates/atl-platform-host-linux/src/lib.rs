// SPDX-License-Identifier: MIT
//! GDK, the host side's implementation of atl-platform-host-api: the desktop's monitors through gtk4-rs.
//! GDK reports logical geometry and the refresh rate in milli-Hz (0: unknown); `Monitor::from_desktop` (atl-platform-types) converts both.
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
        Monitor::from_desktop(
            geometry.width(),
            geometry.height(),
            monitor.refresh_rate(),
            monitor.scale(),
        )
    }
}
