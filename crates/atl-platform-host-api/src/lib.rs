// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask, the system services) as traits: the desktop's monitors, read once at start: size, refresh rate, scale. Following the window to another monitor or scale is the app side's (the window knows its monitor; atld has no window).
//! Shaped by the one implementation, GDK on Linux (atl-platform-host-linux): the connection is thread-affine, and a monitor that reports no refresh rate answers `None`.
//! The machine's motion sensors ([`Sensors`]): shaped by the one implementation, Linux IIO sysfs, which is read on demand (no stream to start or stop), so the caller polls at its own rate.
//! The desktop's fonts ([`FontSource`]): which faces, in what order, for which language; read when atld starts.
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

/// The desktop's font configuration could not be read.
#[derive(Debug, thiserror::Error)]
#[error("fonts unavailable: {0}")]
pub struct FontError(
    /// The source's reason.
    pub String,
);

/// A question to the desktop's font configuration: a family name or generic (`sans-serif`, `monospace`, ...), optionally narrowed to faces covering a language and to a width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontQuery {
    /// Family name or generic alias, resolved by the desktop's rules; empty = any family.
    pub family: String,
    /// The desktop's language id (fontconfig's: `he`, `zh-tw`, `und-zsye`).
    pub lang: Option<String>,
    /// Width, 100 = normal, 75 = condensed (fontconfig's scale = OS/2 `usWidthClass` percentages).
    pub width: Option<i32>,
}

/// One face of a font file the desktop knows. What the face is (weight, axes, names) is read from the file itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Face {
    /// The font file.
    pub path: std::path::PathBuf,
    /// Face index inside a collection (`.ttc`); 0 for a single-face file.
    pub index: u32,
    /// The family name the desktop lists it under (first one).
    pub family: String,
    /// The desktop's width value of the face.
    pub width: i32,
}

/// The desktop's font configuration (plan C7: Android's font list follows the desktop). Shaped by its one implementation, fontconfig: a sorted preference list per query and a plain listing; named instances of a variable font are not separate faces.
pub trait FontSource {
    /// Faces in the desktop's preference order for `q` (best first); fonts not matching `q` follow, as the desktop's fallback order.
    fn sorted(&self, q: &FontQuery) -> Result<Vec<Face>, FontError>;

    /// Every face that matches `q` exactly (family name, covered language, width), unordered.
    fn list(&self, q: &FontQuery) -> Result<Vec<Face>, FontError>;
}

/// The machine's motion sensors. Values are Android's `SensorEvent` frame of the display's natural orientation (x right, y up the screen, z out of the screen; at rest the axis pointing up reads +g), m/s², gravity included: the source has applied its scale, mounting and frame conversion.
/// Only sensors in the display count (a laptop base's accelerometer is not the display's without hinge-angle compensation). Values never follow the display rotation: apps remap with `Display.getRotation()`.
pub trait Sensors: Send + Sync {
    /// The accelerometers present now; empty when there are none.
    fn accelerometers(&self) -> Vec<Accelerometer>;

    /// One sample of accelerometer `id`: x, y, z.
    fn read_accel(&self, id: &str) -> Result<[f32; 3], SensorError>;
}
