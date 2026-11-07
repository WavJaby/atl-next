// SPDX-License-Identifier: MIT
//! The PCM output and its mixer (AOSP AudioFlinger's PlaybackThread, Threads.cpp): one output on the platform layer, running from serve on (silence while no track plays), whose each cycle mixes every enabled track into the quantum (atl-audio-bridge, AudioMixerBase).
//! The output is opened first, so the rate and quantum it grants are the served profile. The mixer passes between the realtime fill and the control calls through [`MixCell`] (one atomic swap each way): the fill never locks or allocates, and a control call that finds the fill mixing waits for its return, one quantum at most.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use atl_audio_bridge::{Mix, MixCell, MixGuard, Track as Server};
use atl_platform_ui::PlatformAudioHost;
use atl_platform_ui::audio::{AudioHost, PcmOutput, PcmSpec};

use crate::Options;

/// The rate the output asks for: PipeWire's default graph rate (`default.clock.rate`). The server serves the rate the graph grants.
const REQUEST_RATE_HZ: u32 = 48_000;
/// Interleaved channels of the output: stereo (`OUTPUT_CHANNEL_MASK`).
const CHANNELS: u16 = 2;

/// What the output reports to AudioFlinger's clients: the rate and quantum the output granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    pub(crate) rate_hz: u32,
    pub(crate) quantum: u32,
}

impl Profile {
    /// Two quanta in ms, truncated: AudioFlinger's latency for a normal output. AudioSystem::calculateMinFrameCount divides it by one quantum in ms, so the minimum buffer is two quanta.
    pub(crate) fn latency_ms(&self) -> i32 {
        let ms = 2 * 1000 * u64::from(self.quantum) / u64::from(self.rate_hz);
        i32::try_from(ms).unwrap_or(i32::MAX)
    }
}

/// The mixer, shared by the output's realtime fill and the control calls (see [`MixCell`]). Installed when the output's quantum is known.
pub(crate) type Shared = Arc<MixCell>;

/// The open output. Dropping it stops the stream: the launcher holds it for the life of the process.
pub(crate) struct Output {
    _pcm: atl_platform_ui::PlatformPcmOutput,
}

/// Realtime cycles that played silence because a control call held the mixer, or it was not installed yet. Logged when the output stops.
static SILENT_CYCLES: AtomicU64 = AtomicU64::new(0);

impl Drop for Output {
    fn drop(&mut self) {
        tracing::info!(
            silent_cycles = SILENT_CYCLES.load(Ordering::Relaxed),
            "PCM output stopped"
        );
    }
}

/// Opens the output, starts it and installs the mixer; returns the profile AudioFlinger serves.
pub(crate) fn open(opts: Options) -> Result<(Output, Profile, Shared), String> {
    let host = PlatformAudioHost::connect().map_err(|e| e.to_string())?;
    let mixer: Shared = Arc::new(MixCell::default());
    let fill_mixer = Arc::clone(&mixer);
    let silent = opts.break_mix;
    let pcm = host
        .open_output(
            PcmSpec {
                rate_hz: REQUEST_RATE_HZ,
                channels: CHANNELS,
            },
            move |out| render(&fill_mixer, out, silent),
        )
        .map_err(|e| e.to_string())?;
    let spec = pcm.spec();
    let quantum = pcm.quantum();
    // a mixer of zero frames per cycle would never leave the realtime fill's loop
    if quantum == 0 {
        return Err("the server ran no cycle: no quantum to mix".to_owned());
    }
    let profile = Profile {
        rate_hz: spec.rate_hz,
        quantum,
    };
    let frames = usize::try_from(quantum).map_err(|e| e.to_string())?;
    if !mixer.install(Mix::new(frames, spec.rate_hz)) {
        return Err("the mixer is already open".to_owned());
    }
    pcm.set_active(true).map_err(|e| e.to_string())?;
    tracing::info!(rate = profile.rate_hz, quantum, "PCM output running");
    Ok((Output { _pcm: pcm }, profile, mixer))
}

/// The realtime fill: one slice of interleaved stereo frames from the mixer, or silence (break, a control call holding the mixer, or no mixer yet).
fn render(mixer: &MixCell, out: &mut [f32], silent: bool) {
    if silent {
        out.fill(0.0);
        return;
    }
    if !mixer.fill(out) {
        SILENT_CYCLES.fetch_add(1, Ordering::Relaxed);
        out.fill(0.0);
    }
}

/// The mixer, held by one control call. None before the output is open.
fn lock(mixer: &Shared) -> Option<MixGuard<'_>> {
    mixer.hold()
}

/// Adds a track's server side to the mix (disabled) as PCM 16-bit in `channel_mask`; its name, or None.
pub(crate) fn add_track(
    mixer: &Shared,
    track: Server,
    session: i32,
    channel_mask: u32,
) -> Option<i32> {
    lock(mixer)?.add(track, session, channel_mask)
}

/// Enables or disables track `name` in the mix.
pub(crate) fn set_active(mixer: &Shared, name: i32, on: bool) {
    if let Some(mut mix) = lock(mixer)
        && !mix.set_active(name, on)
    {
        tracing::error!(name, "track unknown to the mixer");
    }
}

/// Removes track `name` from the mix; its server side is dropped with it.
pub(crate) fn remove_track(mixer: &Shared, name: i32) {
    if let Some(mut mix) = lock(mixer) {
        mix.remove(name);
    }
}
