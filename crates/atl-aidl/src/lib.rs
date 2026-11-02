// SPDX-License-Identifier: MIT
//! Rust stubs of the AIDL interfaces ATL's Rust services implement or call: the converter's AIDL crate `atl_aidl_stubs` (Cargo.toml `[package.metadata.atl] aidl`: libbinder's `libbinder_aidl` filegroup, IServiceManager and what it imports), against AOSP's libbinder_rs (`binder`).
//! Both exist only after `xtask native`, which builds this crate with feature `converter`; without it the crate is empty.
//! Layout as any aidl_interface Rust crate: `aidl::<package path>::<File>::<Type>`, and `mangled`, through which the stubs name each other.
#![forbid(unsafe_code)]

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
#[cfg(feature = "converter")]
extern crate atl_aidl_stubs;

#[cfg(feature = "converter")]
pub use atl_aidl_stubs::{aidl, mangled};
