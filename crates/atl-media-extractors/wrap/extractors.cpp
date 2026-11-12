// SPDX-License-Identifier: MIT
#include "wrap/extractors.h"

#include <MediaExtractorService.h>
#include <android/binder_libbinder.h>
#include <binder/IInterface.h>

namespace atl::extractors {

AIBinder* make_media_extractor_service() {
    // what MediaExtractorService::instantiate does before addService: the view serves it instead
    android::sp<android::MediaExtractorService> service = new android::MediaExtractorService();
    // AIBinder_fromPlatformBinder takes its own +1 for the caller and holds the platform binder, so `service` may drop its reference at return
    return AIBinder_fromPlatformBinder(android::IInterface::asBinder(service));
}

}  // namespace atl::extractors
