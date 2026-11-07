// SPDX-License-Identifier: MIT
//! IAudioPolicyService (AOSP AudioPolicyService.cpp; AIDL in libaudioclient `aidl/`): getOutput, which AudioSystem's rate, frame count and latency queries start from; getOutputForAttr, which a track's construction asks for its output and port id; and the client registration.
//! Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). Routing is one output for every attribute: no device or stream policy yet.

use android_media_audio_common_types::mangled::{
    _7_android_5_media_5_audio_6_common_11_AudioConfig as AudioConfig,
    _7_android_5_media_5_audio_6_common_15_AudioAttributes as AudioAttributes,
    _7_android_5_media_5_audio_6_common_15_AudioConfigBase as AudioConfigBase,
    _7_android_5_media_5_audio_6_common_15_AudioStreamType as AudioStreamType,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyService::{
    BnAudioPolicyService, IAudioPolicyService,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyServiceClient::IAudioPolicyServiceClient;
use atl_audioflinger_stubs::mangled::_7_android_5_media_24_GetOutputForAttrResponse as GetOutputForAttrResponse;
use atl_services::counters;
use binder::{BinderFeatures, Interface, SpIBinder, Status, StatusCode, Strong};
use framework_permission_aidl::mangled::_7_android_7_content_22_AttributionSourceState as AttributionSourceState;

use crate::OUTPUT_RATE_HZ;
use crate::flinger::{output_format, output_handle, output_layout};
use crate::ids;

const INTERFACE: &str = "IAudioPolicyService";

/// AudioAttributes is an AIDL parcelable without Clone: its fields are copied one by one.
fn copy_attributes(attr: &AudioAttributes) -> AudioAttributes {
    AudioAttributes {
        r#contentType: attr.r#contentType,
        r#usage: attr.r#usage,
        r#source: attr.r#source,
        r#flags: attr.r#flags,
        r#tags: attr.r#tags.clone(),
    }
}

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

    /// AudioPolicyService::getOutputForAttr: the one output, a port id for the track, the attributes echoed back. Every usage maps to STREAM_MUSIC (no stream policy yet). The config is the output's profile, which AudioTrack reads as the output's.
    fn r#getOutputForAttr(
        &self,
        attr: Option<&AudioAttributes>,
        _session: i32,
        _attribution_source: Option<&AttributionSourceState>,
        _config: Option<&AudioConfig>,
        _flags: i32,
        _selected_device_ids: Option<&[i32]>,
    ) -> binder::Result<Option<GetOutputForAttrResponse>> {
        let Some(attr) = attr else {
            counters::refused(INTERFACE, "getOutputForAttr");
            return Err(StatusCode::BAD_VALUE.into());
        };
        counters::answered(INTERFACE, "getOutputForAttr");
        Ok(Some(GetOutputForAttrResponse {
            r#output: output_handle(),
            r#stream: AudioStreamType::MUSIC,
            r#selectedDeviceIds: Some(Vec::new()),
            r#portId: ids::next(ids::USE_CLIENT)
                .expect("USE_CLIENT is below AUDIO_UNIQUE_ID_USE_MAX"),
            r#secondaryOutputs: Some(Vec::new()),
            r#isSpatialized: false,
            r#configBase: Some(AudioConfigBase {
                r#sampleRate: OUTPUT_RATE_HZ,
                r#channelMask: output_layout(),
                r#format: output_format(),
            }),
            r#isBitPerfect: false,
            r#attr: Some(copy_attributes(attr)),
            r#volume: 1.0,
            r#muted: false,
        }))
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
