// SPDX-License-Identifier: MIT
//! The Codec2 store an app process serves in-process (audio M1 step 2): AOSP's `codec2::aidl::utils::ComponentStore` over `C2PlatformComponentStore`, as `IComponentStore/default`.
//! The instance name is the one AOSP's manifest declares (manifest_media_c2_default.xml, hal/services/vendor.cpp:198), so the Codec2 client's declared-instance walk (client.cpp:2441-2448) finds it; atl-service-view answers that walk for it locally (no VINTF entry in this process).
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

use binder::unstable_api::new_spibinder;

/// The instance AOSP's manifest declares.
pub const INSTANCE: &str = "android.hardware.media.c2.IComponentStore/default";

/// Creates the store and serves it to this process as [`INSTANCE`]. Refused before `atl_service_view::install`.
pub fn serve() -> Result<(), String> {
    let raw = bridge::ffi::make_component_store();
    // SAFETY: raw is the +1 AIBinder the adapter returned (or null); new_spibinder takes over that reference.
    #[allow(unsafe_code)]
    let binder = unsafe { new_spibinder(raw.cast()) }
        .ok_or_else(|| format!("{INSTANCE}: the Codec2 store could not be created"))?;
    atl_service_view::serve(INSTANCE, binder)
}
