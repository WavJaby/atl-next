// SPDX-License-Identifier: MIT
//! The platform layer's host-side facade (atld, xtask, the system services): the `atl-platform-host-api` trait plus the implementation the target OS selects at compile time, under neutral names.
//! Core crates depend on this crate only, so the choice of OS is made here and nowhere else (plan S1); `deny.toml` keeps GTK out of every other crate.
#![forbid(unsafe_code)]

pub use atl_platform_host_api::*;

#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::FcFonts as PlatformFonts;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::FdoNotifications as PlatformNotifications;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::GdkDesktop as PlatformDesktop;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::HostMemoryInfo as PlatformMemory;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::HostNetwork as PlatformNetwork;
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::IioSensors as PlatformSensors;
/// What an app process's UI side reads from the desktop, as host paths the broker shows read-only at their own place: `wayland_socket()` (the compositor's) and `fontconfig_paths()` (config file + font dirs).
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::app_access;
/// The desktop's time zone id as a native program resolves it: `TZ`, else the zone `/etc/localtime` links to; `None` where it names none.
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::time_zone;
/// The desktop's UI language as one BCP-47 tag, from the process's locale variables (`LC_ALL`, `LC_MESSAGES`, `LANG`); `None` for C/POSIX, none set, or a malformed name.
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::ui_locale;
/// The user's persistent data root (atld `--data-root` default) in the desktop's per-user data dir (Linux: `$XDG_DATA_HOME/atl`, else `~/.local/share/atl`); `None` where the OS names no such dir.
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::user_data_root;
/// Per-app-process Wayland security contexts (`wp_security_context_v1`): a listening socket the compositor serves with privileged globals withheld; probed, so a compositor without the protocol gives `Probe::Unavailable`.
#[cfg(target_os = "linux")]
pub use atl_platform_host_linux::wayland_context;

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
    fn memory<T: MemorySource + Default>() {}
    memory::<PlatformMemory>();
    fn notifications<T: Notifications>() {}
    notifications::<PlatformNotifications>();
}

/// The first monitor, from a connection let go at once; `None` where the desktop lists none. `Err`: no desktop answers.
pub fn first_monitor() -> Result<Option<Monitor>, HostError> {
    Ok(PlatformDesktop::connect()?.monitor())
}
