// SPDX-License-Identifier: MIT
#include "wrap/rms.h"

#include <android/binder_auto_utils.h>
#include <ResourceManagerService.h>

namespace atl::media {

AIBinder* make_resource_manager() {
    // what ResourceManagerService::instantiate does before AServiceManager_addServiceWithFlags: the view serves it instead
    std::shared_ptr<::android::ResourceManagerService> service = ::android::ResourceManagerService::Create();
    if (service == nullptr) return nullptr;
    ::ndk::SpAIBinder binder = service->asBinder();
    // +1 for the caller; `binder` drops its own at return. The AIBinder holds the C++ object (ICInterfaceData userdata), so dropping `service` here is safe.
    AIBinder_incStrong(binder.get());
    return binder.get();
}

}  // namespace atl::media
