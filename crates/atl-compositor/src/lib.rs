// SPDX-License-Identifier: MIT
//! The in-app compositor core (M3 slice 3; plan C3 "Amended 2026-11-04"): the layer tree behind AOSP's SurfaceFlinger-facing wire formats, and the frame hand-off to the platform layer.
//! * The Parcel, `layer_state_t` and `TransactionState` codecs are atl-libgui-parcel's (shared with the window service, as libgui is with system_server).
//! * [`callbacks`], [`event`]: codecs translated from libgui (`ITransactionCompletedListener`, `DisplayEventReceiver::Event`).
//! * [`model`]: layers, buffer cache, transaction queues, release / completion callbacks; [`scene`]: the z-ordered quads one tick yields.
//!
//! Sync and message driven (no runtime, no threads), so the app's GLib loop and plain tests drive it alike. The binder front-ends that register the services and convert real Parcels are M3 slice 4.
#![forbid(unsafe_code)]

pub mod callbacks;
pub mod event;
pub mod model;
pub mod scene;
