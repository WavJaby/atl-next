// SPDX-License-Identifier: MIT
//! The media extractor service an app process serves in-process (audio M1): AOSP's `MediaExtractorService`, as `media.extractor`.
//! Served by [`serve`] at launch (atl-app-launcher step 1c), after `atl_service_view::install`: MediaPlayerService::Client::setDataSource_pre asks for this name (getService) on the caller's thread before every
//! setDataSource, and a name no one serves waits 5 s there. Made at launch, not at the first lookup: the plugins load in the constructor, and MediaExtractorFactory::Create runs in-process
//! (media.stagefright.extractremote=false: the NDK extractor, GenericSource, the metadata retriever) without any lookup, so a service made lazily would leave those without plugins.
//! Constructed as instantiate() does, minus addService: the real servicemanager never sees the name. Boundary crate (cxx, raw AIBinder): `unsafe` only at the bridge call.
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

/// The name AOSP registers it under (`MediaExtractorService::getServiceName`).
pub const NAME: &str = "media.extractor";

/// Serves [`NAME`] to this process: the service made now. Refused before `atl_service_view::install`.
pub fn serve() -> Result<(), String> {
    let raw = bridge::ffi::make_media_extractor_service();
    // SAFETY: raw is the +1 AIBinder the adapter returned (or null); new_spibinder takes over that reference.
    #[allow(unsafe_code)]
    let binder = unsafe { new_spibinder(raw.cast()) }
        .ok_or_else(|| format!("{NAME}: the extractor service could not be made"))?;
    atl_service_view::serve(NAME, binder)
}
