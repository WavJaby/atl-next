// SPDX-License-Identifier: MIT
//! The gralloc core: the agreement between AOSP libui (Gralloc5: AIDL `IAllocator` + stable-C `IMapper` v5) and our two halves of it, the allocator service and `mapper.atl.so`.
//! Core crate: plain Rust, no unsafe, no binder, no DRM.
//! Wire-visible facts come from AOSP, each cited at its constant: `BufferUsage.aidl`, `PixelFormat.aidl`, `StandardMetadataType.aidl`, `IMapperMetadataTypes.h`, `IMapper.h`.
//! Scope (M3 step 1): one pixel format (RGBA_8888), one layer, linear memory a dma-buf holds, CPU access through the dma-buf mapping.
//! Wider formats, GPU-only usages and modifiers are rows added when a consumer first needs them.
//! Own allocator + mapper (rung 6) rather than AOSP's external/minigbm, which has the same AIDL / stable-C interfaces: its device layer selects per-DRM-driver backends at compile time from one Soong variable (no generic desktop value, no AMD, no vgem), so fitting it needs patches beyond a few lines plus a vendor libdrm pin, and the vgem part would be a WSL-only product path. Mesa GBM is the native Linux allocator for every desktop driver (owner-delegated decision 2026-11-04).
#![forbid(unsafe_code)]

pub mod descriptor;
pub mod format;
pub mod handle;
pub mod layout;
pub mod metadata;
pub mod shared;
pub mod usage;
pub mod vintf;
