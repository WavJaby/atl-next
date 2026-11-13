// SPDX-License-Identifier: MIT
//! IAudioFlingerService (AOSP AudioFlinger.cpp; AIDL in libaudioclient `aidl/`): the calls AudioSystem makes for an id, the output's rate, frame count and latency (the served profile, output.rs) and the primary output's (AudioManager's output properties), the client registration, and createTrack (A1b, A2).
//! Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). openOutput and the rest come with later slices.

use std::sync::Arc;

use android_media_audio_common_types::mangled::{
    _7_android_5_media_5_audio_6_common_7_PcmType as PcmType,
    _7_android_5_media_5_audio_6_common_11_AudioConfig as AudioConfig,
    _7_android_5_media_5_audio_6_common_15_AudioFormatType as AudioFormatType,
    _7_android_5_media_5_audio_6_common_15_AudioStreamType as AudioStreamType,
    _7_android_5_media_5_audio_6_common_18_AudioChannelLayout as AudioChannelLayout,
    _7_android_5_media_5_audio_6_common_22_AudioFormatDescription as AudioFormatDescription,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioFlingerClient::IAudioFlingerClient;
use atl_audioflinger_stubs::aidl::android::media::IAudioFlingerService::{
    BnAudioFlingerService, IAudioFlingerService,
};
use atl_audioflinger_stubs::mangled::{
    _7_android_5_media_18_CreateTrackRequest as CreateTrackRequest,
    _7_android_5_media_19_CreateTrackResponse as CreateTrackResponse,
};
use atl_services_types::counters;
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, Strong};

use crate::output::{self, Opened, Opener};
use crate::{OUTPUT_CHANNEL_MASK, Options, ids, track};

const INTERFACE: &str = "IAudioFlingerService";
/// `AUDIO_OUTPUT_FLAG_FAST` (system/media/audio/include/system/audio-hal-enums.h): the low-latency path. Refused: there is no fast mixer yet.
const AUDIO_OUTPUT_FLAG_FAST: i32 = 0x4;
/// `AUDIO_CHANNEL_OUT_MONO` (audio-hal-enums.h): front left only.
const CHANNEL_OUT_MONO: i32 = 0x1;
/// Bytes per sample of the only track format served, PCM 16-bit.
const BYTES_PER_SAMPLE: usize = 2;

/// The audio_io_handle_t of the one output: a unique id of use OUTPUT, made at first use and kept.
pub(crate) fn output_handle() -> i32 {
    static OUTPUT: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *OUTPUT.get_or_init(|| {
        ids::next(ids::USE_OUTPUT).expect("USE_OUTPUT is below AUDIO_UNIQUE_ID_USE_MAX")
    })
}

/// The output's format as AudioFlinger reports it (`afFormat`, AudioConfigBase): PCM 16-bit.
pub(crate) fn output_format() -> AudioFormatDescription {
    AudioFormatDescription {
        r#type: AudioFormatType::PCM,
        r#pcm: PcmType::INT_16_BIT,
        r#encoding: String::new(),
    }
}

/// The output's layout as AudioFlinger reports it (`afChannelMask`, AudioConfigBase): stereo.
pub(crate) fn output_layout() -> AudioChannelLayout {
    AudioChannelLayout::LayoutMask(OUTPUT_CHANNEL_MASK)
}

/// The channel count and mask of a PCM 16-bit mono or stereo config; None for any other format or layout (served later, or refused).
fn pcm16_layout(config: &AudioConfig) -> Option<(usize, u32)> {
    let base = &config.base;
    if base.format.r#type != AudioFormatType::PCM || base.format.r#pcm != PcmType::INT_16_BIT {
        return None;
    }
    match base.channelMask {
        AudioChannelLayout::LayoutMask(CHANNEL_OUT_MONO) => Some((1, CHANNEL_OUT_MONO as u32)),
        AudioChannelLayout::LayoutMask(OUTPUT_CHANNEL_MASK) => {
            Some((2, OUTPUT_CHANNEL_MASK as u32))
        }
        _ => None,
    }
}

struct AudioFlinger {
    opts: Options,
    output: Opener,
}

impl AudioFlinger {
    /// The open output when `handle` is the one output, opened by this call if needed. None for any other handle, or when the server cannot be reached (refused under `method`, counted).
    fn output_for(&self, handle: i32, method: &str) -> Option<Opened> {
        if handle != output_handle() {
            return None;
        }
        match self.output.get() {
            Ok(opened) => Some(opened),
            Err(_) => {
                counters::refused(INTERFACE, method);
                None
            }
        }
    }
}

impl Interface for AudioFlinger {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioFlingerService for AudioFlinger {
    /// AudioSystem::getSamplingRate: AOSP answers 0 for a handle it does not have (AudioFlinger.cpp:1283-1292), and 0 here when the output cannot open.
    fn r#sampleRate(&self, io_handle: i32) -> binder::Result<i32> {
        counters::answered(INTERFACE, "sampleRate");
        Ok(self
            .output_for(io_handle, "sampleRate")
            .map_or(0, |o| o.profile.rate_hz as i32))
    }

    /// AudioSystem::getFrameCount: 0 for an unknown handle, as AOSP (AudioFlinger.cpp:1305-1315).
    fn r#frameCount(&self, io_handle: i32) -> binder::Result<i64> {
        counters::answered(INTERFACE, "frameCount");
        Ok(self
            .output_for(io_handle, "frameCount")
            .map_or(0, |o| i64::from(o.profile.quantum)))
    }

    /// AudioSystem::getLatency, ms (AudioFlinger.cpp:1329-1337: 0 for no playback thread).
    fn r#latency(&self, output: i32) -> binder::Result<i32> {
        counters::answered(INTERFACE, "latency");
        Ok(self
            .output_for(output, "latency")
            .map_or(0, |o| o.profile.latency_ms()))
    }

    /// AudioManager's `PROPERTY_OUTPUT_SAMPLE_RATE` (AudioSystem.getPrimaryOutputSamplingRate): the primary playback thread's rate, 0 with none (AudioFlinger.cpp getPrimaryOutputSamplingRate). The one output is the primary; it opens here on first use.
    fn r#getPrimaryOutputSamplingRate(&self) -> binder::Result<i32> {
        counters::answered(INTERFACE, "getPrimaryOutputSamplingRate");
        Ok(self
            .output_for(output_handle(), "getPrimaryOutputSamplingRate")
            .map_or(0, |o| o.profile.rate_hz as i32))
    }

    /// AudioManager's `PROPERTY_OUTPUT_FRAMES_PER_BUFFER`: the primary thread's HAL frame count (frameCountHAL), 0 with none; here the output's quantum.
    fn r#getPrimaryOutputFrameCount(&self) -> binder::Result<i64> {
        counters::answered(INTERFACE, "getPrimaryOutputFrameCount");
        Ok(self
            .output_for(output_handle(), "getPrimaryOutputFrameCount")
            .map_or(0, |o| i64::from(o.profile.quantum)))
    }

    /// AOSP keeps the client for ioConfigChanged; this service never changes an output, so it never calls back.
    fn r#registerClient(
        &self,
        _client: Option<&Strong<dyn IAudioFlingerClient>>,
    ) -> binder::Result<()> {
        counters::answered(INTERFACE, "registerClient");
        Ok(())
    }

    /// AudioFlinger::newAudioUniqueId: the next id of the use; AUDIO_UNIQUE_ID_ALLOCATE (0) for a use AOSP refuses.
    fn r#newAudioUniqueId(
        &self,
        use_: audioclient_types_aidl::mangled::_7_android_5_media_16_AudioUniqueIdUse,
    ) -> binder::Result<i32> {
        match u32::try_from(use_.0).ok().and_then(ids::next) {
            Some(id) => {
                counters::answered(INTERFACE, "newAudioUniqueId");
                Ok(id)
            }
            None => {
                counters::refused(INTERFACE, "newAudioUniqueId");
                Ok(0)
            }
        }
    }

    /// AudioFlinger keeps per-client session refs for effects; no effect exists here, so the call is a no-op.
    fn r#acquireAudioSessionId(&self, _session: i32, _pid: i32, _uid: i32) -> binder::Result<()> {
        counters::answered(INTERFACE, "acquireAudioSessionId");
        Ok(())
    }

    fn r#releaseAudioSessionId(&self, _session: i32, _pid: i32) -> binder::Result<()> {
        counters::answered(INTERFACE, "releaseAudioSessionId");
        Ok(())
    }

    /// AudioFlinger::createTrack (Tracks.cpp): a PCM 16-bit mono or stereo track with its region, its server side in the output's mixer, and an IAudioTrack. A streaming track gets its own buffer after the control block; a shared-buffer (static) track takes the client's (SoundPool's streams).
    /// FAST is refused and cleared from the response flags, so the client keeps the track on the normal path (AudioTrack.cpp createTrack_l takes the flags the server returns).
    fn r#createTrack(
        &self,
        request: Option<&CreateTrackRequest>,
    ) -> binder::Result<Option<CreateTrackResponse>> {
        let refused = || -> binder::Result<Option<CreateTrackResponse>> {
            counters::refused(INTERFACE, "createTrack");
            Err(StatusCode::BAD_VALUE.into())
        };
        let Some(request) = request else {
            return refused();
        };
        let Some(config) = request.config.as_ref() else {
            return refused();
        };
        let Some((channels, channel_mask)) = pcm16_layout(config) else {
            tracing::warn!("createTrack: format or layout not served");
            return refused();
        };
        let Ok(track_rate) = u32::try_from(config.base.sampleRate) else {
            return refused();
        };
        if track_rate == 0 {
            return refused();
        }
        let session = match request.sessionId {
            0 => ids::next(ids::USE_SESSION).expect("USE_SESSION is below AUDIO_UNIQUE_ID_USE_MAX"),
            s if s > 0 => s,
            _ => return refused(),
        };
        // no playback thread for the output is BAD_VALUE (AudioFlinger.cpp:1155-1158); the output opens here on its first use
        let out = match self.output.get() {
            Ok(out) => out,
            Err(reason) => {
                tracing::warn!(%reason, "createTrack: no PCM output");
                counters::refused(INTERFACE, "createTrack.output");
                return refused();
            }
        };
        let frame_bytes = channels * BYTES_PER_SAMPLE;
        let (frames, region, server) = match request.sharedBuffer.as_ref() {
            // static: the client's buffer is the track's buffer, and the frames are the buffer's (Threads.cpp:2569: a PCM shared buffer ignores the request's frame count). No roundup (TrackBase:162).
            Some(shared) => {
                let frames = i64::try_from(frame_bytes)
                    .ok()
                    .and_then(|bytes| usize::try_from(shared.size / bytes).ok())
                    .unwrap_or(0);
                if frames == 0 {
                    return refused();
                }
                let region = match track::allocate(0, self.opts.break_cblk) {
                    Ok(region) => region,
                    Err(status) => {
                        counters::refused(INTERFACE, "createTrack.region");
                        return Err(status.into());
                    }
                };
                match track::server_shared(
                    &region,
                    shared,
                    channels,
                    frames,
                    frame_bytes,
                    track_rate,
                ) {
                    Ok(server) => (frames, region, server),
                    Err(status) => {
                        counters::refused(INTERFACE, "createTrack.region");
                        return Err(status.into());
                    }
                }
            }
            None => {
                let Some(frames) = track::round_frames(request.frameCount) else {
                    return refused();
                };
                let Some(bytes) = frames.checked_mul(frame_bytes) else {
                    return refused();
                };
                let region = match track::allocate(bytes, self.opts.break_cblk) {
                    Ok(region) => region,
                    Err(status) => {
                        counters::refused(INTERFACE, "createTrack.region");
                        return Err(status.into());
                    }
                };
                match track::server_streaming(&region, frames, frame_bytes, track_rate) {
                    Ok(server) => (frames, region, server),
                    Err(status) => {
                        counters::refused(INTERFACE, "createTrack.region");
                        return Err(status.into());
                    }
                }
            }
        };
        if request.flags & AUDIO_OUTPUT_FLAG_FAST != 0 {
            counters::refused(INTERFACE, "createTrack.FAST");
        }
        let Some(name) = output::add_track(&out.mixer, server, session, channel_mask) else {
            counters::refused(INTERFACE, "createTrack.mixer");
            return refused();
        };
        counters::answered(INTERFACE, "createTrack");
        Ok(Some(CreateTrackResponse {
            r#flags: request.flags & !AUDIO_OUTPUT_FLAG_FAST,
            r#frameCount: frames as i64,
            r#notificationFrameCount: request.notificationFrameCount,
            r#selectedDeviceIds: Some(Vec::new()),
            r#sessionId: session,
            r#sampleRate: config.base.sampleRate,
            r#streamType: AudioStreamType::MUSIC,
            r#afFrameCount: i64::from(out.profile.quantum),
            r#afSampleRate: out.profile.rate_hz as i32,
            r#afChannelMask: Some(output_layout()),
            r#afFormat: Some(output_format()),
            r#afLatencyMs: out.profile.latency_ms(),
            r#afTrackFlags: 0,
            r#outputId: output_handle(),
            r#portId: ids::next(ids::USE_CLIENT)
                .expect("USE_CLIENT is below AUDIO_UNIQUE_ID_USE_MAX"),
            r#audioTrack: Some(track::new_binder(region, Arc::clone(&out.mixer), name)),
        }))
    }
}

/// The binder served as [`crate::FLINGER_NAME`].
pub(crate) fn new_binder(opts: Options, output: Opener) -> SpIBinder {
    BnAudioFlingerService::new_binder(AudioFlinger { opts, output }, BinderFeatures::default())
        .as_binder()
}
