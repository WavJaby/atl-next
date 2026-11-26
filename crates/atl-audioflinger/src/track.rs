// SPDX-License-Identifier: MIT
//! A track's shared memory and its IAudioTrack (AOSP AudioFlinger's TrackBase and TrackHandle, Tracks.cpp / AudioFlinger.cpp).
//! The region is a memfd: the control block (`audio_track_cblk_t`) followed by the streaming buffer, the layout AOSP's TrackBase allocates (Tracks.cpp, ALLOC_CBLK). The client maps it through getCblk and writes into it; the server maps the same region (atl-audio-bridge), and the output's mixer reads it. A static track's buffer is the client's own (its sharedBuffer fd), mapped by createTrack.
//! start/pause/stop/flush drive the track in the mixer (output.rs, atl-audio-bridge `Control`) as AOSP's Track does for a normal mixed track: stop lets a streaming track play out what the client wrote before it, flush discards unread frames.

use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use atl_audio_bridge::{Control, Mapping, Track as Server};
use atl_audioflinger_stubs::aidl::android::media::IAudioTrack::{BnAudioTrack, IAudioTrack};
use av_types_aidl::mangled::{
    _7_android_5_media_17_VolumeShaperState as VolumeShaperState,
    _7_android_5_media_21_VolumeShaperOperation as VolumeShaperOperation,
    _7_android_5_media_25_VolumeShaperConfiguration as VolumeShaperConfiguration,
};
use binder::binder_impl::{Parcel, Serialize};
use binder::{BinderFeatures, Interface, StatusCode, Strong};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use shared_file_region_aidl::mangled::_7_android_5_media_16_SharedFileRegion as SharedFileRegion;

use crate::output::{self, Shared};

const INTERFACE: &str = "IAudioTrack";
/// status_t NO_ERROR.
const NO_ERROR: i32 = 0;
/// status_t BAD_VALUE (-EINVAL).
const BAD_VALUE: i32 = -22;
/// A linear PCM track format the mixer takes (AudioMixerBase::isValidFormat); the value is its audio_format_t (audio-hal-enums.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum PcmFormat {
    I16 = 0x1,
    U8 = 0x2,
    I32 = 0x3,
    Float = 0x5,
    I24Packed = 0x6,
}

impl PcmFormat {
    /// audio_bytes_per_sample (audio.h).
    pub(crate) fn sample_bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 => 2,
            Self::I24Packed => 3,
            Self::I32 | Self::Float => 4,
        }
    }

    /// The audio_format_t the server side and mixer are created with.
    pub(crate) fn audio_format(self) -> u32 {
        self as u32
    }
}
/// Largest track in frames: 2^22 frames of 4 bytes is a 16 MiB region. AOSP only checks size overflow (Tracks.cpp:167, :183); this bound keeps a bad request from allocating the address space.
const MAX_FRAMES: usize = 1 << 22;

/// AOSP's streaming track: `roundup(frameCount)`, the next power of two (Tracks.cpp:162). `None` for a request AOSP would overflow on, or above [`MAX_FRAMES`].
pub(crate) fn round_frames(requested: i64) -> Option<usize> {
    let requested = usize::try_from(requested.max(1)).ok()?;
    let frames = requested.checked_next_power_of_two()?;
    (frames <= MAX_FRAMES).then_some(frames)
}

/// What PlaybackThread::createTrack_l sizes a streaming PCM track by: the output's quantum (mNormalFrameCount), rate and latency, and the track's rate and speed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sizing {
    pub(crate) quantum: u32,
    pub(crate) out_rate: u32,
    pub(crate) latency_ms: u32,
    pub(crate) track_rate: u32,
    pub(crate) speed: f32,
}

/// `FMS_20` (Threads.cpp): a denied FAST track's notification period, in ms.
const FAST_DENIED_NOTIFY_MS: u64 = 20;

/// AudioResamplerPublic.h sourceFramesNeededWithTimestretch: track frames the resampler and time stretcher need for `dst` output frames.
fn source_frames(s: &Sizing, dst: u64) -> u64 {
    let (src, out) = (u64::from(s.track_rate), u64::from(s.out_rate));
    let required = if src == out {
        dst
    } else {
        dst * src / out + 1 + 1
    };
    (required as f64 * f64::from(s.speed)) as u64 + 1 + 1
}

/// A normal streaming track's frame count (Threads.cpp createTrack_l, non-FAST branch): the request, raised to AudioSystem::calculateMinFrameCount (at least two quanta, enough to cover the output's latency). 0 asks for the minimum.
pub(crate) fn streaming_frames(requested: i64, s: &Sizing) -> u64 {
    let quantum_ms = (1000 * u64::from(s.quantum) / u64::from(s.out_rate)).max(1);
    let buffers = (u64::from(s.latency_ms) / quantum_ms).max(2);
    let min = buffers * source_frames(s, u64::from(s.quantum));
    u64::try_from(requested).unwrap_or(0).max(min)
}

/// The notification period AudioFlinger grants a normal streaming PCM track of `frames` (Threads.cpp createTrack_l, "Make sure that application is notified"): 0 or more than `frames / nBuffering` becomes `frames / nBuffering` (nBuffering 3 for a triple-buffered mixer period, else 2), capped at 20 ms when the client asked FAST and was denied.
/// The client's callback thread fills one notification period per wake (AudioTrack processAudioBuffer); a period of 0 makes it spin without ever asking for data.
pub(crate) fn notification_frames(
    requested: i64,
    frames: u64,
    s: &Sizing,
    fast_denied: bool,
) -> u64 {
    let ratio = frames * u64::from(s.out_rate) / (u64::from(s.quantum) * u64::from(s.track_rate));
    let buffering = if ratio == 3 { 3 } else { 2 };
    let mut max = frames / buffering;
    if fast_denied {
        max = max.min(FAST_DENIED_NOTIFY_MS * u64::from(s.track_rate) / 1000);
    }
    match u64::try_from(requested) {
        Ok(n) if n != 0 && n <= max => n,
        _ => max,
    }
}

/// The track's region: a memfd of `len` bytes, the control block and the track's own buffer (none for a shared-buffer track).
pub(crate) struct Region {
    pub(crate) fd: OwnedFd,
    pub(crate) len: usize,
}

/// The region for a track whose buffer is `buffer_bytes`: 0 for a shared-buffer (static) track, whose buffer is the client's (Tracks.cpp:893). `break_cblk` allocates the control block alone, one buffer short of a streaming track (the audio gate's break); the size check then refuses it: a region shorter than the layout is never handed out.
pub(crate) fn allocate(buffer_bytes: usize, break_cblk: bool) -> Result<Region, StatusCode> {
    let cblk = atl_audio_bridge::cblk_bytes();
    let need = buffer_bytes
        .checked_add(cblk)
        .ok_or(StatusCode::BAD_VALUE)?;
    let len = if break_cblk { cblk } else { need };
    if len < need {
        tracing::error!(len, need, "track region smaller than its layout; refused");
        return Err(StatusCode::NO_MEMORY);
    }
    let fd = memfd_create("atl-audio-track", MemfdFlags::CLOEXEC).map_err(|e| {
        tracing::error!(%e, "track region memfd");
        StatusCode::NO_MEMORY
    })?;
    ftruncate(&fd, len as u64).map_err(|e| {
        tracing::error!(%e, len, "track region size");
        StatusCode::NO_MEMORY
    })?;
    Ok(Region { fd, len })
}

/// The server side of a streaming track: its region mapped read-write, the frames after the control block.
pub(crate) fn server_streaming(
    region: &Region,
    format: PcmFormat,
    frames: usize,
    frame_bytes: usize,
    rate: u32,
) -> Result<Server, StatusCode> {
    let cblk = Mapping::map(&region.fd, 0, region.len, true).map_err(|e| {
        tracing::error!(%e, "track region mapping");
        StatusCode::NO_MEMORY
    })?;
    Server::streaming(cblk, format.audio_format(), frames, frame_bytes, rate).map_err(|e| {
        tracing::error!(%e, "streaming track layout");
        StatusCode::NO_MEMORY
    })
}

/// The server side of a static track: the client's shared buffer, mapped read-only, and the control block of `region`. The buffer's address must meet AudioFlinger's alignment (Threads.cpp:2547-2558): the sample size (1 for packed 24-bit), doubled for more than one channel.
pub(crate) fn server_shared(
    region: &Region,
    shared: &SharedFileRegion,
    format: PcmFormat,
    channels: usize,
    frames: usize,
    frame_bytes: usize,
    rate: u32,
) -> Result<Server, StatusCode> {
    let fd = shared.r#fd.as_ref().ok_or(StatusCode::BAD_VALUE)?;
    let offset = u64::try_from(shared.r#offset).map_err(|_| StatusCode::BAD_VALUE)?;
    let size = usize::try_from(shared.r#size).map_err(|_| StatusCode::BAD_VALUE)?;
    let buffer = Mapping::map(fd.as_ref(), offset, size, false).map_err(|e| {
        tracing::error!(%e, "static buffer mapping");
        StatusCode::NO_MEMORY
    })?;
    let sample = match format.sample_bytes() {
        odd if odd & 1 == 1 => 1,
        even => even,
    };
    let alignment = if channels > 1 { sample << 1 } else { sample };
    if buffer.address() % alignment != 0 {
        tracing::error!(
            address = buffer.address(),
            channels,
            "invalid shared buffer alignment; refused"
        );
        return Err(StatusCode::BAD_VALUE);
    }
    let cblk = Mapping::map(&region.fd, 0, region.len, true).map_err(|e| {
        tracing::error!(%e, "track region mapping");
        StatusCode::NO_MEMORY
    })?;
    Server::shared(
        cblk,
        buffer,
        format.audio_format(),
        frames,
        frame_bytes,
        rate,
    )
    .map_err(|e| {
        tracing::error!(%e, "static track layout");
        StatusCode::NO_MEMORY
    })
}

/// A parcelable's bytes as the bridge reads them: the marker and sized body that Parcel::write gives (policy.rs attributes_bytes).
fn parcel_bytes<T: Serialize>(value: &T) -> binder::Result<Vec<u8>> {
    let mut parcel = Parcel::new();
    parcel.write(value)?;
    Ok(parcel.marshal()?)
}

/// AudioTrack's playback states as AOSP's Track keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Active,
    Paused,
    Stopped,
}

/// One track as its IAudioTrack: the region the client maps, its name in the output's mixer (which owns its server side), and the state its start/stop/pause/flush drive.
struct Track {
    region: Region,
    mixer: Shared,
    name: i32,
    state: Mutex<State>,
}

impl Track {
    fn state(&self) -> MutexGuard<'_, State> {
        // a poisoned lock still holds a plain enum
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        // the mixer lets go of the track's server side before the region goes
        output::remove_track(&self.mixer, self.name);
    }
}

impl Interface for Track {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioTrack for Track {
    /// IAudioTrack.getCblk: a new fd of the region, as AOSP's TrackHandle returns a dup per call; the client maps it read-write.
    fn r#getCblk(&self) -> binder::Result<Option<SharedFileRegion>> {
        atl_diag::answered(INTERFACE, "getCblk");
        let fd = self
            .region
            .fd
            .try_clone()
            .map_err(|_| StatusCode::NO_MEMORY)?;
        Ok(Some(SharedFileRegion {
            r#fd: Some(binder::ParcelFileDescriptor::new(fd)),
            r#offset: 0,
            r#size: self.region.len as i64,
            r#writeable: true,
        }))
    }

    /// Idle, stopped (draining or not) or paused -> active: enabled in the mixer. Starting an active track is a no-op (the client's own guard decides before it calls).
    fn r#start(&self) -> binder::Result<i32> {
        atl_diag::answered(INTERFACE, "start");
        let mut state = self.state();
        if *state != State::Active {
            *state = State::Active;
            output::control(&self.mixer, self.name, Control::Start);
        }
        Ok(NO_ERROR)
    }

    /// Active or paused -> stopped (Tracks.cpp Track::stop). An active streaming track keeps playing what the client wrote before its stop (the client proxy's stop position bounds it), then leaves the mix.
    fn r#stop(&self) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "stop");
        let mut state = self.state();
        if matches!(*state, State::Active | State::Paused) {
            *state = State::Stopped;
            output::control(&self.mixer, self.name, Control::Stop);
        }
        Ok(())
    }

    /// Active -> paused: out of the mix, unread frames kept. A stopped track (draining or not) stays stopped (Track::pause acts on ACTIVE only for a non-offloaded track).
    fn r#pause(&self) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "pause");
        let mut state = self.state();
        if *state == State::Active {
            *state = State::Paused;
            output::control(&self.mixer, self.name, Control::Pause);
        }
        Ok(())
    }

    /// Discards the unread frames of a stopped, paused or idle track and leaves it idle (Track::flush: FLUSHED, then reset to IDLE); no effect on an active one. The frames count as dropped when the track is removed.
    fn r#flush(&self) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "flush");
        let mut state = self.state();
        if *state != State::Active {
            *state = State::Idle;
            output::control(&self.mixer, self.name, Control::Flush);
        }
        Ok(())
    }

    /// AudioTrack's construction attaches effect 0 (none): NO_ERROR. A real effect would need an effect chain this server does not have.
    fn r#attachAuxEffect(&self, effect_id: i32) -> binder::Result<i32> {
        if effect_id == 0 {
            atl_diag::answered(INTERFACE, "attachAuxEffect");
            Ok(NO_ERROR)
        } else {
            atl_diag::refused(INTERFACE, "attachAuxEffect");
            Ok(BAD_VALUE)
        }
    }

    /// IAudioTrack.getVolumeShaperState (TrackHandle::getVolumeShaperState, Tracks.cpp:623-633): shaper `id`'s state, or null when the track has no such shaper (Tracks.cpp:627-630).
    fn r#getVolumeShaperState(&self, id: i32) -> binder::Result<Option<VolumeShaperState>> {
        atl_diag::answered(INTERFACE, "getVolumeShaperState");
        Ok(
            output::volume_shaper_state(&self.mixer, self.name, id).map(|state| {
                VolumeShaperState {
                    r#volume: state.volume,
                    r#xOffset: state.x_offset,
                }
            }),
        )
    }

    /// IAudioTrack.applyVolumeShaper (TrackHandle::applyVolumeShaper, Tracks.cpp:603-621): the configuration and operation become the track's VolumeShapers in the mix, applied from the next quantum. A null argument is BAD_VALUE (AIDL: not nullable). A negative status counts as a refusal: a shaper the mix does not apply would fail silently.
    fn r#applyVolumeShaper(
        &self,
        configuration: Option<&VolumeShaperConfiguration>,
        operation: Option<&VolumeShaperOperation>,
    ) -> binder::Result<i32> {
        let status = match (configuration, operation) {
            (Some(configuration), Some(operation)) => output::apply_volume_shaper(
                &self.mixer,
                self.name,
                &parcel_bytes(configuration)?,
                &parcel_bytes(operation)?,
            ),
            _ => BAD_VALUE,
        };
        if status < 0 {
            atl_diag::refused(INTERFACE, "applyVolumeShaper");
        } else {
            atl_diag::answered(INTERFACE, "applyVolumeShaper");
        }
        Ok(status)
    }
}

/// The IAudioTrack for a new track over `region`, idle, named `name` in `mixer`.
pub(crate) fn new_binder(region: Region, mixer: Shared, name: i32) -> Strong<dyn IAudioTrack> {
    BnAudioTrack::new_binder(
        Track {
            region,
            mixer,
            name,
            state: Mutex::new(State::Idle),
        },
        BinderFeatures::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The served output: PipeWire's 1024-frame quantum at 48 kHz, latency two quanta = 42 ms (output.rs `latency_ms`).
    fn out(track_rate: u32) -> Sizing {
        Sizing {
            quantum: 1024,
            out_rate: 48_000,
            latency_ms: 42,
            track_rate,
            speed: 1.0,
        }
    }

    // Expected values by hand from the AOSP formulas: quantum ms = 1024000 / 48000 = 21; buffers = max(42 / 21, 2) = 2.
    // Same rate: sourceFramesNeeded = 1024, timestretch 1024 * 1.0 + 2 = 1026; min = 2 * 1026 = 2052.
    #[test]
    fn zero_request_gets_the_minimum() {
        assert_eq!(streaming_frames(0, &out(48_000)), 2052);
        assert_eq!(streaming_frames(8192, &out(48_000)), 8192);
    }

    // 44.1 kHz: 1024 * 44100 / 48000 = 940 (truncated) + 2 = 942, + 2 = 944; min = 1888.
    #[test]
    fn resampled_minimum() {
        assert_eq!(streaming_frames(0, &out(44_100)), 1888);
    }

    // 24 kHz (FMOD's AAudio stream in Angry Birds 2): 1024 * 24000 / 48000 = 512 + 2 = 514, + 2 = 516; min = 1032.
    // Notification: ratio 1032 * 48000 / (1024 * 24000) = 2 (truncated) -> halves, 516.
    #[test]
    fn half_rate_track() {
        let s = out(24_000);
        assert_eq!(streaming_frames(0, &s), 1032);
        assert_eq!(notification_frames(0, 1032, &s, false), 516);
    }

    // 2052 frames: ratio 2052 * 48000 / (1024 * 48000) = 2 -> double buffering, max 1026; FAST denied caps at 20 ms = 960.
    #[test]
    fn zero_notification_becomes_the_period() {
        let s = out(48_000);
        assert_eq!(notification_frames(0, 2052, &s, false), 1026);
        assert_eq!(notification_frames(0, 2052, &s, true), 960);
        assert_eq!(notification_frames(500, 2052, &s, true), 500);
        assert_eq!(notification_frames(5000, 2052, &s, false), 1026);
    }

    // 3072 frames = three quanta at the same rate -> triple buffering: 3072 / 3 = 1024.
    #[test]
    fn triple_buffered_period() {
        assert_eq!(notification_frames(0, 3072, &out(48_000), false), 1024);
    }

    use std::io::Write;
    use std::sync::Arc;

    use atl_audio_bridge::{Mix, MixCell};
    use av_types_aidl::mangled::_7_android_5_media_29_VolumeShaperConfigurationType as VolumeShaperConfigurationType;

    /// The output's quantum and rate (output.rs REQUEST_RATE_HZ, PipeWire's 1024 frames).
    const QUANTUM: usize = 1024;
    const RATE: u32 = 48_000;
    /// AUDIO_CHANNEL_OUT_STEREO (system/media/audio/include/system/audio.h).
    const STEREO: u32 = 0x3;

    /// A static track of `frames` stereo float frames of `value`, added to `mixer` (installed at QUANTUM). Its client volume is the control block's default, unity (AudioTrackShared.cpp:66-68).
    fn static_track(mixer: &Shared, frames: usize, value: f32) -> Track {
        let cblk = allocate(0, false).expect("control block region");
        let fd = memfd_create("atl-audio-test-clip", MemfdFlags::CLOEXEC).expect("clip memfd");
        let clip: Vec<u8> = (0..frames * 2).flat_map(|_| value.to_le_bytes()).collect();
        ftruncate(&fd, clip.len() as u64).expect("clip size");
        std::fs::File::from(fd.try_clone().expect("clip dup"))
            .write_all(&clip)
            .expect("clip write");
        let buffer = SharedFileRegion {
            r#fd: Some(binder::ParcelFileDescriptor::new(fd)),
            r#offset: 0,
            r#size: clip.len() as i64,
            r#writeable: false,
        };
        let server = server_shared(&cblk, &buffer, PcmFormat::Float, 2, frames, 8, RATE)
            .expect("static track");
        let name = output::add_track(mixer, server, 0, STEREO).expect("track in the mix");
        Track {
            region: cblk,
            mixer: Arc::clone(mixer),
            name,
            state: Mutex::new(State::Idle),
        }
    }

    /// One quantum of the output, interleaved stereo, pulled as the realtime fill does.
    fn quantum(mixer: &Shared) -> Vec<f32> {
        let mut out = vec![0f32; QUANTUM * 2];
        assert!(mixer.fill(&mut out), "mixer in the cell");
        out
    }

    /// A TYPE_SCALE shaper holding `volume` over its whole curve: both points are `volume` (VolumeShaper.java:508-511 writes xy as time, volume pairs).
    fn flat_shaper(volume: f32) -> (VolumeShaperConfiguration, VolumeShaperOperation) {
        let mut configuration = VolumeShaperConfiguration {
            r#type: VolumeShaperConfigurationType::SCALE,
            r#id: 1,
            r#durationMs: 1000.0,
            ..Default::default()
        };
        configuration
            .r#interpolatorConfig
            .insert(Default::default())
            .r#xy = vec![0.0, volume, 1.0, volume];
        let operation = VolumeShaperOperation {
            r#replaceId: -1,
            ..Default::default()
        };
        (configuration, operation)
    }

    /// Refusals AOSP's status counts for `api` (atl_diag, process-wide).
    fn refusals(api: &str) -> u64 {
        atl_diag::counts()
            .iter()
            .filter(|(outcome, name, _)| outcome == "refused" && name == api)
            .map(|(_, _, n)| *n)
            .sum()
    }

    // IAudioTrack.applyVolumeShaper must change the PCM the mixer produces. Expected by construction: the clip is 0.5; the client volume is unity (AudioTrackShared.cpp:66-68), so 0.5; the flat shaper holds 0.5 (Threads.cpp:5860-5862 multiplies the track volume by it), so 0.5 x 0.5 = 0.25.
    // Quantum 0 starts the track and quantum 1 is steady at unity; the change ramps in over quantum 2 and quantum 3 is steady at the shaper's gain.
    #[test]
    fn volume_shaper_changes_the_pcm() {
        let mixer: Shared = Arc::new(MixCell::default());
        assert!(mixer.install(Mix::new(QUANTUM, RATE)), "mixer installed");
        let track = static_track(&mixer, 4 * QUANTUM, 0.5);
        track.r#start().expect("start");
        let _ = quantum(&mixer);
        let unity = quantum(&mixer);
        assert!(unity.iter().all(|&s| s == 0.5), "unity: {:?}", &unity[..4]);
        let (configuration, operation) = flat_shaper(0.5);
        let status = track
            .r#applyVolumeShaper(Some(&configuration), Some(&operation))
            .expect("applyVolumeShaper answers");
        assert!(status >= 0, "a shaper id, got {status}");
        let _ = quantum(&mixer);
        let shaped = quantum(&mixer);
        assert!(
            shaped.iter().all(|&s| (s - 0.25).abs() < 1e-6),
            "shaped: {:?}",
            &shaped[..4]
        );
    }

    // A null argument is BAD_VALUE (AIDL: not nullable, Tracks.cpp:603-621 reads both parcelables first) and counts as a refusal.
    #[test]
    fn null_shaper_argument_is_refused() {
        let mixer: Shared = Arc::new(MixCell::default());
        assert!(mixer.install(Mix::new(QUANTUM, RATE)), "mixer installed");
        let track = static_track(&mixer, QUANTUM, 0.5);
        let (_, operation) = flat_shaper(0.5);
        let before = refusals("IAudioTrack.applyVolumeShaper");
        assert_eq!(
            track.r#applyVolumeShaper(None, Some(&operation)),
            Ok(BAD_VALUE)
        );
        assert_eq!(refusals("IAudioTrack.applyVolumeShaper"), before + 1);
    }

    /// Shaper 1's state on `track` is the flat `volume` at offset 0.
    fn assert_flat_state(track: &Track, volume: f32) {
        let state = track
            .r#getVolumeShaperState(1)
            .expect("getVolumeShaperState answers")
            .expect("shaper 1 has a state");
        assert!(
            (state.r#volume - volume).abs() < 1e-6,
            "volume: {}",
            state.r#volume
        );
        assert_eq!(state.r#xOffset, 0.0, "xOffset");
    }

    // IAudioTrack.getVolumeShaperState (TrackHandle::getVolumeShaperState, Tracks.cpp:623-633). An id the track does not have is null (Tracks.cpp:627-630; VolumeHandler::getVolumeShaperState returns nullptr, VolumeShaper.h:1064-1069).
    // A shaper that has not started reports its delay: the curve at its delay offset, and the offset (VolumeShaper.h:771-776). The offset starts at MIN_CURVE_TIME = 0 (VolumeShaper.h:64, :754), and the operation's xOffset 0 sets it (VolumeShaper.h:1042-1056, :786-788). The flat curve is 0.5 everywhere (VolumeShaper.java:508-511).
    // Started at frame 0, the state is the same point: x = 0 at the start (VolumeShaper.h:816-823).
    #[test]
    fn volume_shaper_state_is_served() {
        let mixer: Shared = Arc::new(MixCell::default());
        assert!(mixer.install(Mix::new(QUANTUM, RATE)), "mixer installed");
        let track = static_track(&mixer, 4 * QUANTUM, 0.5);
        let (configuration, operation) = flat_shaper(0.5);
        assert!(
            matches!(track.r#getVolumeShaperState(1), Ok(None)),
            "no shaper 1 before applyVolumeShaper"
        );
        track
            .r#applyVolumeShaper(Some(&configuration), Some(&operation))
            .expect("applyVolumeShaper answers");
        assert_flat_state(&track, 0.5);
        track.r#start().expect("start");
        assert_flat_state(&track, 0.5);
    }
}
