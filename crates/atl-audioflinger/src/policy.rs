// SPDX-License-Identifier: MIT
//! IAudioPolicyService (AOSP AudioPolicyService.cpp; AIDL in libaudioclient `aidl/`): getOutput, which AudioSystem's rate, frame count and latency queries start from, the client registration, and the device
//! model: one output device, the built-in speaker (the host's default sink stands behind it; the same device dispatcher.rs registers with AudioService and createTrack reports as the routed device).
//! No input device is modelled. Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). getOutputForAttr is not served: its only server-side caller is AudioFlinger::createTrack
//! (AudioFlinger.cpp:1120), which this server replaces, and no client path calls it (AudioTrack gets its output from createTrack's response). Routing is one output for every stream.
//! Not modelled: the output mix port and its patch to the speaker (AudioPolicyManager lists both); listAudioPorts answers device ports only, listAudioPatches none.

use android_media_audio_common_types::mangled::{
    _7_android_5_media_5_audio_6_common_3_Int as Int,
    _7_android_5_media_5_audio_6_common_9_AudioPort as AudioPort,
    _7_android_5_media_5_audio_6_common_11_AudioDevice as AudioDevice,
    _7_android_5_media_5_audio_6_common_12_AudioIoFlags as AudioIoFlags,
    _7_android_5_media_5_audio_6_common_12_AudioPortExt as AudioPortExt,
    _7_android_5_media_5_audio_6_common_15_AudioDeviceType as AudioDeviceType,
    _7_android_5_media_5_audio_6_common_15_AudioMMapPolicy as AudioMMapPolicy,
    _7_android_5_media_5_audio_6_common_15_AudioPortConfig as AudioPortConfig,
    _7_android_5_media_5_audio_6_common_15_AudioStreamType as AudioStreamType,
    _7_android_5_media_5_audio_6_common_16_AudioOffloadInfo as AudioOffloadInfo,
    _7_android_5_media_5_audio_6_common_18_AudioDeviceAddress as AudioDeviceAddress,
    _7_android_5_media_5_audio_6_common_18_AudioPortDeviceExt as AudioPortDeviceExt,
    _7_android_5_media_5_audio_6_common_19_AudioMMapPolicyInfo as AudioMMapPolicyInfo,
    _7_android_5_media_5_audio_6_common_19_AudioMMapPolicyType as AudioMMapPolicyType,
    _7_android_5_media_5_audio_6_common_22_AudioDeviceDescription as AudioDeviceDescription,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyService::{
    BnAudioPolicyService, IAudioPolicyService,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyServiceClient::IAudioPolicyServiceClient;
use atl_audioflinger_stubs::mangled::{
    _7_android_5_media_16_AudioOffloadMode as AudioOffloadMode,
    _7_android_5_media_22_AudioPolicyDeviceState as AudioPolicyDeviceState,
};
use audioclient_types_aidl::mangled::{
    _7_android_5_media_11_AudioPortFw as AudioPortFw,
    _7_android_5_media_12_AudioPatchFw as AudioPatchFw,
    _7_android_5_media_12_AudioPortSys as AudioPortSys,
    _7_android_5_media_13_AudioPortRole as AudioPortRole,
    _7_android_5_media_13_AudioPortType as AudioPortType,
    _7_android_5_media_15_AudioPortExtSys as AudioPortExtSys,
    _7_android_5_media_17_AudioPortConfigFw as AudioPortConfigFw,
    _7_android_5_media_18_AudioPortConfigSys as AudioPortConfigSys,
    _7_android_5_media_21_AudioPortDeviceExtSys as AudioPortDeviceExtSys,
};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, Strong};

use crate::flinger::output_handle;
use crate::ids;

const INTERFACE: &str = "IAudioPolicyService";

/// The speaker's audio_port_handle_t. Policy port handles are their own namespace, not AudioFlinger's unique ids (PolicyAudioPort.cpp:43 getNextUniqueId -> getNextHandle); the one device takes the first.
pub(crate) const SPEAKER_PORT_ID: i32 = 1;
/// The port configuration generation listAudioPorts and listAudioPatches answer: nothing is ever connected or disconnected, so it never steps. AudioManager.updateAudioPortCache needs the two equal.
const PORT_GENERATION: i32 = 1;
/// AudioPolicyInterfaceImpl.cpp:57 MAX_ITEMS_PER_LIST: the most entries one list call fills.
const MAX_ITEMS_PER_LIST: i32 = 1024;
/// The speaker's name as AOSP's default configuration declares it (audio_policy_configuration.xml devicePort tagName "Speaker").
const SPEAKER_NAME: &str = "Speaker";

/// The primary HAL module's handle: a unique id of use MODULE (AudioFlinger::loadHwModule), made at first use and kept.
fn module_handle() -> i32 {
    static MODULE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *MODULE.get_or_init(|| {
        ids::next(ids::USE_MODULE).expect("USE_MODULE is below AUDIO_UNIQUE_ID_USE_MAX")
    })
}

/// The speaker as an AudioDevice: type OUT_SPEAKER, no connection, no address (aidl2legacy: AUDIO_DEVICE_OUT_SPEAKER, address "").
fn speaker_device() -> AudioDevice {
    AudioDevice {
        r#type: AudioDeviceDescription {
            r#type: AudioDeviceType::OUT_SPEAKER,
            r#connection: String::new(),
        },
        r#address: AudioDeviceAddress::Id(String::new()),
    }
}

fn is_speaker(device: &AudioDevice) -> bool {
    let no_address = matches!(&device.r#address, AudioDeviceAddress::Id(a) if a.is_empty());
    device.r#type.r#type == AudioDeviceType::OUT_SPEAKER
        && device.r#type.r#connection.is_empty()
        && no_address
}

fn speaker_ext_sys() -> AudioPortExtSys {
    AudioPortExtSys::Device(AudioPortDeviceExtSys {
        r#hwModule: module_handle(),
    })
}

/// The speaker's port as DeviceDescriptor::toAudioPort fills it: sink, device, an output port with no declared profile or gain (AudioDeviceInfo then reports no fixed rates, channel
/// masks or encodings: any is accepted) and its active configuration (DeviceDescriptor::toAudioPortConfig: id and port id the device's, the module handle, no preferred config).
fn speaker_port() -> AudioPortFw {
    let ext = || {
        AudioPortExt::Device(AudioPortDeviceExt {
            r#device: speaker_device(),
            ..Default::default()
        })
    };
    AudioPortFw {
        r#hal: AudioPort {
            r#id: SPEAKER_PORT_ID,
            r#name: SPEAKER_NAME.into(),
            r#flags: AudioIoFlags::Output(0),
            r#ext: ext(),
            ..Default::default()
        },
        r#sys: AudioPortSys {
            r#role: AudioPortRole::SINK,
            r#type: AudioPortType::DEVICE,
            r#activeConfig: AudioPortConfigFw {
                r#hal: AudioPortConfig {
                    r#id: SPEAKER_PORT_ID,
                    r#portId: SPEAKER_PORT_ID,
                    r#ext: ext(),
                    ..Default::default()
                },
                r#sys: AudioPortConfigSys {
                    r#role: AudioPortRole::SINK,
                    r#type: AudioPortType::DEVICE,
                    r#ext: speaker_ext_sys(),
                },
            },
            r#ext: speaker_ext_sys(),
            ..Default::default()
        },
    }
}

/// The MMAP policy types AudioPolicyManager answers for: DEFAULT and EXCLUSIVE; any other is BAD_VALUE.
fn mmap_type(policy_type: AudioMMapPolicyType) -> binder::Result<()> {
    if matches!(
        policy_type,
        AudioMMapPolicyType::DEFAULT | AudioMMapPolicyType::EXCLUSIVE
    ) {
        Ok(())
    } else {
        Err(StatusCode::BAD_VALUE.into())
    }
}

struct AudioPolicy;

impl Interface for AudioPolicy {}

/// AOSP's AIDL method names; the generated trait declares them raw.
#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IAudioPolicyService for AudioPolicy {
    /// AudioPolicyService::getOutput: the one output, for every stream (no routing policy yet).
    fn r#getOutput(&self, _stream: AudioStreamType) -> binder::Result<i32> {
        atl_diag::answered(INTERFACE, "getOutput");
        Ok(output_handle())
    }

    /// AOSP keeps the client for device and port callbacks; none is sent here, since no device or port ever changes.
    fn r#registerClient(
        &self,
        _client: Option<&Strong<dyn IAudioPolicyServiceClient>>,
    ) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "registerClient");
        Ok(())
    }

    /// AudioPolicyService::setAudioPortCallbacksEnabled: marks the client for port-list callbacks; none are ever due.
    fn r#setAudioPortCallbacksEnabled(&self, _enabled: bool) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "setAudioPortCallbacksEnabled");
        Ok(())
    }

    /// AudioPolicyManager::getDeviceConnectionState (AudioPolicyManager.cpp:523): AVAILABLE for a device in the available list (the speaker, address ""), UNAVAILABLE for every other.
    fn r#getDeviceConnectionState(
        &self,
        device: Option<&AudioDevice>,
    ) -> binder::Result<AudioPolicyDeviceState> {
        atl_diag::answered(INTERFACE, "getDeviceConnectionState");
        Ok(if device.is_some_and(is_speaker) {
            AudioPolicyDeviceState::AVAILABLE
        } else {
            AudioPolicyDeviceState::UNAVAILABLE
        })
    }

    /// AudioPolicyService::listAudioPorts (AudioPolicyInterfaceImpl.cpp:1777) over AudioPolicyManager::listAudioPorts (:5275): the speaker for role SINK/NONE and type DEVICE/NONE; `count` in =
    /// the most to fill (capped at MAX_ITEMS_PER_LIST), out = how many exist; returns the generation.
    fn r#listAudioPorts(
        &self,
        role: AudioPortRole,
        r#type: AudioPortType,
        count: &mut Option<Int>,
        ports: &mut Option<Vec<Option<AudioPortFw>>>,
    ) -> binder::Result<i32> {
        let wanted = count.as_ref().map_or(0, |c| c.r#value);
        if wanted < 0 {
            return Err(StatusCode::BAD_VALUE.into());
        }
        let mut all = Vec::new();
        if matches!(r#type, AudioPortType::NONE | AudioPortType::DEVICE)
            && matches!(role, AudioPortRole::NONE | AudioPortRole::SINK)
        {
            all.push(speaker_port());
        }
        let total = i32::try_from(all.len()).map_err(|_| StatusCode::BAD_VALUE)?;
        let filled = usize::try_from(wanted.min(MAX_ITEMS_PER_LIST).min(total))
            .map_err(|_| StatusCode::BAD_VALUE)?;
        *ports = Some(all.into_iter().take(filled).map(Some).collect());
        *count = Some(Int { r#value: total });
        atl_diag::answered(INTERFACE, "listAudioPorts");
        Ok(PORT_GENERATION)
    }

    /// AudioPolicyManager::getAudioPort (:5367): the port of `port_id`; BAD_VALUE for none or an unknown id.
    fn r#getAudioPort(&self, port_id: i32) -> binder::Result<Option<AudioPortFw>> {
        if port_id != SPEAKER_PORT_ID {
            return Err(StatusCode::BAD_VALUE.into());
        }
        atl_diag::answered(INTERFACE, "getAudioPort");
        Ok(Some(speaker_port()))
    }

    /// AudioPolicyService::listAudioPatches: no patch is modelled (module docs); the count is 0 and the generation equals listAudioPorts', as AudioManager.updateAudioPortCache requires.
    fn r#listAudioPatches(
        &self,
        count: &mut Option<Int>,
        patches: &mut Option<Vec<Option<AudioPatchFw>>>,
    ) -> binder::Result<i32> {
        *patches = Some(Vec::new());
        *count = Some(Int { r#value: 0 });
        atl_diag::answered(INTERFACE, "listAudioPatches");
        Ok(PORT_GENERATION)
    }

    /// AudioPolicyManager::getOffloadSupport (:4867): NOT_SUPPORTED when no output profile has AUDIO_OUTPUT_FLAG_COMPRESS_OFFLOAD; the one output here is a PCM mixer, so never offload.
    fn r#getOffloadSupport(
        &self,
        _info: Option<&AudioOffloadInfo>,
    ) -> binder::Result<AudioOffloadMode> {
        atl_diag::answered(INTERFACE, "getOffloadSupport");
        Ok(AudioOffloadMode::NOT_SUPPORTED)
    }

    /// AudioPolicyManager::getMmapPolicyInfos (:9516): BAD_VALUE for a type other than DEFAULT / EXCLUSIVE; else what the HAL reports. This output has no MMAP path (output.rs mixes into a
    /// socket stream), so none: an empty list, and AAudio takes its legacy (AudioTrack) path.
    fn r#getMmapPolicyInfos(
        &self,
        policy_type: AudioMMapPolicyType,
    ) -> binder::Result<Option<Vec<Option<AudioMMapPolicyInfo>>>> {
        mmap_type(policy_type)?;
        atl_diag::answered(INTERFACE, "getMmapPolicyInfos");
        Ok(Some(Vec::new()))
    }

    /// AudioPolicyManager::getMmapPolicyForDevice (:9531): BAD_VALUE for a type other than DEFAULT / EXCLUSIVE; a device with no reported policy gets NEVER, and none is reported here.
    fn r#getMmapPolicyForDevice(
        &self,
        policy_type: AudioMMapPolicyType,
        policy_info: &mut Option<AudioMMapPolicyInfo>,
    ) -> binder::Result<()> {
        mmap_type(policy_type)?;
        let info = policy_info.get_or_insert_with(Default::default);
        info.r#mmapPolicy = AudioMMapPolicy::NEVER;
        atl_diag::answered(INTERFACE, "getMmapPolicyForDevice");
        Ok(())
    }
}

/// The binder served as [`crate::POLICY_NAME`].
pub(crate) fn new_binder() -> SpIBinder {
    BnAudioPolicyService::new_binder(AudioPolicy, BinderFeatures::default()).as_binder()
}
