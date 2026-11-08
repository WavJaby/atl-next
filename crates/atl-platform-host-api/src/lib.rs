// SPDX-License-Identifier: MIT
//! The platform layer's host side (atld, xtask, the system services) as traits: the desktop's monitors, read once at start: size, refresh rate, scale. Following the window to another monitor or scale is the app side's (the window knows its monitor; atld has no window).
//! Shaped by the one implementation, GDK on Linux (atl-platform-host-linux): the connection is thread-affine, and a monitor that reports no refresh rate answers `None`.
//! The machine's motion sensors ([`Sensors`]): shaped by the one implementation, Linux IIO. A stream per open, served by the kernel buffer where the environment enabled it and by sysfs reads otherwise ([`Tier`]); the caller paces its reads.
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
    /// Stable key for [`Sensors::open`] while the device exists.
    pub id: String,
    /// The device's own name.
    pub name: String,
    /// Largest magnitude one axis reports, m/s².
    pub max_range: f32,
    /// One raw step, m/s².
    pub resolution: f32,
    /// Shortest useful read interval of the tier in `tier`: the device's sample interval when buffered, the read span when polled.
    pub min_period: std::time::Duration,
    /// The tier [`Sensors::open`] serves, probed at listing; a stream opened later may fall back (see [`SampleStream::tier`]).
    pub tier: Tier,
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

    /// A stream of accelerometer `id`. An unusable buffer is not an error: the stream falls back to the baseline tier and says why.
    fn open(&self, id: &str) -> Result<Box<dyn SampleStream>, SensorError>;
}

/// How a stream gets its samples. The kernel buffer is the faster tier, used where the environment set it up; sysfs reads are the baseline every machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The kernel buffer: one scan per device sample, stamped by the kernel.
    Buffered,
    /// Sysfs reads, and why the buffer was not used.
    Polled(Fallback),
}

/// Why the buffered tier is not used; the label is the tier counter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The scan layout is not one the decoder reads.
    Layout,
    /// No trigger, or the named trigger is not on the machine.
    Trigger,
    /// The buffer is off: nothing has enabled it.
    NotEnabled,
    /// The timestamp clock is not one the kernel offers.
    Clock,
    /// This process may not open the char device.
    NodeDenied,
    /// Another reader holds the char device.
    NodeBusy,
    /// The buffer is on but no fresh scan arrives.
    NoData,
}

impl Fallback {
    /// The reason's label in the tier counter.
    pub fn label(self) -> &'static str {
        match self {
            Fallback::Layout => "layout",
            Fallback::Trigger => "trigger",
            Fallback::NotEnabled => "not-enabled",
            Fallback::Clock => "clock",
            Fallback::NodeDenied => "node-denied",
            Fallback::NodeBusy => "node-busy",
            Fallback::NoData => "no-data",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier::Buffered => f.write_str("buffered"),
            Tier::Polled(why) => write!(f, "polled ({})", why.label()),
        }
    }
}

/// One sample: the frame of [`Sensors`] and the instant it was measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// CLOCK_BOOTTIME, ns: Android's `elapsedRealtimeNanos`.
    pub boottime_ns: i64,
    /// x, y, z in the frame of [`Sensors`], m/s².
    pub values: [f32; 3],
}

/// One open reading of an accelerometer; dropping it releases the device.
pub trait SampleStream: Send {
    /// The tier serving this stream, decided when it was opened.
    fn tier(&self) -> Tier;

    /// Samples so far whose stamp was not in the stream's clock, each taken at arrival instead (the `ts-fallback` counter). Zero where stamps never leave their clock.
    fn ts_fallbacks(&self) -> u64;

    /// The newest sample not returned before. Returns as soon as one is ready, or `Ok(None)` after `timeout` with none; the polled tier returns after one read (its span is its interval). The caller paces one call per period and repeats its last sample on `None`.
    fn next(&mut self, timeout: std::time::Duration) -> Result<Option<Sample>, SensorError>;
}
