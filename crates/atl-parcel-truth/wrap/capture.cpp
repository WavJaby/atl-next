// SPDX-License-Identifier: MIT
#include "wrap/capture.h"

#include <android/binder_libbinder.h>
#include <gui/ScreenCaptureResults.h>

namespace atl::capture {

int32_t write_result(AParcel* parcel, int32_t fence_fd, int32_t fence_status, bool secure, bool hdr, uint32_t dataspace, float hdr_sdr_ratio) {
    android::gui::ScreenCaptureResults results;
    if (fence_fd >= 0) {
        results.fenceResult = android::sp<android::Fence>::make(fence_fd);
    } else if (fence_status != android::NO_ERROR) {
        results.fenceResult = android::FenceResult(android::base::unexpected(static_cast<android::status_t>(fence_status)));
    }
    results.capturedSecureLayers = secure;
    results.capturedHdrLayers = hdr;
    results.capturedDataspace = static_cast<android::ui::Dataspace>(dataspace);
    results.hdrSdrRatio = hdr_sdr_ratio;
    return results.writeToParcel(AParcel_viewPlatformParcel(parcel));
}

int32_t reserialize(const AParcel* input, AParcel* out) {
    android::gui::ScreenCaptureResults results;
    const android::status_t status = results.readFromParcel(AParcel_viewPlatformParcel(input));
    if (status != android::NO_ERROR) {
        return status;
    }
    return results.writeToParcel(AParcel_viewPlatformParcel(out));
}

}  // namespace atl::capture
