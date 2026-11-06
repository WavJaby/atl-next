// SPDX-License-Identifier: MIT
//! libgui's Parcel formats in Rust, shared the way AOSP shares libgui between SurfaceFlinger and system_server (WMS reads a whole `SurfaceControl.Transaction`: `nativeReadTransactionFromParcel`,
//! android_view_SurfaceControl.cpp:2064-2071 -> `SurfaceComposerClient::Transaction::createFromParcel`, SurfaceComposerClient.cpp:856).
//! * [`parcel`]: a Parcel as bytes + object table, binders and fds kept as objects.
//! * [`state`], [`transaction`]: `layer_state_t` and friends, `TransactionState`, and the client `Transaction` (state + listener callbacks) a Java `SurfaceControl.Transaction` parcels as.
//! * [`wire`]: libbinder flat objects in Parcel bytes: finding them, naming a binder by a key.
#![forbid(unsafe_code)]

pub mod parcel;
pub mod state;
pub mod transaction;
pub mod wire;
