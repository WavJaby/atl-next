// SPDX-License-Identifier: MIT
//! The PCM output and its mixer (AOSP AudioFlinger's PlaybackThread, Threads.cpp): one output on the platform layer, whose each cycle mixes every enabled track into the quantum (atl-audio-bridge, AudioMixerBase).
//! The output opens on the first request that needs it (a profile query or createTrack), not at launch: AOSP opens an output when a client first asks for one, and an app that never plays starts without the audio server.
//! A server that cannot be reached fails those requests with AOSP's no-output answers (AudioFlinger.cpp:1155 BAD_VALUE, :1283-1337 zero), and the next request tries again.
//! The platform output is not Send (its PipeWire wrappers hold Rc): [`Opener`]'s thread owns it, and the binder threads ask that thread over a channel.
//! The mixer passes between the realtime fill and the control calls through [`MixCell`] (one atomic swap each way): the fill never locks or allocates, and a control call that finds the fill mixing waits for its return, one quantum at most.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use atl_audio_bridge::{Control, Mix, MixCell, MixGuard, Track as Server};
use atl_platform_ui::PlatformAudioHost;
use atl_platform_ui::audio::{AudioHost, PcmOutput, PcmSpec, StreamIdentity};

use crate::Options;
use crate::volume::{AppVolume, Report};

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
    pcm: atl_platform_ui::PlatformPcmOutput,
}

/// Realtime cycles that played silence because a control call held the mixer, or it was not installed yet. Logged when the output stops.
static SILENT_CYCLES: AtomicU64 = AtomicU64::new(0);

/// Mix evidence since the last counters line, written by the realtime fill (relaxed adds only) and taken by the output's thread.
/// requested = frames the output asked for; mixed = frames the mixer rendered; audible = mixed frames with a nonzero sample; peak = largest |sample| as f32 bits (fetch_max on bits orders non-negative floats).
static REQUESTED: AtomicU64 = AtomicU64::new(0);
static MIXED: AtomicU64 = AtomicU64::new(0);
static AUDIBLE: AtomicU64 = AtomicU64::new(0);
static PEAK_BITS: AtomicU32 = AtomicU32::new(0);
/// Frames the client wrote that never reached the mix: unread when their track was removed, or discarded by a flush. Separates "dropped by the server" from "never written" when a recording comes up short.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// At most one counters line per this period (the diag lines' rate class: once per second at most).
const COUNTERS_PERIOD: Duration = Duration::from_secs(1);

/// One counters line `audio-mix: requested=N mixed=N from_tracks=N audible=N peak=F active=N muted=N dropped=N underruns=N rt_fallbacks=N` with the deltas since the last (active, muted and
/// rt_fallbacks are levels) and the tracks' side `track_underruns=N track_underrun_frames=N`; written while the output is open, zeros when silent. underruns = server cycles that found no buffer of `pcm`'s
/// (the sound cut there; delta of [`PcmOutput::underruns`] against `underruns_seen`); rt_fallbacks = 1 when its callbacks run without a realtime grant. track_underruns = underruns the tracks' clients
/// saw begin (their getXRunCount steps), track_underrun_frames = frames the mixer asked of a playing track and got none for: a track that keeps the mix short shows here, not only as from_tracks < requested.
fn log_counters(pcm: &atl_platform_ui::PlatformPcmOutput, underruns_seen: &mut u64) {
    let total = pcm.underruns();
    let underruns = total.saturating_sub(*underruns_seen);
    *underruns_seen = total;
    let rt_fallbacks = pcm.rt_fallbacks();
    let requested = REQUESTED.swap(0, Ordering::Relaxed);
    let mixed = MIXED.swap(0, Ordering::Relaxed);
    let audible = AUDIBLE.swap(0, Ordering::Relaxed);
    let peak = f32::from_bits(PEAK_BITS.swap(0, Ordering::Relaxed));
    let active = atl_audio_bridge::active_tracks();
    let from_tracks = atl_audio_bridge::take_track_frames();
    let muted = atl_audio_bridge::muted_tracks();
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    let track_underruns = atl_audio_bridge::take_underruns();
    let track_underrun_frames = atl_audio_bridge::take_underrun_frames();
    let _ = writeln!(
        std::io::stderr(),
        "audio-mix: requested={requested} mixed={mixed} from_tracks={from_tracks} audible={audible} peak={peak:.4} active={active} muted={muted} dropped={dropped} underruns={underruns} rt_fallbacks={rt_fallbacks} track_underruns={track_underruns} track_underrun_frames={track_underrun_frames}"
    );
}

impl Drop for Output {
    fn drop(&mut self) {
        tracing::info!(
            silent_cycles = SILENT_CYCLES.load(Ordering::Relaxed),
            "PCM output stopped"
        );
    }
}

/// Asks the output's thread for [`Opened`]. A request opens the output if it is not open yet; a failed open answers that request with its reason, and the next request tries again.
#[derive(Clone)]
pub(crate) struct Opener {
    requests: Sender<Ask>,
    volume: Arc<AppVolume>,
}

/// What the output's thread is asked.
enum Ask {
    /// The open output (opened now if needed).
    Open(Sender<Result<Opened, String>>),
    /// Write the app volume's pending value to the output, if it is open (else it waits for the open).
    Volume,
}

/// Where the output plays and as whom.
pub(crate) struct Target {
    /// The server's socket; None: none granted.
    pub(crate) remote: Option<PathBuf>,
    /// The app's id the stream carries ([`StreamIdentity`]); None: a process of no app.
    pub(crate) identity: Option<String>,
}

impl Opener {
    /// Starts the output's thread for `target`, whose stream's volume `volume` controls; the host's changes of it go to `reports` (sent from the PipeWire connection thread, which must not
    /// block: the receiver makes the AudioService calls). Opens nothing: the first [`Opener::get`] does.
    pub(crate) fn spawn(
        target: Target,
        opts: Options,
        volume: Arc<AppVolume>,
        reports: Sender<Report>,
    ) -> Result<Self, String> {
        let (requests, inbox) = mpsc::channel();
        let owned = Arc::clone(&volume);
        thread::Builder::new()
            .name("atl-pcm-output".to_owned())
            .spawn(move || owner(inbox, target, opts, owned, reports))
            .map_err(|e| format!("starting the PCM output's thread: {e}"))?;
        Ok(Self { requests, volume })
    }

    /// The open output, opened by this call if it is not open yet. Err: why it cannot be opened.
    pub(crate) fn get(&self) -> Result<Opened, String> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(Ask::Open(reply))
            .map_err(|_| "the PCM output's thread has ended".to_owned())?;
        answer
            .recv()
            .map_err(|_| "the PCM output's thread has ended".to_owned())?
    }

    /// Sets the app volume (music index, mute) and has the output's thread write it to the stream: now if open, else when the stream appears.
    pub(crate) fn set_volume(&self, index: u32, mute: bool) -> Result<(), String> {
        self.volume.set(index, mute);
        self.requests
            .send(Ask::Volume)
            .map_err(|_| "the PCM output's thread has ended".to_owned())
    }
}

/// Writes the app volume's pending value to `pcm`; a write the server refuses is counted (`refused AppVolume.apply`) and dropped.
fn apply_volume(pcm: &atl_platform_ui::PlatformPcmOutput, volume: &AppVolume) {
    if let Some(v) = volume.take_pending()
        && let Err(e) = pcm.set_volume(v)
    {
        atl_diag::refused("AppVolume", "apply");
        tracing::warn!(error = %e, "app volume not applied");
    }
}

/// The output's thread: opens the output on the first request, holds it until the process ends, and answers every request. It ends when no [`Opener`] is left.
fn owner(
    inbox: Receiver<Ask>,
    target: Target,
    opts: Options,
    volume: Arc<AppVolume>,
    reports: Sender<Report>,
) {
    let mut open: Option<(Output, Opened)> = None;
    let mut logged = Instant::now();
    let mut underruns_seen = 0;
    loop {
        // while open the thread writes the counters line once per period, however often requests come; closed, it only waits for requests
        let ask = if let Some((output, _)) = open.as_ref() {
            let wait = COUNTERS_PERIOD.saturating_sub(logged.elapsed());
            match inbox.recv_timeout(wait) {
                Ok(ask) => ask,
                Err(RecvTimeoutError::Timeout) => {
                    log_counters(&output.pcm, &mut underruns_seen);
                    logged = Instant::now();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match inbox.recv() {
                Ok(ask) => ask,
                Err(_) => break,
            }
        };
        let reply = match ask {
            Ask::Open(reply) => reply,
            Ask::Volume => {
                if let Some((output, _)) = open.as_ref() {
                    apply_volume(&output.pcm, &volume);
                }
                continue;
            }
        };
        let answer = match open.as_ref() {
            Some((_, opened)) => Ok(opened.clone()),
            None => match start(&target, opts, &volume, &reports) {
                Ok((output, opened)) => {
                    open = Some((output, opened.clone()));
                    Ok(opened)
                }
                Err(reason) => {
                    atl_diag::refused("PcmOutput", "open");
                    tracing::warn!(%reason, "PCM output unavailable");
                    Err(reason)
                }
            },
        };
        // the caller may have given up (its binder call ended): then nothing is waiting for the answer
        let _ = reply.send(answer);
    }
}

/// Connects to `target`'s server, opens and starts the output, and installs the mixer for the quantum the server ran. The stream's volume reports go to `volume`, its changes on to `reports`,
/// and its pending value is written once the stream runs.
fn start(
    target: &Target,
    opts: Options,
    volume: &Arc<AppVolume>,
    reports: &Sender<Report>,
) -> Result<(Output, Opened), String> {
    let remote = target.remote.as_deref().ok_or_else(|| {
        "no audio server socket granted: atld found no PipeWire socket on the desktop".to_owned()
    })?;
    let identity = target.identity.as_deref().map(|id| StreamIdentity { id });
    let host = PlatformAudioHost::connect(remote, identity).map_err(|e| e.to_string())?;
    let mixer: Shared = Arc::new(MixCell::default());
    let fill_mixer = Arc::clone(&mixer);
    let silent = opts.break_mix;
    let observer = Arc::clone(volume);
    let reports = reports.clone();
    let pcm = host
        .open_output(
            PcmSpec {
                rate_hz: REQUEST_RATE_HZ,
                channels: CHANNELS,
            },
            move |out| render(&fill_mixer, out, silent),
            // the connection thread only hands the change off; no receiver (no AudioService link) drops it
            move |v| {
                if let Some(report) = observer.observed(v) {
                    let _ = reports.send(report);
                }
            },
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
    apply_volume(&pcm, volume);
    Ok((Output { pcm }, Opened { profile, mixer }))
}

/// The realtime fill: one slice of interleaved stereo frames from the mixer, or silence (break, a control call holding the mixer, or no mixer yet).
fn render(mixer: &MixCell, out: &mut [f32], silent: bool) {
    let frames = (out.len() / usize::from(CHANNELS)) as u64;
    REQUESTED.fetch_add(frames, Ordering::Relaxed);
    if silent {
        out.fill(0.0);
        return;
    }
    if !mixer.fill(out) {
        SILENT_CYCLES.fetch_add(1, Ordering::Relaxed);
        out.fill(0.0);
        return;
    }
    MIXED.fetch_add(frames, Ordering::Relaxed);
    let mut audible = 0u64;
    let mut peak = 0f32;
    for frame in out.chunks_exact(usize::from(CHANNELS)) {
        let loudest = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
        audible += u64::from(loudest > 0.0);
        peak = peak.max(loudest);
    }
    AUDIBLE.fetch_add(audible, Ordering::Relaxed);
    PEAK_BITS.fetch_max(peak.to_bits(), Ordering::Relaxed);
}

/// The mixer, held by one control call. None before the output is open.
fn lock(mixer: &Shared) -> Option<MixGuard<'_>> {
    mixer.hold()
}

/// Adds a track's server side to the mix (disabled) in its format and `channel_mask`; its name, or None.
pub(crate) fn add_track(
    mixer: &Shared,
    track: Server,
    session: i32,
    channel_mask: u32,
) -> Option<i32> {
    lock(mixer)?.add(track, session, channel_mask)
}

/// Applies an IAudioTrack playback call to track `name` in the mix.
pub(crate) fn control(mixer: &Shared, name: i32, call: Control) {
    if let Some(mut mix) = lock(mixer)
        && !mix.control(name, call)
    {
        tracing::error!(name, ?call, "track unknown to the mixer");
    }
}

/// Removes track `name` from the mix; its server side is dropped with it. Its streaming frames never played count as dropped; the underruns its client saw over its life are logged per track.
pub(crate) fn remove_track(mixer: &Shared, name: i32) {
    if let Some(mut mix) = lock(mixer) {
        let removed = mix.remove(name);
        if removed.dropped > 0 {
            DROPPED.fetch_add(removed.dropped, Ordering::Relaxed);
            tracing::info!(
                name,
                dropped = removed.dropped,
                "track removed with frames it never played"
            );
        }
        if removed.underruns > 0 {
            tracing::info!(
                name,
                underruns = removed.underruns,
                "track removed after underruns"
            );
        }
    }
}
