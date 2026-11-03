// SPDX-License-Identifier: MIT
//! The in-app compositor core (M3 slice 3; plan C3 "Amended 2026-11-04"): AOSP's SurfaceFlinger-facing wire formats in Rust, the layer tree behind them, and the frame hand-off to the platform layer.
//! * [`parcel`]: a Parcel as bytes + object table, binders and fds kept as objects.
//! * [`state`], [`transaction`], [`callbacks`], [`event`]: codecs translated from libgui (`layer_state_t`, `TransactionState`, `ITransactionCompletedListener`, `DisplayEventReceiver::Event`).
//! * [`model`]: layers, buffer cache, transaction queues, release / completion callbacks; [`scene`]: the z-ordered quads one tick yields.
//!
//! Sync and message driven (no runtime, no threads), so the app's GLib loop and plain tests drive it alike. The binder front-ends that register the services and convert real Parcels are M3 slice 4.
#![forbid(unsafe_code)]

pub mod callbacks;
pub mod event;
pub mod model;
pub mod parcel;
pub mod scene;
pub mod state;
pub mod transaction;
