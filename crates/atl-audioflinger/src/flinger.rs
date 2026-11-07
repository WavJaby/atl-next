// SPDX-License-Identifier: MIT
//! IAudioFlingerService (AOSP AudioFlinger.cpp; AIDL in libaudioclient `aidl/`): the calls AudioSystem makes for an id, the output's rate, frame count and latency, and the client registration.
//! Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). Output and track calls (openOutput, createTrack) come with A1b and A2.

use std::sync::OnceLock;

use atl_audioflinger_stubs::aidl::android::media::IAudioFlingerClient::IAudioFlingerClient;
use atl_audioflinger_stubs::aidl::android::media::IAudioFlingerService::{
    BnAudioFlingerService, IAudioFlingerService,
};
use atl_services::counters;
use binder::{BinderFeatures, Interface, SpIBinder, Status, Strong};

use crate::ids;
use crate::{OUTPUT_FRAMES, OUTPUT_LATENCY_MS, OUTPUT_RATE_HZ};

const INTERFACE: &str = "IAudioFlingerService";

/// The audio_io_handle_t of the one output: a unique id of use OUTPUT, made at first use and kept.
pub(crate) fn output_handle() -> i32 {
    static OUTPUT: OnceLock<i32> = OnceLock::new();
    *OUTPUT.get_or_init(|| {
        ids::next(ids::USE_OUTPUT).expect("USE_OUTPUT is below AUDIO_UNIQUE_ID_USE_MAX")
    })
}

struct AudioFlinger;

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
}

/// The binder served as [`crate::FLINGER_NAME`].
pub(crate) fn new_binder() -> SpIBinder {
    BnAudioFlingerService::new_binder(AudioFlinger, BinderFeatures::default()).as_binder()
}
