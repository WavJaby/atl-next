// SPDX-License-Identifier: MIT
// Adapter from the cxx bridge (src/bridge.rs) to AOSP's Codec2 store: converts types and forwards, no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <android/binder_ibinder.h>

namespace atl::codec {

using AIBinder = ::AIBinder;

// AOSP's `codec2::aidl::utils::ComponentStore` over GetCodec2PlatformComponentStore(), as a binder +1 ref; null when it could not be made.
AIBinder* make_component_store();

}  // namespace atl::codec
