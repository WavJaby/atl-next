// SPDX-License-Identifier: MIT
// Forwarder from the cxx bridge (src/bridge.rs) to AOSP's MediaExtractorFactory: one call, no logic (project CLAUDE.md, C++ exception).
#pragma once

namespace atl::extractors {

// MediaExtractorFactory::LoadExtractors (media/libstagefright): the media.extractor service's start-up call, made here by the app process.
void load_extractors();

}  // namespace atl::extractors
