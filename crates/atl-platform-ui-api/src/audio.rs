// SPDX-License-Identifier: MIT
//! The PCM output of the platform layer (plan A0): one stream of interleaved F32 samples that the audio server pulls from a callback on its realtime thread.
//! [`AudioHost`] is the connection to the server, [`PcmOutput`] one stream on it. One implementation (PipeWire, atl-platform-ui-linux), so the shape is that implementation's call sites: open, spec, set_active, rt_fallbacks, underruns.
//! No server type appears here; device choice is the server's (the default sink), not a parameter.

/// Frames per second and interleaved channels of a PCM stream. Samples are F32 in the server's native byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmSpec {
    /// Frames per second: the server's graph rate after negotiation.
    pub rate_hz: u32,
    /// Interleaved channels per frame.
    pub channels: u16,
}

/// Why an audio call failed. Failures reach the caller; they never turn into silence.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// No audio server to connect to: none running, or the named remote does not exist.
    #[error("audio server unavailable: {0}")]
    Unavailable(String),
    /// The stream could not be created, negotiated or started, or did not start in time (no sink linked).
    #[error("audio stream: {0}")]
    Stream(String),
}

/// The connection to the audio server.
pub trait AudioHost: Sized {
    /// One output stream on this connection.
    type Output: PcmOutput;

    /// Connects to the server. Fails when none is running.
    fn connect() -> Result<Self, AudioError>;

    /// Opens an output that pulls samples from `fill` on the server's realtime thread, in the rate and channels the server grants ([`PcmOutput::spec`]; `want` is the request).
    /// `fill` writes every sample of each slice it is given, in calls of one server quantum (at most 8192 frames), and keeps its state between calls.
    /// It runs on the realtime thread: it must not allocate, lock or block.
    /// The output starts inactive; [`PcmOutput::set_active`] starts it.
    fn open_output(
        &self,
        want: PcmSpec,
        fill: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Result<Self::Output, AudioError>;
}

/// One output stream. Dropping it stops and disconnects the stream.
pub trait PcmOutput {
    /// The format `fill` is called for: what the server granted.
    fn spec(&self) -> PcmSpec;

    /// Starts (`true`) or stops (`false`) the stream.
    /// `true` returns once the server runs the stream; a stream that fails or does not start in time is an error.
    fn set_active(&self, on: bool) -> Result<(), AudioError>;

    /// 1 when the stream's first realtime callback ran without SCHED_FIFO or SCHED_RR: the server's realtime request was refused (no rtkit, RLIMIT_RTPRIO 0), so callbacks run at normal priority and can underrun under load.
    /// Counted, not an error. 0 until that first callback ran.
    fn rt_fallbacks(&self) -> u64;

    /// Server cycles in which this stream had no buffer to fill: the sound was cut there, not stretched.
    /// Counted, not an error; 0 in a healthy run.
    fn underruns(&self) -> u64;
}
