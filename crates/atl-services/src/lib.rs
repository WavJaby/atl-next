// SPDX-License-Identifier: MIT
//! ATL's M2 system services in Rust (owner decision 2026-11-03: AOSP's Java system_server on ART would add resident memory, startup time and a large build closure; not measured; more services and methods later, per service).
//! Each implements a converter-built AIDL stub (atl-aidl) and only the methods M2 needs; the rest keep the stub's default body (aidl `--rust_unimplemented_hook`), which lands in
//! [`counters::unimplemented`]: counted per (interface, method) and answered UNKNOWN_TRANSACTION, what Java's Binder.onTransact gives an unknown code.
//! `@EnforcePermission` methods ask [`permission::enforce`] first (fail-closed). Policy modules are plain Rust; the binder modules need feature `converter`.
#![forbid(unsafe_code)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
#[cfg(feature = "converter")]
extern crate atl_parcelables;
#[cfg(feature = "converter")]
extern crate binder;

pub mod activity_stack;
pub mod activity_start;
pub mod alarm_table;
pub mod broadcast;
pub mod compositor_registry;
pub mod config_diff;
pub mod counters;
pub mod display_model;
pub mod focus;
pub mod ime_bind;
pub mod input_devices;
pub mod manifest_flags;
pub mod native_libs;
pub mod observer_rules;
pub mod packages;
pub mod pending_intent_table;
pub mod permission;
pub mod players;
pub mod process_control;
pub mod sensor;
pub mod shared_memory;
pub mod size_buckets;
pub mod tasks;
pub mod window_layout;

#[cfg(feature = "converter")]
pub mod activity;
#[cfg(feature = "converter")]
pub mod activity_task;
#[cfg(feature = "converter")]
pub mod alarm;
#[cfg(feature = "converter")]
pub mod audio;
#[cfg(feature = "converter")]
mod broadcast_queue;
#[cfg(feature = "converter")]
pub mod clipboard;
#[cfg(feature = "converter")]
pub mod connectivity;
#[cfg(feature = "converter")]
pub mod content;
#[cfg(feature = "converter")]
pub mod display;
#[cfg(feature = "converter")]
pub mod input;
#[cfg(feature = "converter")]
pub mod input_method;
#[cfg(feature = "converter")]
pub mod notification;
#[cfg(feature = "converter")]
pub mod package;
#[cfg(feature = "converter")]
pub mod pending_intent;
#[cfg(feature = "converter")]
pub mod permission_manager;
#[cfg(feature = "converter")]
pub mod process_info;
#[cfg(feature = "converter")]
pub mod registry_service;
#[cfg(feature = "converter")]
pub mod sensor_wire;
#[cfg(feature = "converter")]
pub mod settings;
#[cfg(feature = "converter")]
pub mod storage;
#[cfg(feature = "converter")]
mod token;
#[cfg(feature = "converter")]
pub mod ui_agent;
#[cfg(feature = "converter")]
pub mod user;
#[cfg(feature = "converter")]
pub mod window;
