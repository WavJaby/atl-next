// SPDX-License-Identifier: MIT
//! AOSP's audio policy engine (libaudiopolicyenginedefault, EngineInterface) with its default config, behind the cxx bridge in src/bridge.rs.
//! The five IAudioPolicyService queries policy.rs serves: each function is the AOSP service or AudioPolicyManager body for that call (engine.cpp names them); the engine decides the answer. No table, no matcher.
//! Errors are AOSP's status_t, unchanged. Attributes come in as the AudioAttributes parcelable's bytes; a list goes out as an AIDL parcel: int32 count, then each parcelable (Parcel::writeParcelable).
#pragma once

#include <cstdint>
#include <memory>

#include "rust/cxx.h"

namespace android {
class EngineInterface;
}

namespace atl::audio {

/// EngineInterface from createEngineInstance() (EngineInstance.cpp), loaded with its default config: no XML, so EngineBase's built-in tables (EngineBase.cpp:128-138). Freed by the library's destroyEngineInstance: its destructor is protected (EngineInterface.h).
struct PolicyEngine {
    PolicyEngine();
    ~PolicyEngine();
    std::unique_ptr<android::EngineInterface, void (*)(android::EngineInterface*)> engine;
    /// loadFromXmlConfigWithFallback's status: NO_ERROR when the engine is usable.
    int32_t load_status;
};

std::unique_ptr<PolicyEngine> new_policy_engine();
int32_t policy_engine_status(const PolicyEngine& policy);
/// listAudioProductStrategies: the status; `out` holds the parcel of the strategies on NO_ERROR.
int32_t list_product_strategies(const PolicyEngine& policy, rust::Vec<uint8_t>& out);
/// getProductStrategyFromAudioAttributes: `out` holds the strategy id on NO_ERROR. `attributes` is the AudioAttributes parcelable's bytes.
int32_t product_strategy_for_attributes(const PolicyEngine& policy, rust::Slice<const uint8_t> attributes,
                                        bool fallback_on_default, int32_t& out);
/// getStrategyForStream: `out` holds the strategy id on NO_ERROR.
int32_t product_strategy_for_stream(const PolicyEngine& policy, int32_t stream, int32_t& out);
/// listAudioVolumeGroups: the status; `out` holds the parcel of the groups on NO_ERROR.
int32_t list_volume_groups(const PolicyEngine& policy, rust::Vec<uint8_t>& out);
/// getVolumeGroupFromAudioAttributes: `out` holds the group id on NO_ERROR.
int32_t volume_group_for_attributes(const PolicyEngine& policy, rust::Slice<const uint8_t> attributes,
                                    bool fallback_on_default, int32_t& out);

}  // namespace atl::audio
