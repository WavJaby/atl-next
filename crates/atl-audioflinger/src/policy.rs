// SPDX-License-Identifier: MIT
//! IAudioPolicyService (AOSP AudioPolicyService.cpp; AIDL in libaudioclient `aidl/`): getOutput, which AudioSystem's rate, frame count and latency queries start from, the client registration, and the device
//! model: one output device, the built-in speaker (the host's default sink stands behind it; the same device dispatcher.rs registers with AudioService and createTrack reports as the routed device).
//! No input device is modelled. Other methods keep the stub's default (UNKNOWN_TRANSACTION, counted). getOutputForAttr is not served: its only server-side caller is AudioFlinger::createTrack
//! (AudioFlinger.cpp:1120), which this server replaces, and no client path calls it (AudioTrack gets its output from createTrack's response). Routing is one output for every stream.
//! The primary output mix (a SOURCE MIX port; its active config carries getOutput's I/O handle) and its patch to the speaker are listed from startup, as the route is fixed (no hotplug, no reroute). AOSP lists a mix only while its output is open.
//! The product strategy and volume group transactions are AOSP's engine's answers (atl-audio-bridge engine.rs): this file unparcels the attributes and the answer, and maps the engine's status to the binder's error.
//! The service rules of AudioPolicyInterfaceImpl.cpp are here too, at the overrides that implement those calls: the public stream range, and NONE with the fallback as BAD_VALUE.

use android_media_audio_common_types::mangled::{
    _7_android_5_media_5_audio_6_common_3_Int as Int,
    _7_android_5_media_5_audio_6_common_9_AudioPort as AudioPort,
    _7_android_5_media_5_audio_6_common_11_AudioDevice as AudioDevice,
    _7_android_5_media_5_audio_6_common_12_AudioIoFlags as AudioIoFlags,
    _7_android_5_media_5_audio_6_common_12_AudioPortExt as AudioPortExt,
    _7_android_5_media_5_audio_6_common_15_AudioAttributes as AudioAttributes,
    _7_android_5_media_5_audio_6_common_15_AudioDeviceType as AudioDeviceType,
    _7_android_5_media_5_audio_6_common_15_AudioMMapPolicy as AudioMMapPolicy,
    _7_android_5_media_5_audio_6_common_15_AudioPortConfig as AudioPortConfig,
    _7_android_5_media_5_audio_6_common_15_AudioPortMixExt as AudioPortMixExt,
    _7_android_5_media_5_audio_6_common_15_AudioStreamType as AudioStreamType,
    _7_android_5_media_5_audio_6_common_16_AudioOffloadInfo as AudioOffloadInfo,
    _7_android_5_media_5_audio_6_common_18_AudioDeviceAddress as AudioDeviceAddress,
    _7_android_5_media_5_audio_6_common_18_AudioPortDeviceExt as AudioPortDeviceExt,
    _7_android_5_media_5_audio_6_common_19_AudioMMapPolicyInfo as AudioMMapPolicyInfo,
    _7_android_5_media_5_audio_6_common_19_AudioMMapPolicyType as AudioMMapPolicyType,
    _7_android_5_media_5_audio_6_common_22_AudioDeviceDescription as AudioDeviceDescription,
    _7_android_5_media_5_audio_6_common_22_AudioPortMixExtUseCase as AudioPortMixExtUseCase,
};
use atl_audio_bridge::PolicyEngine;
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyService::{
    BnAudioPolicyService, IAudioPolicyService,
};
use atl_audioflinger_stubs::aidl::android::media::IAudioPolicyServiceClient::IAudioPolicyServiceClient;
use atl_audioflinger_stubs::mangled::{
    _7_android_5_media_16_AudioOffloadMode as AudioOffloadMode,
    _7_android_5_media_16_AudioVolumeGroup as AudioVolumeGroup,
    _7_android_5_media_20_AudioProductStrategy as AudioProductStrategy,
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
    _7_android_5_media_18_AudioPortMixExtSys as AudioPortMixExtSys,
    _7_android_5_media_21_AudioPortDeviceExtSys as AudioPortDeviceExtSys,
};
use binder::binder_impl::{Deserialize, Parcel};
use binder::{BinderFeatures, Interface, SpIBinder, Status, StatusCode, Strong};

use crate::flinger::{output_format, output_handle, output_layout};
use crate::ids;
use std::sync::{Mutex, MutexGuard, PoisonError};

const INTERFACE: &str = "IAudioPolicyService";

/// The speaker's audio_port_handle_t. Policy port handles are their own namespace, not AudioFlinger's unique ids (PolicyAudioPort.cpp:43 getNextUniqueId -> getNextHandle); the speaker takes the first, the mix the second.
pub(crate) const SPEAKER_PORT_ID: i32 = 1;
/// The primary output mix's audio_port_handle_t: the second policy handle (PolicyAudioPort.cpp:43).
const MIX_PORT_ID: i32 = 2;
/// The port configuration generation listAudioPorts and listAudioPatches answer: nothing is ever connected or disconnected, so it never steps. AudioManager.updateAudioPortCache needs the two equal.
const PORT_GENERATION: i32 = 1;
/// AudioPolicyInterfaceImpl.cpp:57 MAX_ITEMS_PER_LIST: the most entries one list call fills.
const MAX_ITEMS_PER_LIST: i32 = 1024;
/// The speaker's name as AOSP's default configuration declares it (audio_policy_configuration.xml devicePort tagName "Speaker").
const SPEAKER_NAME: &str = "Speaker";
/// AOSP's default name for the primary output mix (primary_audio_policy_configuration.xml mixPort "primary output").
const MIX_NAME: &str = "primary output";
/// `AUDIO_OUTPUT_FLAG_PRIMARY` (audio-hal-enums.h:438): the module's primary output.
const AUDIO_OUTPUT_FLAG_PRIMARY: i32 = 0x2;
/// `STRATEGY_NONE` (policy.h:46, V(STRATEGY_NONE, -1)): PRODUCT_STRATEGY_NONE is the same value (AudioCommonTypes.h:93).
const PRODUCT_STRATEGY_NONE: i32 = -1;
/// VOLUME_GROUP_NONE (AudioCommonTypes.h:148): no volume group.
const VOLUME_GROUP_NONE: i32 = -1;
/// AUDIO_STREAM_PUBLIC_CNT (audio-base-utils.h:32, ASSISTANT + 1; AUDIO_STREAM_ASSISTANT is 11, audio-hal-enums.h:773): the streams with a public number.
const AUDIO_STREAM_PUBLIC_CNT: u32 = 12;

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

/// The speaker's active configuration (DeviceDescriptor::toAudioPortConfig): the device's id and port id, the device ext, the module handle; no preferred config. The patch's sink end.
fn speaker_config() -> AudioPortConfigFw {
    AudioPortConfigFw {
        r#hal: AudioPortConfig {
            r#id: SPEAKER_PORT_ID,
            r#portId: SPEAKER_PORT_ID,
            r#ext: AudioPortExt::Device(AudioPortDeviceExt {
                r#device: speaker_device(),
                ..Default::default()
            }),
            ..Default::default()
        },
        r#sys: AudioPortConfigSys {
            r#role: AudioPortRole::SINK,
            r#type: AudioPortType::DEVICE,
            r#ext: speaker_ext_sys(),
        },
    }
}

/// The speaker's port as DeviceDescriptor::toAudioPort fills it: sink, device, an output port with no declared profile or gain (AudioDeviceInfo then reports no fixed rates, channel masks or encodings: any is accepted) and its active configuration (speaker_config).
fn speaker_port() -> AudioPortFw {
    AudioPortFw {
        r#hal: AudioPort {
            r#id: SPEAKER_PORT_ID,
            r#name: SPEAKER_NAME.into(),
            r#flags: AudioIoFlags::Output(0),
            r#ext: AudioPortExt::Device(AudioPortDeviceExt {
                r#device: speaker_device(),
                ..Default::default()
            }),
            ..Default::default()
        },
        r#sys: AudioPortSys {
            r#role: AudioPortRole::SINK,
            r#type: AudioPortType::DEVICE,
            r#activeConfig: speaker_config(),
            r#ext: speaker_ext_sys(),
            ..Default::default()
        },
    }
}

/// The mix ext, hal side: the open and active counts, which AudioPort.h:133,139 default to 1 (the primary configuration sets neither). No I/O handle: AOSP's port-level AudioPortMixExt carries none (AudioPort.cpp:249-256); the handle is on the active config (mix_config). The module is on the sys side (AudioPortMixExtSys).
fn mix_ext() -> AudioPortMixExt {
    AudioPortMixExt {
        r#maxOpenStreamCount: 1,
        r#maxActiveStreamCount: 1,
        ..Default::default()
    }
}

/// The mix's active configuration (AudioOutputDescriptor::toAudioPortConfig, SwAudioOutputDescriptor::toAudioPortConfig): the mix's id and port id, the I/O handle, stream AUDIO_STREAM_DEFAULT (SYS_RESERVED_DEFAULT), the module; the channel mask and format createTrack reports for the output (flinger.rs). No sample rate: the output grants it at open (output.rs).
fn mix_config() -> AudioPortConfigFw {
    AudioPortConfigFw {
        r#hal: AudioPortConfig {
            r#id: MIX_PORT_ID,
            r#portId: MIX_PORT_ID,
            r#channelMask: Some(output_layout()),
            r#format: Some(output_format()),
            r#ext: AudioPortExt::Mix(AudioPortMixExt {
                r#handle: output_handle(),
                r#usecase: AudioPortMixExtUseCase::Stream(AudioStreamType::SYS_RESERVED_DEFAULT),
                ..Default::default()
            }),
            ..Default::default()
        },
        r#sys: AudioPortConfigSys {
            r#role: AudioPortRole::SOURCE,
            r#type: AudioPortType::MIX,
            r#ext: AudioPortExtSys::Mix(AudioPortMixExtSys {
                r#hwModule: module_handle(),
            }),
        },
    }
}

/// The primary output mix as AudioOutputDescriptor::toAudioPort fills it: source, mix, PRIMARY flags, its name, the mix ext and module, and its active configuration (mix_config). Like the speaker, no declared profile.
fn mix_port() -> AudioPortFw {
    AudioPortFw {
        r#hal: AudioPort {
            r#id: MIX_PORT_ID,
            r#name: MIX_NAME.into(),
            r#flags: AudioIoFlags::Output(AUDIO_OUTPUT_FLAG_PRIMARY),
            r#ext: AudioPortExt::Mix(mix_ext()),
            ..Default::default()
        },
        r#sys: AudioPortSys {
            r#role: AudioPortRole::SOURCE,
            r#type: AudioPortType::MIX,
            r#activeConfig: mix_config(),
            r#ext: AudioPortExtSys::Mix(AudioPortMixExtSys {
                r#hwModule: module_handle(),
            }),
            ..Default::default()
        },
    }
}

/// Every port in AudioPolicyManager's list order: the device, then the output mix.
fn all_ports() -> Vec<AudioPortFw> {
    vec![speaker_port(), mix_port()]
}

/// The patch's audio_patch_handle_t: AUDIO_UNIQUE_ID_USE_PATCH (audio.h:194), made at first use and kept.
fn patch_handle() -> i32 {
    static PATCH: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *PATCH.get_or_init(|| {
        ids::next(ids::USE_PATCH).expect("USE_PATCH is below AUDIO_UNIQUE_ID_USE_MAX")
    })
}

/// The primary mix to the speaker: the mix's config is the source end, the speaker's the sink end (one patch: AudioPatchCollection's mPatch with one source and one sink).
fn primary_patch() -> AudioPatchFw {
    AudioPatchFw {
        r#id: patch_handle(),
        r#sources: vec![mix_config()],
        r#sinks: vec![speaker_config()],
    }
}

/// The room rule of AudioPolicyService's list calls: `count` in = the most to fill (capped at MAX_ITEMS_PER_LIST), out = how many exist (AudioPolicyInterfaceImpl.cpp:1777, :1906); a negative room is BAD_VALUE (convertIntegral).
fn fill_list<T>(
    all: Vec<T>,
    count: &mut Option<Int>,
    out: &mut Option<Vec<Option<T>>>,
) -> binder::Result<()> {
    let wanted = count.as_ref().map_or(0, |c| c.r#value);
    if wanted < 0 {
        return Err(StatusCode::BAD_VALUE.into());
    }
    let total = i32::try_from(all.len()).map_err(|_| StatusCode::BAD_VALUE)?;
    let filled = usize::try_from(wanted.min(MAX_ITEMS_PER_LIST).min(total))
        .map_err(|_| StatusCode::BAD_VALUE)?;
    *out = Some(all.into_iter().take(filled).map(Some).collect());
    *count = Some(Int { r#value: total });
    Ok(())
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

/// AudioPolicyInterfaceImpl.cpp:1384 compares the stream as uint32_t, so a negative stream is past the public range too.
fn is_public_stream(stream: i32) -> bool {
    u32::try_from(stream).is_ok_and(|s| s < AUDIO_STREAM_PUBLIC_CNT)
}

/// The engine this process answers from: AOSP's default-config engine, loaded at first use. A failed load is every answer's error (AOSP has no policy manager then, NO_INIT-like).
fn engine() -> binder::Result<MutexGuard<'static, PolicyEngine>> {
    static ENGINE: std::sync::OnceLock<Result<Mutex<PolicyEngine>, i32>> =
        std::sync::OnceLock::new();
    match ENGINE.get_or_init(|| PolicyEngine::load().map(Mutex::new)) {
        Ok(engine) => Ok(engine.lock().unwrap_or_else(PoisonError::into_inner)),
        Err(status) => Err(status_error(*status)),
    }
}

/// AOSP's status as the binder's error: binderStatusFromStatusT.
fn status_error(status: i32) -> Status {
    Status::from(status)
}

/// The list an engine parcel holds: an int32 count, then each parcelable (engine.cpp).
fn unparcel_list<T: Deserialize>(bytes: &[u8]) -> binder::Result<Vec<T>> {
    let parcel = Parcel::unmarshal(bytes)?;
    let count: i32 = parcel.read()?;
    let mut list = Vec::new();
    for _ in 0..count {
        list.push(parcel.read()?);
    }
    Ok(list)
}

/// The AudioAttributes parcelable as the engine reads it: the marker and sized body that Parcel::write gives. A null argument is BAD_VALUE (AIDL: not nullable).
fn attributes_bytes(aa: Option<&AudioAttributes>) -> binder::Result<Vec<u8>> {
    let aa = aa.ok_or(StatusCode::BAD_VALUE)?;
    let mut parcel = Parcel::new();
    parcel.write(aa)?;
    Ok(parcel.marshal()?)
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

    /// AudioPolicyService::listAudioPorts (AudioPolicyInterfaceImpl.cpp:1777) over AudioPolicyManager::listAudioPorts (:5275): the ports whose role and type match (NONE matches any), filled to `count` in; out = the total; returns the generation.
    fn r#listAudioPorts(
        &self,
        role: AudioPortRole,
        r#type: AudioPortType,
        count: &mut Option<Int>,
        ports: &mut Option<Vec<Option<AudioPortFw>>>,
    ) -> binder::Result<i32> {
        let listed = all_ports().into_iter().filter(|p| {
            (role == AudioPortRole::NONE || p.sys.role == role)
                && (r#type == AudioPortType::NONE || p.sys.r#type == r#type)
        });
        fill_list(listed.collect(), count, ports)?;
        atl_diag::answered(INTERFACE, "listAudioPorts");
        Ok(PORT_GENERATION)
    }

    /// AudioPolicyManager::getAudioPort (:5367): the port of `port_id`; BAD_VALUE for none or an unknown id.
    fn r#getAudioPort(&self, port_id: i32) -> binder::Result<Option<AudioPortFw>> {
        let port = all_ports()
            .into_iter()
            .find(|p| p.hal.id == port_id)
            .ok_or(StatusCode::BAD_VALUE)?;
        atl_diag::answered(INTERFACE, "getAudioPort");
        Ok(Some(port))
    }

    /// AudioPolicyService::listAudioPatches (AudioPolicyInterfaceImpl.cpp:1906): the primary mix's patch to the speaker, filled to `count` in; out = the total; the generation equals listAudioPorts' (AudioManager.updateAudioPortCache needs the two equal).
    fn r#listAudioPatches(
        &self,
        count: &mut Option<Int>,
        patches: &mut Option<Vec<Option<AudioPatchFw>>>,
    ) -> binder::Result<i32> {
        fill_list(vec![primary_patch()], count, patches)?;
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

    /// AudioPolicyService::getStrategyForStream (AudioPolicyInterfaceImpl.cpp:1379): NONE past the public range (1384-1387), else the engine's strategy for the stream.
    fn r#getStrategyForStream(&self, stream: AudioStreamType) -> binder::Result<i32> {
        let strategy = if is_public_stream(stream.0) {
            engine()?
                .product_strategy_for_stream(stream.0)
                .map_err(status_error)?
        } else {
            PRODUCT_STRATEGY_NONE
        };
        atl_diag::answered(INTERFACE, "getStrategyForStream");
        Ok(strategy)
    }

    /// AudioPolicyService::listAudioProductStrategies (AudioPolicyInterfaceImpl.cpp:2438): the engine's strategies as AudioProductStrategy parcelables.
    fn r#listAudioProductStrategies(
        &self,
    ) -> binder::Result<Option<Vec<Option<AudioProductStrategy>>>> {
        let parcel = engine()?.product_strategies().map_err(status_error)?;
        let strategies = unparcel_list::<AudioProductStrategy>(&parcel)?;
        atl_diag::answered(INTERFACE, "listAudioProductStrategies");
        Ok(Some(strategies.into_iter().map(Some).collect()))
    }

    /// AudioPolicyService::getProductStrategyFromAudioAttributes (AudioPolicyInterfaceImpl.cpp:2455): the engine's strategy for `aa`.
    fn r#getProductStrategyFromAudioAttributes(
        &self,
        aa: Option<&AudioAttributes>,
        fallback_on_default: bool,
    ) -> binder::Result<i32> {
        let attributes = attributes_bytes(aa)?;
        let strategy = engine()?
            .product_strategy_for_attributes(&attributes, fallback_on_default)
            .map_err(status_error)?;
        // AudioPolicyManager.h:389-391: NONE with the fallback is BAD_VALUE; AudioPolicyInterfaceImpl.cpp:2466-2468 returns the manager's status
        if fallback_on_default && strategy == PRODUCT_STRATEGY_NONE {
            return Err(StatusCode::BAD_VALUE.into());
        }
        atl_diag::answered(INTERFACE, "getProductStrategyFromAudioAttributes");
        Ok(strategy)
    }

    /// AudioPolicyService::listAudioVolumeGroups (AudioPolicyInterfaceImpl.cpp:2474): the engine's volume groups as AudioVolumeGroup parcelables.
    fn r#listAudioVolumeGroups(&self) -> binder::Result<Option<Vec<Option<AudioVolumeGroup>>>> {
        let parcel = engine()?.volume_groups().map_err(status_error)?;
        let groups = unparcel_list::<AudioVolumeGroup>(&parcel)?;
        atl_diag::answered(INTERFACE, "listAudioVolumeGroups");
        Ok(Some(groups.into_iter().map(Some).collect()))
    }

    /// AudioPolicyService::getVolumeGroupFromAudioAttributes (AudioPolicyInterfaceImpl.cpp:2489): the engine's volume group for `aa`.
    fn r#getVolumeGroupFromAudioAttributes(
        &self,
        aa: Option<&AudioAttributes>,
        fallback_on_default: bool,
    ) -> binder::Result<i32> {
        let attributes = attributes_bytes(aa)?;
        let group = engine()?
            .volume_group_for_attributes(&attributes, fallback_on_default)
            .map_err(status_error)?;
        // AudioPolicyManager.h:401-405: VOLUME_GROUP_NONE with the fallback is BAD_VALUE; AudioPolicyInterfaceImpl.cpp:2500-2504 returns the manager's status
        if fallback_on_default && group == VOLUME_GROUP_NONE {
            return Err(StatusCode::BAD_VALUE.into());
        }
        atl_diag::answered(INTERFACE, "getVolumeGroupFromAudioAttributes");
        Ok(group)
    }
}

/// The binder served as [`crate::POLICY_NAME`].
pub(crate) fn new_binder() -> SpIBinder {
    BnAudioPolicyService::new_binder(AudioPolicy, BinderFeatures::default()).as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Room for every entry: the list caps at MAX_ITEMS_PER_LIST, so this asks for all of them.
    const ALL: i32 = 1024;
    /// The primary mix's audio_port_handle_t: the second policy handle, after the speaker's (PolicyAudioPort.cpp:43). Literal: the spec's number, not the code's constant.
    const MIX_ID: i32 = 2;

    /// listAudioPorts with room for `room` entries: (generation, total, the entries filled).
    fn list_ports(
        role: AudioPortRole,
        r#type: AudioPortType,
        room: i32,
    ) -> (i32, i32, Vec<AudioPortFw>) {
        let mut count = Some(Int { r#value: room });
        let mut ports = None;
        let generation = AudioPolicy
            .r#listAudioPorts(role, r#type, &mut count, &mut ports)
            .expect("listAudioPorts");
        let total = count.expect("the total is answered").r#value;
        (
            generation,
            total,
            ports.unwrap_or_default().into_iter().flatten().collect(),
        )
    }

    /// The port `id` as getAudioPort answers it.
    fn port(id: i32) -> AudioPortFw {
        AudioPolicy
            .r#getAudioPort(id)
            .expect("getAudioPort")
            .expect("a port")
    }

    /// AudioPolicyManager::listAudioPorts (:5275): role and type filter; NONE matches any.
    #[test]
    fn port_list_filters_by_role_and_type() {
        assert_eq!(
            list_ports(AudioPortRole::NONE, AudioPortType::NONE, ALL).1,
            2
        );
        assert_eq!(
            list_ports(AudioPortRole::SOURCE, AudioPortType::MIX, ALL).1,
            1
        );
        assert_eq!(
            list_ports(AudioPortRole::SINK, AudioPortType::DEVICE, ALL).1,
            1
        );
        assert_eq!(
            list_ports(AudioPortRole::SOURCE, AudioPortType::DEVICE, ALL).1,
            0
        );
    }

    /// The room caps the entries filled, the total is always answered, and a negative room is BAD_VALUE (AudioPolicyInterfaceImpl.cpp:1777).
    #[test]
    fn port_list_fills_only_the_room_given() {
        let (_, total, entries) = list_ports(AudioPortRole::NONE, AudioPortType::NONE, 0);
        assert_eq!((total, entries.len()), (2, 0));
        let (_, total, entries) = list_ports(AudioPortRole::NONE, AudioPortType::NONE, 1);
        assert_eq!((total, entries.len()), (2, 1));
        assert_eq!(entries[0].hal.id, SPEAKER_PORT_ID);
        let mut count = Some(Int { r#value: -1 });
        let mut ports = None;
        let err = AudioPolicy
            .r#listAudioPorts(
                AudioPortRole::NONE,
                AudioPortType::NONE,
                &mut count,
                &mut ports,
            )
            .expect_err("a negative room");
        assert_eq!(err.transaction_error(), StatusCode::BAD_VALUE);
    }

    /// AudioPolicyManager::getAudioPort (:5367): both ports resolve by id; an id no port has is BAD_VALUE.
    #[test]
    fn both_ports_resolve_by_id() {
        assert_eq!(port(SPEAKER_PORT_ID).hal.id, SPEAKER_PORT_ID);
        assert_eq!(port(MIX_ID).hal.id, MIX_ID);
        let err = AudioPolicy.r#getAudioPort(3).expect_err("no port has id 3");
        assert_eq!(err.transaction_error(), StatusCode::BAD_VALUE);
    }

    /// The primary output: a SOURCE MIX port on the speaker's module. Its mix ext has no I/O handle: AOSP's port-level AudioPortMixExt carries the counts only (AudioPort.cpp:249-256); the handle is on the active config (topology test).
    #[test]
    fn mix_port_is_the_primary_output() {
        let mix = port(MIX_ID);
        assert_eq!(mix.sys.role, AudioPortRole::SOURCE);
        assert_eq!(mix.sys.r#type, AudioPortType::MIX);
        assert_eq!(mix.hal.name, "primary output");
        assert!(matches!(mix.hal.flags, AudioIoFlags::Output(0x2)));
        let AudioPortExt::Mix(ext) = &mix.hal.ext else {
            panic!("the mix port carries a mix ext");
        };
        // AUDIO_IO_HANDLE_NONE (audio.h:71) is 0: the port-level ext names no handle
        assert_eq!(ext.r#handle, 0);
        let AudioPortExtSys::Mix(sys) = &mix.sys.r#ext else {
            panic!("the mix port's sys ext is Mix");
        };
        let speaker = port(SPEAKER_PORT_ID);
        let AudioPortExtSys::Device(speaker_sys) = &speaker.sys.r#ext else {
            panic!("the speaker's sys ext is Device");
        };
        assert_eq!(sys.r#hwModule, speaker_sys.r#hwModule);
    }

    /// The speaker stays as it was: id 1, name "Speaker", OUT_SPEAKER at address "" (AudioPolicyManager::getDeviceConnectionState :523 reads the same device).
    #[test]
    fn speaker_is_unchanged() {
        let speaker = port(SPEAKER_PORT_ID);
        assert_eq!(speaker.hal.name, "Speaker");
        assert_eq!(speaker.sys.role, AudioPortRole::SINK);
        assert_eq!(speaker.sys.r#type, AudioPortType::DEVICE);
        let AudioPortExt::Device(ext) = &speaker.hal.ext else {
            panic!("the speaker's ext is Device");
        };
        assert!(is_speaker(&ext.r#device));
    }

    /// The topology: one patch from the primary mix (source) to the speaker (sink), on the port list's generation; the patch id is kept across calls (AudioPolicyManager::listAudioPatches :5902).
    #[test]
    fn topology_patch_connects_mix_to_speaker() {
        let (ports_generation, _, _) = list_ports(AudioPortRole::NONE, AudioPortType::NONE, 0);
        let list_patches = |room: i32| {
            let mut count = Some(Int { r#value: room });
            let mut patches = None;
            let generation = AudioPolicy
                .r#listAudioPatches(&mut count, &mut patches)
                .expect("listAudioPatches");
            (
                generation,
                count.expect("the total is answered").r#value,
                patches.unwrap_or_default(),
            )
        };
        let (patches_generation, total, listed) = list_patches(ALL);
        assert_eq!(total, 1);
        assert_eq!(patches_generation, ports_generation);
        let patch = listed.first().and_then(Option::as_ref).expect("one patch");
        assert_ne!(patch.id, 0);
        assert_eq!(patch.sources.len(), 1);
        let source = &patch.sources[0];
        assert_eq!(source.hal.portId, MIX_ID);
        // the I/O handle rides on the active config (AidlConversion.cpp:233-243) and is the one getOutput answers
        let AudioPortExt::Mix(config_ext) = &source.hal.ext else {
            panic!("the source config carries a mix ext");
        };
        let output = AudioPolicy
            .r#getOutput(AudioStreamType::MUSIC)
            .expect("getOutput");
        assert_eq!(config_ext.r#handle, output);
        assert_eq!(source.sys.role, AudioPortRole::SOURCE);
        assert_eq!(source.sys.r#type, AudioPortType::MIX);
        assert_eq!(patch.sinks.len(), 1);
        let sink = &patch.sinks[0];
        assert_eq!(sink.hal.portId, SPEAKER_PORT_ID);
        assert_eq!(sink.sys.role, AudioPortRole::SINK);
        assert_eq!(sink.sys.r#type, AudioPortType::DEVICE);
        let (_, _, again) = list_patches(ALL);
        assert_eq!(
            again.first().and_then(Option::as_ref).map(|p| p.id),
            Some(patch.id)
        );
    }

    use android_media_audio_common_types::mangled::{
        _7_android_5_media_5_audio_6_common_10_AudioUsage as AudioUsage,
        _7_android_5_media_5_audio_6_common_11_AudioSource as AudioSource,
        _7_android_5_media_5_audio_6_common_16_AudioContentType as AudioContentType,
    };

    /// policy.h:46 (V(STRATEGY_NONE, -1)): no strategy; getProductStrategyFromAudioAttributes answers it when fallback is off.
    const STRATEGY_NONE: i32 = -1;
    /// policy.h:52 (V(STRATEGY_MEDIA, 5)): the strategy of the media streams and of the default attributes.
    const STRATEGY_MEDIA: i32 = 5;
    /// policy.h:56 (V(STRATEGY_REROUTING, 9)): a system strategy the engine appends (EngineBase.cpp:244-247).
    const STRATEGY_REROUTING: i32 = 9;
    /// policy.h:57 (V(STRATEGY_PATCH, 10)): the second system strategy (EngineBase.cpp:244-247).
    const STRATEGY_PATCH: i32 = 10;

    /// AudioAttributes of `usage`, every other field at its default: content UNKNOWN, source DEFAULT, no flags, no tags.
    fn attributes(usage: AudioUsage) -> AudioAttributes {
        AudioAttributes {
            r#contentType: AudioContentType::UNKNOWN,
            r#usage: usage,
            r#source: AudioSource::DEFAULT,
            r#flags: 0,
            r#tags: Vec::new(),
        }
    }

    /// getProductStrategyFromAudioAttributes for `aa` (AudioPolicyInterfaceImpl.cpp:2455).
    fn strategy_for(aa: &AudioAttributes, fallback: bool) -> binder::Result<i32> {
        AudioPolicy.r#getProductStrategyFromAudioAttributes(Some(aa), fallback)
    }

    /// getVolumeGroupFromAudioAttributes for `aa`, with fallback on (AudioPolicyInterfaceImpl.cpp:2489).
    fn group_for(aa: &AudioAttributes) -> binder::Result<i32> {
        AudioPolicy.r#getVolumeGroupFromAudioAttributes(Some(aa), true)
    }

    /// listAudioProductStrategies, its entries flattened (AudioPolicyInterfaceImpl.cpp:2438).
    fn list_strategies() -> Vec<AudioProductStrategy> {
        AudioPolicy
            .r#listAudioProductStrategies()
            .expect("listAudioProductStrategies")
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect()
    }

    /// listAudioVolumeGroups, its entries flattened (AudioPolicyInterfaceImpl.cpp:2474).
    fn list_groups() -> Vec<AudioVolumeGroup> {
        AudioPolicy
            .r#listAudioVolumeGroups()
            .expect("listAudioVolumeGroups")
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect()
    }

    /// AUDIO_USAGE_MEDIA is listed under STRATEGY_MEDIA (EngineDefaultConfig.h:91); the exact match is EQUALS, which wins outright (ProductStrategy.cpp:164-165).
    #[test]
    fn media_attributes_follow_the_media_strategy() {
        let media = attributes(AudioUsage::MEDIA);
        assert_eq!(
            strategy_for(&media, true).expect("a strategy"),
            STRATEGY_MEDIA
        );
    }

    /// No entry lists AUDIO_USAGE_ANNOUNCEMENT (EngineDefaultConfig.h has none). Its only match is STRATEGY_MEDIA's default entry, score 0 (EngineDefaultConfig.h:99, AudioProductStrategy.cpp:73-78): fallback on takes it, fallback off gives NONE (ProductStrategy.cpp:172-173).
    #[test]
    fn unlisted_usage_has_no_strategy_without_fallback() {
        let announcement = attributes(AudioUsage::ANNOUNCEMENT);
        assert_eq!(
            strategy_for(&announcement, true).expect("a strategy"),
            STRATEGY_MEDIA
        );
        assert_eq!(
            strategy_for(&announcement, false).expect("a strategy"),
            STRATEGY_NONE
        );
    }

    /// Each strategy's volume-group name gets its own group (EngineBase.cpp:264-283: no configured group, so one is made from the MUSIC config under that name): the ring (EngineDefaultConfig.h:46) and notification (:75) usages land in two distinct groups, both in the list.
    #[test]
    fn ring_and_notification_volume_groups_are_distinct() {
        let groups: Vec<i32> = list_groups().iter().map(|g| g.r#groupId).collect();
        let ring = group_for(&attributes(AudioUsage::NOTIFICATION_TELEPHONY_RINGTONE))
            .expect("the ring's group");
        let notification =
            group_for(&attributes(AudioUsage::NOTIFICATION)).expect("the notification's group");
        assert!(groups.contains(&ring));
        assert!(groups.contains(&notification));
        assert_ne!(ring, notification);
    }

    /// gOrderedSystemStrategies (EngineDefaultConfig.h:147-163) is appended to the list (EngineBase.cpp:244-247): ids policy.h:56-57, names EngineDefaultConfig.h:148,156.
    #[test]
    fn list_includes_the_rerouting_and_patch_strategies() {
        let strategies = list_strategies();
        for (id, name) in [
            (STRATEGY_REROUTING, "STRATEGY_REROUTING"),
            (STRATEGY_PATCH, "STRATEGY_PATCH"),
        ] {
            let strategy = strategies
                .iter()
                .find(|s| s.r#id == id)
                .expect("a listed strategy");
            assert_eq!(strategy.r#name.as_deref(), Some(name));
        }
    }

    /// Every attribute a listed strategy carries names a volume group the group list holds (AudioAttributesEx.groupId, AudioVolumeGroup.groupId).
    #[test]
    fn every_listed_attribute_group_is_in_the_group_list() {
        let groups: Vec<i32> = list_groups().iter().map(|g| g.r#groupId).collect();
        assert!(!groups.is_empty());
        let mut checked = 0;
        for strategy in list_strategies() {
            for entry in strategy.r#audioAttributes.iter().flatten().flatten() {
                assert!(
                    groups.contains(&entry.r#groupId),
                    "group {} of a listed attribute is not in the group list",
                    entry.r#groupId
                );
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    /// streamToStrategy (AudioPolicyManager.h:204-208): the stream's attributes, then their strategy. MUSIC's first attributes are AUDIO_USAGE_MEDIA (EngineDefaultConfig.h:89-91).
    #[test]
    fn music_stream_follows_the_media_strategy() {
        let strategy = AudioPolicy
            .r#getStrategyForStream(AudioStreamType::MUSIC)
            .expect("a strategy");
        assert_eq!(strategy, STRATEGY_MEDIA);
    }

    /// AudioPolicyService::getStrategyForStream (AudioPolicyInterfaceImpl.cpp:1384): a stream past AUDIO_STREAM_PUBLIC_CNT (audio-base-utils.h:32, ASSISTANT + 1 = 12) has no strategy. SYS_RESERVED_REROUTING is 12 (AudioStreamType.aidl).
    #[test]
    fn stream_past_the_public_range_has_no_strategy() {
        let strategy = AudioPolicy
            .r#getStrategyForStream(AudioStreamType::SYS_RESERVED_REROUTING)
            .expect("a strategy");
        assert_eq!(strategy, STRATEGY_NONE);
    }
}
