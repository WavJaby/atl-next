// SPDX-License-Identifier: MIT
#include "wrap/view.h"

#include <android/binder_libbinder.h>
#include <android/os/IServiceManager.h>
#include <binder/IServiceManager.h>
#include <binder/IServiceManagerUnitTestHelper.h>
#include <binder/ProcessState.h>

namespace atl::view {

void install(AIBinder* view) {
    // getServiceManagerShimFromAidlServiceManagerForTests (binder/IServiceManagerUnitTestHelper.h): libbinder's only constructor of its IServiceManager shim
    // over an AIDL os::IServiceManager. Used because there is no non-test constructor; the alternatives were a libbinder patch or a C++ IServiceManager
    // with logic (owner, 2026-11-06). Delete when upstream exports a production constructor. A pin bump that drops or changes it fails this compile.
    android::setDefaultServiceManager(android::getServiceManagerShimFromAidlServiceManagerForTests(
            android::interface_cast<android::os::IServiceManager>(AIBinder_toPlatformBinder(view))));
}

AIBinder* context_object() {
    android::sp<android::IBinder> sm = android::ProcessState::self()->getContextObject(nullptr);
    return sm == nullptr ? nullptr : AIBinder_fromPlatformBinder(sm);
}

}  // namespace atl::view
