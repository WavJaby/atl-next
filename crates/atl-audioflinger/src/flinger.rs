// SPDX-License-Identifier: MIT
//! IAudioFlingerService (AOSP AudioFlinger.cpp; AIDL in libaudioclient `aidl/`): the calls AudioSystem makes for an id, the output's rate, frame count and latency, the client registration, and createTrack (A1b).
//! Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). openOutput and the rest come with A2.

use std::sync::OnceLock;

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
use atl_services::counters;
use binder::{BinderFeatures, Interface, SpIBinder, Status, StatusCode, Strong};

use crate::{
    OUTPUT_CHANNEL_MASK, OUTPUT_FRAMES, OUTPUT_LATENCY_MS, OUTPUT_RATE_HZ, Options, ids, track,
};

const INTERFACE: &str = "IAudioFlingerService";
/// `AUDIO_OUTPUT_FLAG_FAST` (system/media/audio/include/system/audio-hal-enums.h): the low-latency path. Refused: there is no fast mixer yet.
const AUDIO_OUTPUT_FLAG_FAST: i32 = 0x4;
/// `AUDIO_CHANNEL_OUT_MONO` (audio-hal-enums.h): front left only.
const CHANNEL_OUT_MONO: i32 = 0x1;
/// Bytes per sample of the only track format served, PCM 16-bit.
const BYTES_PER_SAMPLE: usize = 2;

/// The audio_io_handle_t of the one output: a unique id of use OUTPUT, made at first use and kept.
pub(crate) fn output_handle() -> i32 {
    static OUTPUT: OnceLock<i32> = OnceLock::new();
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

/// The channel count of a PCM 16-bit mono or stereo config; None for any other format or layout (served later, or refused).
fn pcm16_channels(config: &AudioConfig) -> Option<usize> {
    let base = &config.base;
    if base.format.r#type != AudioFormatType::PCM || base.format.r#pcm != PcmType::INT_16_BIT {
        return None;
    }
    match base.channelMask {
        AudioChannelLayout::LayoutMask(CHANNEL_OUT_MONO) => Some(1),
        AudioChannelLayout::LayoutMask(OUTPUT_CHANNEL_MASK) => Some(2),
        _ => None,
    }
}

struct AudioFlinger {
    opts: Options,
}

impl Interface for AudioFlinger {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioFlingerService for AudioFlinger {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// AudioSystem::getSamplingRate: AOSP answers 0 for a handle it does not have.
    fn r#sampleRate(&self, io_handle: i32) -> binder::Result<i32> {
        counters::answered(INTERFACE, "sampleRate");
        Ok(if io_handle == output_handle() {
            OUTPUT_RATE_HZ
        } else {
            0
        })
    }

    /// AudioSystem::getFrameCount: 0 for an unknown handle, as AOSP.
    fn r#frameCount(&self, io_handle: i32) -> binder::Result<i64> {
        counters::answered(INTERFACE, "frameCount");
        Ok(if io_handle == output_handle() {
            OUTPUT_FRAMES
        } else {
            0
        })
    }

    /// AudioSystem::getLatency, ms.
    fn r#latency(&self, output: i32) -> binder::Result<i32> {
        counters::answered(INTERFACE, "latency");
        Ok(if output == output_handle() {
            OUTPUT_LATENCY_MS
        } else {
            0
        })
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

    /// AudioFlinger::createTrack (Tracks.cpp): a PCM 16-bit mono or stereo track with its region and IAudioTrack. A streaming track gets its own buffer after the control block; a shared-buffer (static) track takes the client's (SoundPool's streams).
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
        let Some(channels) = pcm16_channels(config) else {
            tracing::warn!("createTrack: format or layout not served");
            return refused();
        };
        let session = match request.sessionId {
            0 => ids::next(ids::USE_SESSION).expect("USE_SESSION is below AUDIO_UNIQUE_ID_USE_MAX"),
            s if s > 0 => s,
            _ => return refused(),
        };
        let frame_bytes = channels * BYTES_PER_SAMPLE;
        let (frames, buffer_bytes) = match request.sharedBuffer.as_ref() {
            // static: the client's buffer is the track's buffer, and the frames are the buffer's (Threads.cpp:2569: a PCM shared buffer ignores the request's frame count). No roundup (TrackBase:162). The fd is not kept: the data plane (A2) must keep it.
            Some(shared) => {
                let frames = i64::try_from(frame_bytes)
                    .ok()
                    .and_then(|bytes| usize::try_from(shared.size / bytes).ok())
                    .unwrap_or(0);
                if frames == 0 {
                    return refused();
                }
                (frames, 0)
            }
            None => {
                let Some(frames) = track::round_frames(request.frameCount) else {
                    return refused();
                };
                let Some(bytes) = frames.checked_mul(frame_bytes) else {
                    return refused();
                };
                (frames, bytes)
            }
        };
        if request.flags & AUDIO_OUTPUT_FLAG_FAST != 0 {
            counters::refused(INTERFACE, "createTrack.FAST");
        }
        let region = match track::allocate(buffer_bytes, self.opts.break_cblk) {
            Ok(region) => region,
            Err(status) => {
                counters::refused(INTERFACE, "createTrack.region");
                return Err(status.into());
            }
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
            r#afFrameCount: OUTPUT_FRAMES,
            r#afSampleRate: OUTPUT_RATE_HZ,
            r#afChannelMask: Some(output_layout()),
            r#afFormat: Some(output_format()),
            r#afLatencyMs: OUTPUT_LATENCY_MS,
            r#afTrackFlags: 0,
            r#outputId: output_handle(),
            r#portId: ids::next(ids::USE_CLIENT)
                .expect("USE_CLIENT is below AUDIO_UNIQUE_ID_USE_MAX"),
            r#audioTrack: Some(track::new_binder(region)),
        }))
    }
}

/// The binder served as [`crate::FLINGER_NAME`].
pub(crate) fn new_binder(opts: Options) -> SpIBinder {
    BnAudioFlingerService::new_binder(AudioFlinger { opts }, BinderFeatures::default()).as_binder()
}
