// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask, the system services) as traits: the desktop's monitors, read once at start: size, refresh rate, scale. Following the window to another monitor or scale is the app side's (the window knows its monitor; atld has no window).
//! Shaped by the one implementation, GDK on Linux (atl-platform-host-linux): the connection is thread-affine, and a monitor that reports no refresh rate answers `None`.
//! The machine's motion sensors ([`Sensors`]): shaped by the one implementation, Linux IIO sysfs, which is read on demand (no stream to start or stop), so the caller polls at its own rate.
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

/// A sensor could not be read (the device went away, or answered something unparsable).
#[derive(Debug, thiserror::Error)]
#[error("sensor unavailable: {0}")]
pub struct SensorError(
    /// The source's reason.
    pub String,
);

/// One accelerometer of the machine, as listed by [`Sensors::accelerometers`].
#[derive(Debug, Clone, PartialEq)]
pub struct Accelerometer {
    /// Stable key for [`Sensors::read_accel`] while the device exists.
    pub id: String,
    /// The device's own name.
    pub name: String,
    /// Largest magnitude one axis reports, m/s².
    pub max_range: f32,
    /// One raw step, m/s².
    pub resolution: f32,
    /// Shortest useful read interval.
    pub min_period: std::time::Duration,
}

/// The machine's motion sensors. Values are in the device frame of the machine's natural orientation (x right, y up the screen, z out of the screen), m/s², gravity included: the source has applied its scale and mounting.
pub trait Sensors: Send + Sync {
    /// The accelerometers present now; empty when there are none.
    fn accelerometers(&self) -> Vec<Accelerometer>;

    /// One sample of accelerometer `id`: x, y, z.
    fn read_accel(&self, id: &str) -> Result<[f32; 3], SensorError>;
}
