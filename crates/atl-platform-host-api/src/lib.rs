// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask) as a trait: the desktop's monitors, read once at start: size, refresh rate, scale. Following the window to another monitor or scale is the app side's (the window knows its monitor; atld has no window).
//! Shaped by the one implementation, GDK on Linux (atl-platform-host-linux): the connection is thread-affine, and a monitor that reports no refresh rate answers `None`.
//! No OS or toolkit dependency, no unsafe; core crates use the `atl-platform-host` facade, not this crate or the implementation directly.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The desktop could not be reached: no display server answers, or the toolkit refused to start.
#[derive(Debug, thiserror::Error)]
#[error("desktop unavailable: {0}")]
pub struct HostError(
    /// The toolkit's reason.
    pub String,
);

/// The value types the app side shares (atld's monitor is the same type a window reports).
pub use atl_platform_types::{Monitor, PhysSize, Scale};

/// A connection to the desktop. Connect where the monitors are read; `monitor` answers from that connection.
pub trait Desktop: Sized {
    /// Connects on the calling thread.
    fn connect() -> Result<Self, HostError>;

    /// The first monitor (Wayland has no primary); `None` where the desktop lists none.
    fn monitor(&self) -> Option<Monitor>;
}
