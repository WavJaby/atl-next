// SPDX-License-Identifier: MIT
// Adapters from the cxx bridge (src/capture.rs) to libgui's ScreenCaptureResults. Each converts scalars to AOSP's types and forwards calls to AOSP's own parcel code; no logic (project CLAUDE.md, C++ exception).
#pragma once

#include <cstdint>

#include <android/binder_parcel.h>

namespace atl::capture {

using AParcel = ::AParcel;

// ScreenCaptureResults::writeToParcel of a result with these fields (the others at the header's defaults) into the parcel's data at its position. fence_fd >= 0 is a valid Fence that takes ownership of the fd; otherwise fence_status is the fence status. Returns the status_t.
int32_t write_result(AParcel* parcel, int32_t fence_fd, int32_t fence_status, bool secure, bool hdr, uint32_t dataspace, float hdr_sdr_ratio);

// ScreenCaptureResults::readFromParcel of `input` at its position, then writeToParcel of that result into `out`. Returns the first failing status_t.
int32_t reserialize(const AParcel* input, AParcel* out);

}  // namespace atl::capture
