// SPDX-License-Identifier: MIT
//! The gralloc core: the agreement between AOSP libui (Gralloc5: AIDL `IAllocator` + stable-C `IMapper` v5) and our two halves of it, the allocator service and `mapper.atl.so`.
//! Core crate: plain Rust, no unsafe, no binder, no DRM.
//! Wire-visible facts come from AOSP, each cited at its constant: `BufferUsage.aidl`, `PixelFormat.aidl`, `StandardMetadataType.aidl`, `IMapperMetadataTypes.h`, `IMapper.h`.
//! Scope (M3 step 1): one pixel format (RGBA_8888), one layer, linear memory a dma-buf holds, CPU access through the dma-buf mapping.
//! Wider formats, GPU-only usages and modifiers are rows added when a consumer first needs them.
#![forbid(unsafe_code)]

pub mod descriptor;
pub mod format;
pub mod handle;
pub mod metadata;
pub mod shared;
pub mod usage;
pub mod vintf;
