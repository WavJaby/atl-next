// SPDX-License-Identifier: MIT
//! The PCM output and its mixer (AOSP AudioFlinger's PlaybackThread, Threads.cpp): one output on the platform layer, whose each cycle mixes every enabled track into the quantum (atl-audio-bridge, AudioMixerBase).
//! The output opens on the first request that needs it (a profile query or createTrack), not at launch: AOSP opens an output when a client first asks for one, and an app that never plays starts without the audio server.
//! A server that cannot be reached fails those requests with AOSP's no-output answers (AudioFlinger.cpp:1155 BAD_VALUE, :1283-1337 zero), and the next request tries again.
//! The platform output is not Send (its PipeWire wrappers hold Rc): [`Opener`]'s thread owns it, and the binder threads ask that thread over a channel.
//! The mixer passes between the realtime fill and the control calls through [`MixCell`] (one atomic swap each way): the fill never locks or allocates, and a control call that finds the fill mixing waits for its return, one quantum at most.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use atl_audio_bridge::{Mix, MixCell, MixGuard, Track as Server};
use atl_platform_ui::PlatformAudioHost;
use atl_platform_ui::audio::{AudioHost, PcmOutput, PcmSpec};
use atl_services_types::counters;

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

/// What the binder threads read from an open output: its profile, and the mixer the tracks join.
#[derive(Clone)]
pub(crate) struct Opened {
    pub(crate) profile: Profile,
    pub(crate) mixer: Shared,
}

/// The open output, held by [`Opener`]'s thread. Dropping it stops the stream.
struct Output {
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

/// Asks the output's thread for [`Opened`]. A request opens the output if it is not open yet; a failed open answers that request with its reason, and the next request tries again.
pub(crate) struct Opener {
    requests: Sender<Sender<Result<Opened, String>>>,
}

impl Opener {
    /// Starts the output's thread for the server at `remote` (None: none granted). Opens nothing: the first [`Opener::get`] does.
    pub(crate) fn spawn(remote: Option<PathBuf>, opts: Options) -> Result<Self, String> {
        let (requests, inbox) = mpsc::channel();
        thread::Builder::new()
            .name("atl-pcm-output".to_owned())
            .spawn(move || owner(inbox, remote, opts))
            .map_err(|e| format!("starting the PCM output's thread: {e}"))?;
        Ok(Self { requests })
    }

    /// The open output, opened by this call if it is not open yet. Err: why it cannot be opened.
    pub(crate) fn get(&self) -> Result<Opened, String> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(reply)
            .map_err(|_| "the PCM output's thread has ended".to_owned())?;
        answer
            .recv()
            .map_err(|_| "the PCM output's thread has ended".to_owned())?
    }
}

/// The output's thread: opens the output on the first request, holds it until the process ends, and answers every request. It ends when no [`Opener`] is left.
fn owner(inbox: Receiver<Sender<Result<Opened, String>>>, remote: Option<PathBuf>, opts: Options) {
    let mut open: Option<(Output, Opened)> = None;
    for reply in inbox {
        let answer = match open.as_ref() {
            Some((_, opened)) => Ok(opened.clone()),
            None => match start(remote.as_deref(), opts) {
                Ok((output, opened)) => {
                    open = Some((output, opened.clone()));
                    Ok(opened)
                }
                Err(reason) => {
                    counters::refused("PcmOutput", "open");
                    tracing::warn!(%reason, "PCM output unavailable");
                    Err(reason)
                }
            },
        };
        // the caller may have given up (its binder call ended): then nothing is waiting for the answer
        let _ = reply.send(answer);
    }
}

/// Connects to the server at `remote`, opens and starts the output, and installs the mixer for the quantum the server ran.
fn start(remote: Option<&Path>, opts: Options) -> Result<(Output, Opened), String> {
    let remote = remote.ok_or_else(|| {
        "no audio server socket granted: atld found no PipeWire socket on the desktop".to_owned()
    })?;
    let host = PlatformAudioHost::connect(remote).map_err(|e| e.to_string())?;
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
    Ok((Output { _pcm: pcm }, Opened { profile, mixer }))
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
