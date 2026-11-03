// SPDX-License-Identifier: MIT
//! Rust stubs of the AIDL interfaces ATL's Rust services implement or call: the converter's AIDL crate `atl_aidl_stubs` (Cargo.toml `[package.metadata.atl.aidl]`: IServiceManager, the M2 framework interfaces, the M3 window path and libgui's `android.gui`, with their import closure), against AOSP's libbinder_rs (`binder`); Java-only parcelables are atl-parcelables' types.
//! The stable HAL interface `android.hardware.graphics.allocator` is AOSP's own `-V2-rust` backend, re-exported as [`graphics_allocator`].
//! Both exist only after `xtask native`, which builds this crate with feature `converter`; without it the crate is empty.
//! Layout as any aidl_interface Rust crate: `aidl::<package path>::<File>::<Type>`, and `mangled`, through which the stubs name each other.
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
pub extern crate android_hardware_graphics_allocator as graphics_allocator;
#[cfg(feature = "converter")]
extern crate atl_aidl_stubs;

#[cfg(feature = "converter")]
pub use atl_aidl_stubs::{aidl, mangled};
