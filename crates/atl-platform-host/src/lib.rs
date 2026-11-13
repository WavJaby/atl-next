// SPDX-License-Identifier: MIT
//! The platform layer's host-side facade (atld, xtask, the system services): the `atl-platform-host-api` trait plus the implementation the target OS selects at compile time, under neutral names.
//! Core crates depend on this crate only, so the choice of OS is made here and nowhere else (plan S1); `deny.toml` keeps GTK out of every other crate.
#![forbid(unsafe_code)]

pub use atl_platform_host_api::*;

#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::FcFonts as PlatformFonts;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::GdkDesktop as PlatformDesktop;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::HostNetwork as PlatformNetwork;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::IioSensors as PlatformSensors;
/// What an app process's UI side reads from the desktop, as host paths the broker shows read-only at their own place: `wayland_socket()` (the compositor's) and `fontconfig_paths()` (config file + font dirs).
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::app_access;

#[cfg(not(target_os = "linux"))]
compile_error!("atl-platform-host has no implementation for this target OS");

/// Every implementation item satisfies its api trait, checked at compile time.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn implements_the_api() {
    fn desktop<T: Desktop>() {}
    desktop::<PlatformDesktop>();
    fn sensors<T: Sensors + Default>() {}
    sensors::<PlatformSensors>();
    fn fonts<T: FontSource>() {}
    fonts::<PlatformFonts>();
    fn network<T: NetworkSource + Default>() {}
    network::<PlatformNetwork>();
}

/// The first monitor, from a connection let go at once; `None` where the desktop lists none. `Err`: no desktop answers.
pub fn first_monitor() -> Result<Option<Monitor>, HostError> {
    Ok(PlatformDesktop::connect()?.monitor())
}
