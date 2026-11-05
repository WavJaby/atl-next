// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask) as a trait: the desktop's monitors, read once at start. Refresh rate only; the monitor's size and density follow with monitor changes (plan M3 step 5).
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

/// A connection to the desktop. Connect where the monitors are read; `refresh_hz` answers from that connection.
pub trait Desktop: Sized {
    /// Connects on the calling thread.
    fn connect() -> Result<Self, HostError>;

    /// The first monitor's refresh rate in Hz; `None` where the monitor reports none.
    fn refresh_hz(&self) -> Option<f32>;
}
