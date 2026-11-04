// SPDX-License-Identifier: MIT
//! The graphics allocator HAL service (AIDL `android.hardware.graphics.allocator.IAllocator`, instance `default`) AOSP libui's Gralloc5 allocates through.
//!
//! - `backend`: linear dma-bufs from the host's DRM device through GBM, on a thread of their own (GBM objects are not `Send`).
//! - `prime`: the dma-buf export (DRM PRIME ioctl) and the GEM handle read out of GBM, the crate's only `unsafe`.
//! - `allocator`: one allocation -- the dma-buf, the shared metadata page, the handle parameters.
//! - `service` (feature `converter`): the `IAllocator` implementation on AOSP's Rust stub; served by the bin `atl-gralloc-allocator`.
//!
//! The mapper half is atl-gralloc-mapper (`mapper.atl.so`, loaded into each client process). Buffer layout, metadata and the VINTF names are atl-gralloc's.
//! Boundary crate (GPU / dmabuf interop): `unsafe` only in `prime`.
#![deny(unsafe_code)]

pub mod allocator;
pub mod backend;
#[allow(unsafe_code)]
mod prime;
#[cfg(feature = "converter")]
pub mod service;
