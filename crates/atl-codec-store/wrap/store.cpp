// SPDX-License-Identifier: MIT
#include "wrap/store.h"

#include <android/binder_auto_utils.h>
#include <C2PlatformSupport.h>
#include <codec2/aidl/ComponentStore.h>

namespace atl::codec {

AIBinder* make_component_store() {
    // the platform store (vndk/C2Store.cpp): the soft codecs it lists are dlopened by name when the list is read
    std::shared_ptr<C2ComponentStore> platform = android::GetCodec2PlatformComponentStore();
    std::shared_ptr<::aidl::android::hardware::media::c2::IComponentStore> store =
            ::ndk::SharedRefBase::make<::aidl::android::hardware::media::c2::utils::ComponentStore>(platform);
    ::ndk::SpAIBinder binder = store->asBinder();
    // +1 for the caller; `binder` drops its own at return. The AIBinder holds the C++ object (ICInterfaceData userdata), so dropping `store` here is safe.
    AIBinder_incStrong(binder.get());
    return binder.get();
}

}  // namespace atl::codec
