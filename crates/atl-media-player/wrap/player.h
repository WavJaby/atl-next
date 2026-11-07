// SPDX-License-Identifier: MIT
// Adapter from the cxx bridge (src/bridge.rs) to AOSP's MediaPlayerService: converts types and forwards, no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <android/binder_ibinder.h>

namespace atl::player {

using AIBinder = ::AIBinder;

// AOSP's `MediaPlayerService` constructed as instantiate() does, minus its addService, as a binder +1 ref; null when it could not be made.
AIBinder* make_media_player();

}  // namespace atl::player
