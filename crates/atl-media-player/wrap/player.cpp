// SPDX-License-Identifier: MIT
#include "wrap/player.h"

#include <MediaPlayerService.h>
#include <android/binder_libbinder.h>
#include <binder/IInterface.h>

namespace atl::player {

AIBinder* make_media_player() {
    // what MediaPlayerService::instantiate does before addService: the view serves it instead
    android::sp<android::MediaPlayerService> service = new android::MediaPlayerService();
    // AIBinder_fromPlatformBinder takes its own +1 for the caller and holds the platform binder, so `service` may drop its reference at return
    // IInterface::asBinder is static in this AOSP (binder/IInterface.h): the member call does not exist
    return AIBinder_fromPlatformBinder(android::IInterface::asBinder(service));
}

}  // namespace atl::player
