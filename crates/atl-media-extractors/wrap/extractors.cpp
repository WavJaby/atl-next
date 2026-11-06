// SPDX-License-Identifier: MIT
#include "wrap/extractors.h"

#include <media/stagefright/MediaExtractorFactory.h>

namespace atl::extractors {

void load_extractors() {
    android::MediaExtractorFactory::LoadExtractors();
}

}  // namespace atl::extractors
