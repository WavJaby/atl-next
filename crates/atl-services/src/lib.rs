// SPDX-License-Identifier: MIT
//! ATL's M2 system services in Rust (owner decision 2026-11-03: AOSP's Java system_server measured too costly; more services and methods later, per service).
//! Each implements a converter-built AIDL stub (atl-aidl) and only the methods M2 needs; the rest keep the stub's default body (aidl `--rust_unimplemented_hook`), which lands in
//! [`counters::unimplemented`]: counted per (interface, method) and answered UNKNOWN_TRANSACTION, what Java's Binder.onTransact gives an unknown code.
//! `@EnforcePermission` methods ask [`permission::enforce`] first (fail-closed). Policy modules are plain Rust; the binder modules need feature `converter`.
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
extern crate atl_parcelables;
#[cfg(feature = "converter")]
extern crate binder;

pub mod counters;
pub mod packages;
pub mod permission;
pub mod shared_memory;

#[cfg(feature = "converter")]
pub mod activity;
#[cfg(feature = "converter")]
pub mod display;
#[cfg(feature = "converter")]
pub mod package;
#[cfg(feature = "converter")]
pub mod settings;
#[cfg(feature = "converter")]
mod token;
