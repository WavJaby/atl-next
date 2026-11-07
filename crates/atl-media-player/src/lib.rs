// SPDX-License-Identifier: MIT
//! The media player service an app process serves in-process (audio P1a): AOSP's `MediaPlayerService`, as `media.player`.
//! MediaPlayer's native calls find it through `IMediaDeathNotifier::getMediaPlayerService` (waitForService), which the process's view answers; on Android mediaserver registers it.
//! Constructed as instantiate() does, minus addService: the real servicemanager never sees the name. Made at the first lookup ([`atl_service_view::serve_lazy`]), so a process that never plays media never constructs it.
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

/// The name AOSP registers it under (`MediaPlayerService::instantiate`).
pub const NAME: &str = "media.player";

/// Serves [`NAME`] to this process, the service made at its first lookup. Refused before `atl_service_view::install`.
pub fn serve() -> Result<(), String> {
    atl_service_view::serve_lazy(NAME, make)
}

/// AOSP's `MediaPlayerService` constructor as a binder; None (lookups then see no service) when it could not be made.
fn make() -> Option<SpIBinder> {
    let raw = bridge::ffi::make_media_player();
    // SAFETY: raw is the +1 AIBinder the adapter returned (or null); new_spibinder takes over that reference.
    #[allow(unsafe_code)]
    unsafe {
        new_spibinder(raw.cast())
    }
}
