// SPDX-License-Identifier: MIT
//! The five calls of wrap/engine.h: forwards to the engine, AOSP's AIDL conversions, and the parcel encoding of the answers. Status is AOSP's status_t, unchanged.
//! The service rules (the public stream range, NONE with the fallback) are policy.rs's, at the overrides that AudioPolicyInterfaceImpl.cpp implements.
#include "wrap/engine.h"

#include <EngineInterface.h>
#include <binder/Parcel.h>
#include <media/AidlConversion.h>
#include <media/AudioProductStrategy.h>
#include <media/AudioVolumeGroup.h>
#include <utils/Errors.h>

namespace atl::audio {

namespace {

/// The parcel's bytes into `out`.
void copy_out(const android::Parcel& parcel, rust::Vec<uint8_t>& out) {
    const uint8_t* data = parcel.data();
    out.clear();
    out.reserve(parcel.dataSize());
    for (size_t i = 0; i < parcel.dataSize(); ++i) {
        out.push_back(data[i]);
    }
}

/// AudioAttributes from its parcelable bytes (marker, then the sized body), then AOSP's conversion (AudioPolicyInterfaceImpl.cpp:2458). A parcel that does not read gives its status.
android::status_t attributes_from(rust::Slice<const uint8_t> bytes, audio_attributes_t& out) {
    android::Parcel parcel;
    if (android::status_t status = parcel.setData(bytes.data(), bytes.size()); status != android::NO_ERROR) {
        return status;
    }
    parcel.setDataPosition(0);
    android::media::audio::common::AudioAttributes aidl;
    if (android::status_t status = parcel.readParcelable(&aidl); status != android::NO_ERROR) {
        return status;
    }
    auto legacy = android::aidl2legacy_AudioAttributes_audio_attributes_t(aidl);
    if (!legacy.ok()) {
        return legacy.error();
    }
    out = legacy.value();
    return android::NO_ERROR;
}

}  // namespace

PolicyEngine::PolicyEngine()
    : engine(android::createEngineInstance(), android::destroyEngineInstance),
      load_status(engine ? engine->loadFromXmlConfigWithFallback("") : android::NO_INIT) {}

PolicyEngine::~PolicyEngine() = default;

std::unique_ptr<PolicyEngine> new_policy_engine() {
    return std::make_unique<PolicyEngine>();
}

int32_t policy_engine_status(const PolicyEngine& policy) {
    return policy.load_status;
}

int32_t list_product_strategies(const PolicyEngine& policy, rust::Vec<uint8_t>& out) {
    // AudioPolicyService::listAudioProductStrategies (AudioPolicyInterfaceImpl.cpp:2438) over AudioPolicyManager::listAudioProductStrategies (AudioPolicyManager.h:380)
    android::AudioProductStrategyVector strategies;
    if (android::status_t status = policy.engine->listAudioProductStrategies(strategies);
        status != android::NO_ERROR) {
        return status;
    }
    android::Parcel parcel;
    if (android::status_t status = parcel.writeInt32(static_cast<int32_t>(strategies.size()));
        status != android::NO_ERROR) {
        return status;
    }
    for (const auto& strategy : strategies) {
        auto aidl = android::legacy2aidl_AudioProductStrategy(strategy);
        if (!aidl.ok()) {
            return aidl.error();
        }
        if (android::status_t status = parcel.writeParcelable(aidl.value()); status != android::NO_ERROR) {
            return status;
        }
    }
    copy_out(parcel, out);
    return android::NO_ERROR;
}

int32_t product_strategy_for_attributes(const PolicyEngine& policy, rust::Slice<const uint8_t> attributes,
                                        bool fallback_on_default, int32_t& out) {
    // AudioPolicyManager::getProductStrategyFromAudioAttributes (AudioPolicyManager.h:385) forwarded. Its NONE-with-fallback BAD_VALUE is policy.rs's (AudioPolicyManager.h:389-391).
    audio_attributes_t aa{};
    if (android::status_t status = attributes_from(attributes, aa); status != android::NO_ERROR) {
        return status;
    }
    out = static_cast<int32_t>(policy.engine->getProductStrategyForAttributes(aa, fallback_on_default));
    return android::NO_ERROR;
}

int32_t product_strategy_for_stream(const PolicyEngine& policy, int32_t stream, int32_t& out) {
    // AudioPolicyManager::getStrategyForStream = streamToStrategy (AudioPolicyManager.h:204-208): the stream's attributes, then their strategy with the fallback on.
    // `stream` is public: policy.rs answers NONE past the range (AudioPolicyInterfaceImpl.cpp:1384). EngineInterface has no stream-to-strategy call: EngineBase::getProductStrategyForStream (EngineBase.h:129) is off the interface, and a different lookup (first strategy listing the stream, else the default; ProductStrategy.cpp:214-223), so the two calls stay here.
    const audio_attributes_t aa =
            policy.engine->getAttributesForStreamType(static_cast<audio_stream_type_t>(stream));
    out = static_cast<int32_t>(policy.engine->getProductStrategyForAttributes(aa, true));
    return android::NO_ERROR;
}

int32_t list_volume_groups(const PolicyEngine& policy, rust::Vec<uint8_t>& out) {
    // AudioPolicyService::listAudioVolumeGroups (AudioPolicyInterfaceImpl.cpp:2474) over AudioPolicyManager::listAudioVolumeGroups (AudioPolicyManager.h:394)
    android::AudioVolumeGroupVector groups;
    if (android::status_t status = policy.engine->listAudioVolumeGroups(groups); status != android::NO_ERROR) {
        return status;
    }
    android::Parcel parcel;
    if (android::status_t status = parcel.writeInt32(static_cast<int32_t>(groups.size()));
        status != android::NO_ERROR) {
        return status;
    }
    for (const auto& group : groups) {
        auto aidl = android::legacy2aidl_AudioVolumeGroup(group);
        if (!aidl.ok()) {
            return aidl.error();
        }
        if (android::status_t status = parcel.writeParcelable(aidl.value()); status != android::NO_ERROR) {
            return status;
        }
    }
    copy_out(parcel, out);
    return android::NO_ERROR;
}

int32_t volume_group_for_attributes(const PolicyEngine& policy, rust::Slice<const uint8_t> attributes,
                                    bool fallback_on_default, int32_t& out) {
    // AudioPolicyManager::getVolumeGroupFromAudioAttributes (AudioPolicyManager.h:399) forwarded. Its VOLUME_GROUP_NONE-with-fallback BAD_VALUE is policy.rs's (AudioPolicyManager.h:401-405).
    audio_attributes_t aa{};
    if (android::status_t status = attributes_from(attributes, aa); status != android::NO_ERROR) {
        return status;
    }
    out = static_cast<int32_t>(policy.engine->getVolumeGroupForAttributes(aa, fallback_on_default));
    return android::NO_ERROR;
}

}  // namespace atl::audio
