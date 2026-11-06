// SPDX-License-Identifier: MIT
//! The extractor plugins of this app process (audio M1). AOSP registers them only in the media.extractor service (MediaExtractorService.cpp calls LoadExtractors at its start); the app runs with media.stagefright.extractremote=false, so its MediaExtractorFactory has no plugins until this call.
//! The plugin dirs are the platform's (`/system/lib64/extractors`, plus APEX and system_ext): the sandbox supplies them (atld `--file` rows, atld_run.rs `platform_rows`).
//! Served by [`load`] at launch, after the Codec2 store (atl-app-launcher step 1d). Boundary crate (cxx): no unsafe here.
#![cfg(feature = "converter")]
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod bridge;

/// Registers this process's extractor plugins (once; later calls return).
pub fn load() {
    bridge::ffi::load_extractors();
}
