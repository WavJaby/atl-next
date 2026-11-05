// SPDX-License-Identifier: MIT
//! The in-app SurfaceFlinger (M3 slice 4; plan C3 "Amended 2026-11-04"): AOSP's own compositor interfaces served from the app's process over the atl-compositor core.
//! libgui and libhwui run as-is in the app; the one thing they cannot find is a SurfaceFlinger, so the app process hosts one. Per process:
//! 1. [`start`] (the launcher calls it in app mode, before ActivityThread) builds the [`host::Composer`] around the core and a timer frame clock ([`clock`]), and the service objects: `android.ui.ISurfaceComposer` raw ([`surface_flinger`],
//!    `setTransactionState`) and `android.gui.ISurfaceComposer` / `ISurfaceComposerClient` / `IDisplayEventConnection` through the converter's stubs ([`service`]).
//! 2. it hands both to the compositor registry in the system process (atl_services::compositor_registry), which registers them as `SurfaceFlinger<suffix>` / `SurfaceFlingerAIDL<suffix>`;
//!    libgui's lookup appends the suffix it reads from the process's property area (patch to SurfaceComposerClient.cpp), so every process finds its own compositor.
//! 3. libgui's binder calls arrive as local calls (the service is in the same process): parcels are converted to the core's `WireParcel` ([`bridge`], [`wire`]), the core decides, and its callbacks to
//!    libgui's `TransactionCompletedListener` go back the same way ([`listener`]). The vsync events go down a BitTube socketpair ([`vsync`]) to the client's DisplayEventReceiver.
//!
//! The modules without binder types are plain Rust with unit tests; the rest needs feature `converter` (`xtask native`) and is covered by `xtask test composer`.
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
extern crate atl_parcelables;
#[cfg(feature = "converter")]
extern crate binder;
#[cfg(feature = "converter")]
extern crate gui_aidl_types_rs;
#[cfg(feature = "converter")]
extern crate rustutils;

pub mod clock;
pub mod input;
pub mod objects;
pub mod sink;
pub mod vsync;
pub mod wire;

#[cfg(feature = "converter")]
pub mod bridge;
#[cfg(feature = "converter")]
pub mod host;
#[cfg(feature = "converter")]
pub mod listener;
#[cfg(feature = "converter")]
pub mod service;
#[cfg(feature = "converter")]
pub mod start;
#[cfg(feature = "converter")]
pub mod surface_flinger;
#[cfg(feature = "converter")]
pub mod task_sender;
#[cfg(feature = "converter")]
pub mod ui_agent;
#[cfg(feature = "converter")]
pub mod ui_window;

#[cfg(feature = "converter")]
pub use host::ThreadHost;
#[cfg(feature = "converter")]
pub use start::start;
