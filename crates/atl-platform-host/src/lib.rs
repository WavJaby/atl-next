// SPDX-License-Identifier: MIT
//! The platform layer's host-side facade (atld, xtask): the `atl-platform-host-api` trait plus the implementation the target OS selects at compile time, under neutral names.
//! Core crates depend on this crate only, so the choice of OS is made here and nowhere else (plan S1); `deny.toml` keeps GTK out of every other crate.
#![forbid(unsafe_code)]

pub use atl_platform_host_api::*;

#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::GdkDesktop as PlatformDesktop;

#[cfg(not(target_os = "linux"))]
compile_error!("atl-platform-host has no implementation for this target OS");

/// Every implementation item satisfies its api trait, checked at compile time.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn implements_the_api() {
    fn desktop<T: Desktop>() {}
    desktop::<PlatformDesktop>();
}

/// The first monitor's refresh rate in Hz, from a connection let go at once; `None` where the monitor reports none. `Err`: no desktop answers.
pub fn monitor_refresh_hz() -> Result<Option<f32>, HostError> {
    Ok(PlatformDesktop::connect()?.refresh_hz())
}
