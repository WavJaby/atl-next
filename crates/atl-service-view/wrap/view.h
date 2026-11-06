// SPDX-License-Identifier: MIT
// Adapters from the cxx bridge (src/bridge.rs) to libbinder's service-manager API. Each converts an AIBinder to libbinder's types and forwards; no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <android/binder_ibinder.h>

namespace atl::view {

using AIBinder = ::AIBinder;

// libbinder's default service manager := `view` (an android.os.IServiceManager), through AOSP's shim. Aborts (libbinder) if defaultServiceManager() already ran.
void install(AIBinder* view);

// The real servicemanager: the process's binder context object (handle 0), +1 ref; null when the driver gave none.
AIBinder* context_object();

}  // namespace atl::view
