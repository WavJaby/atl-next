// SPDX-License-Identifier: MIT
// Adapter from the cxx bridge (src/bridge.rs) to AOSP's ResourceManagerService: converts types and forwards, no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <android/binder_ibinder.h>

namespace atl::media {

using AIBinder = ::AIBinder;

// AOSP's `ResourceManagerService::Create()` as a binder +1 ref; null when it could not be made.
AIBinder* make_resource_manager();

}  // namespace atl::media
