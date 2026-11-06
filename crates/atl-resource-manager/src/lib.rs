// SPDX-License-Identifier: MIT
//! The media resource manager an app process serves in-process (audio M1): AOSP's `ResourceManagerService`, as `media.resource_manager`.
//! libstagefright's MediaCodec waits for that name (`ResourceManagerServiceProxy::init` -> `AServiceManager_waitForService`) before a codec is configured; on Android mediaserver registers it. Rung 1: AOSP's service built as-is; its codec bookkeeping and reclaim then cover this process's own codecs only, which is all one app sees.
//! The resource observer service (`media.resource_observer`, AOSP's instantiate registers it too) is not served: nothing in an app process looks it up.
//! Served by [`serve`] at launch, after `atl_service_view::install` (atl-app-launcher). Boundary crate (cxx, raw AIBinder): `unsafe` only at the bridge call.
#![cfg(feature = "converter")]
#![deny(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate atl_parcelables;
extern crate binder;
extern crate gui_aidl_types_rs;
extern crate rustutils;

#[allow(unsafe_code)]
mod bridge;

use binder::SpIBinder;
use binder::unstable_api::new_spibinder;

/// The name AOSP registers it under (`ResourceManagerService::getServiceName`).
pub const NAME: &str = "media.resource_manager";

/// Serves [`NAME`] to this process, the service made at its first lookup: its constructor asks the real servicemanager for `batterystats` (BatteryNotifier::noteResetVideo), so a process that never uses MediaCodec, or has no binder driver, never pays for it. Refused before `atl_service_view::install`.
pub fn serve() -> Result<(), String> {
    atl_service_view::serve_lazy(NAME, make)
}

/// AOSP's `ResourceManagerService::Create()`; None (lookups then see no service) when it could not be made.
fn make() -> Option<SpIBinder> {
    let raw = bridge::ffi::make_resource_manager();
    // SAFETY: raw is the +1 AIBinder the adapter returned (or null); new_spibinder takes over that reference.
    #[allow(unsafe_code)]
    unsafe {
        new_spibinder(raw.cast())
    }
}
