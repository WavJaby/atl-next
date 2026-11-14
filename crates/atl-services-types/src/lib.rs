// SPDX-License-Identifier: MIT
//! The system services' plain types and policy: platform permission definitions and shared libraries, per-app GL driver, display model, the compositor registry names, input devices, the installer's output frames, the logcat record and logd's write socket, SystemConfig's system features, uid identity and permission rules. No binder, no build script, no workspace runtime crate: shared by atl-services, atld and xtask.
#![forbid(unsafe_code)]

pub mod compositor_registry;
pub mod display_model;
pub mod gl_driver;
pub mod input_devices;
pub mod installer_output;
pub mod logcat;
pub mod native_libs;
pub mod packages;
pub mod permission;
pub mod size_buckets;
pub mod system_config;
pub mod system_packages;
