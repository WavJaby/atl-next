// SPDX-License-Identifier: MIT
//! The system services' plain types and policy: package records and manifest parsing results, per-app GL driver, display model, the compositor registry names, input devices, the installer's output frames, uid identity and permission rules, the unimplemented-method counters. No binder, no build script, no workspace runtime crate: shared by atl-services, atld and xtask, which writes and reads the same records.
#![forbid(unsafe_code)]

pub mod compositor_registry;
pub mod counters;
pub mod display_model;
pub mod gl_driver;
pub mod input_devices;
pub mod installer_output;
pub mod manifest_flags;
pub mod native_libs;
pub mod packages;
pub mod permission;
pub mod size_buckets;
