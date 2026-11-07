// SPDX-License-Identifier: MIT
//! IAudioPolicyService (AOSP AudioPolicyService.cpp; AIDL in libaudioclient `aidl/`): getOutput, which AudioSystem's rate, frame count and latency queries start from, and the client registration.
//! Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). getOutputForAttr is not served: its only server-side caller is AudioFlinger::createTrack (AudioFlinger.cpp:1120), which this server replaces, and no client path calls it (AudioTrack gets its output from createTrack's response). Routing is one output for every stream.

use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyService::{
    BnAudioPolicyService, IAudioPolicyService,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyServiceClient::IAudioPolicyServiceClient;
use atl_services::counters;
use binder::{BinderFeatures, Interface, SpIBinder, Status, Strong};

use crate::flinger::output_handle;

use android_media_audio_common_types::mangled::_7_android_5_media_5_audio_6_common_15_AudioStreamType as AudioStreamType;

const INTERFACE: &str = "IAudioPolicyService";

struct AudioPolicy;

impl Interface for AudioPolicy {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioPolicyService for AudioPolicy {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// AudioPolicyService::getOutput: the one output, for every stream (no routing policy yet).
    fn r#getOutput(&self, _stream: AudioStreamType) -> binder::Result<i32> {
        counters::answered(INTERFACE, "getOutput");
        Ok(output_handle())
    }

    /// AOSP keeps the client for device and port callbacks; none is sent here.
    fn r#registerClient(
        &self,
        _client: Option<&Strong<dyn IAudioPolicyServiceClient>>,
    ) -> binder::Result<()> {
        counters::answered(INTERFACE, "registerClient");
        Ok(())
    }
}

/// The binder served as [`crate::POLICY_NAME`].
pub(crate) fn new_binder() -> SpIBinder {
    BnAudioPolicyService::new_binder(AudioPolicy, BinderFeatures::default()).as_binder()
}
