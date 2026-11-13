// SPDX-License-Identifier: MIT
//! The wire of the hand-written binder interfaces the system services (atl-services) and their in-ATL clients share: both sides' protocol, no server state.
//! [`ui_agent`]: the app's UI agent the window service calls; [`sensor`] + [`sensor_wire`]: libsensor's event record and binder interfaces, with the data-injection client.
//! An app process (atl-composer, atl-audioflinger) depends on this crate, never on atl-services, so it does not link the services' platform-host closure.
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
extern crate atl_diag;
#[cfg(feature = "converter")]
extern crate atl_parcelables;
#[cfg(feature = "converter")]
extern crate binder;

pub mod sensor;
#[cfg(feature = "converter")]
pub mod sensor_wire;
#[cfg(feature = "converter")]
pub mod ui_agent;
