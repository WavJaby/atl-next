// SPDX-License-Identifier: MIT
//! The platform layer's app-process facade: the `atl-platform-ui-api` traits and types, plus the implementation for the target OS as [`PlatformUi`] and [`PlatformWindow`].
//! Core crates depend on this crate only, so the choice of OS is made here and nowhere else (plan S1); `deny.toml` keeps GTK out of every other crate.
#![forbid(unsafe_code)]

pub use atl_platform_ui_api::*;

#[cfg(target_os = "linux")]
pub use atl_platform_ui_linux::{GtkUi as PlatformUi, GtkWindow as PlatformWindow};

#[cfg(not(target_os = "linux"))]
compile_error!("atl-platform-ui has no implementation for this target OS");

/// Every implementation item satisfies its api trait, checked at compile time.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn implements_the_api() {
    fn ui<T: Ui>() {}
    fn window<T: Window<Ui = PlatformUi> + FrameClock + InputSource>() {}
    ui::<PlatformUi>();
    window::<PlatformWindow>();
}
