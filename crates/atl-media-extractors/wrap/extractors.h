// SPDX-License-Identifier: MIT
// Adapter from the cxx bridge (src/bridge.rs) to AOSP's MediaExtractorService: converts types and forwards, no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <android/binder_ibinder.h>

namespace atl::extractors {

using AIBinder = ::AIBinder;

// AOSP's `MediaExtractorService` constructed as instantiate() does, minus its addService, as a binder +1 ref; null when it could not be made.
// The constructor calls MediaExtractorFactory::LoadExtractors, so the plugins are loaded when this returns.
AIBinder* make_media_extractor_service();

}  // namespace atl::extractors
