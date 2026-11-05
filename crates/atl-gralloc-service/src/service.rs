// SPDX-License-Identifier: MIT
//! `IAllocator` on AOSP's own Rust stub (`android.hardware.graphics.allocator-V2-rust`): `allocate2`, `isSupported`, `getIMapperLibrarySuffix`.
//! The deprecated `allocate(byte[] descriptor, ...)` takes a HIDL-encoded `BufferDescriptorInfo` for mapper 4 ("As of allocator-V2 in combination with AIMAPPER_VERSION_5 this is deprecated & replaced with allocate2", IAllocator.aidl); Gralloc5 never calls it, so it is refused, counted and printed.

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::io::Write;
use std::sync::Arc;

use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::AllocationError::AllocationError;
use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::AllocationResult::AllocationResult;
use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::BufferDescriptorInfo::BufferDescriptorInfo;
use atl_aidl::graphics_allocator::aidl::android::hardware::graphics::allocator::IAllocator::{
    BnAllocator, IAllocator,
};
use atl_gralloc::descriptor::Descriptor;
use atl_gralloc::format::DRM_FORMAT_MOD_LINEAR;
use atl_gralloc::{handle, vintf};
use binder::{BinderFeatures, Interface, ParcelFileDescriptor, SpIBinder, Status};

use crate::allocator::Allocator;

fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "gralloc: {msg}");
}

/// The service-specific error libui reads (`AllocationError`, IAllocator.aidl: "service-specific error").
fn error(e: AllocationError) -> Status {
    Status::new_service_specific_error(e.0, None)
}

/// `BufferDescriptorInfo` as the core's request.
fn descriptor(info: &BufferDescriptorInfo) -> Descriptor {
    Descriptor {
        // the name is C chars in an i8/u8 array; the core treats it as bytes up to the first NUL
        name: info.name.to_vec(),
        width: info.width,
        height: info.height,
        layer_count: info.layerCount,
        format: info.format.0,
        // BufferUsage is a long-backed enum; its bits are the usage mask
        usage: info.usage.0 as u64,
        reserved_size: info.reservedSize,
        option_names: info
            .additionalOptions
            .iter()
            .map(|o| o.name.clone())
            .collect(),
        // device policy, not AIDL: LINEAR only until the GBM modifier path (G5)
        modifiers: vec![DRM_FORMAT_MOD_LINEAR],
    }
}

struct Service {
    allocator: Arc<Allocator>,
}

impl Interface for Service {}

impl IAllocator for Service {
    fn r#allocate(&self, _descriptor: &[u8], _count: i32) -> binder::Result<AllocationResult> {
        log(format_args!(
            "refused allocate(): HIDL-encoded descriptors are mapper 4's; Gralloc5 uses allocate2"
        ));
        Err(error(AllocationError::UNSUPPORTED))
    }

    fn r#allocate2(
        &self,
        info: &BufferDescriptorInfo,
        count: i32,
    ) -> binder::Result<AllocationResult> {
        let checked = match descriptor(info).check() {
            Ok(c) => c,
            Err(refusal) => {
                log(format_args!(
                    "refused allocate2({}): {refusal}",
                    descriptor(info).name_text()
                ));
                return Err(error(AllocationError::UNSUPPORTED));
            }
        };
        let count = usize::try_from(count)
            .ok()
            .filter(|&c| c >= 1)
            .ok_or_else(|| error(AllocationError::BAD_DESCRIPTOR))?;
        let mut result = AllocationResult::default();
        for _ in 0..count {
            let a = self.allocator.allocate(&checked).map_err(|e| {
                log(format_args!("allocation of {} failed: {e}", checked.name));
                error(AllocationError::NO_RESOURCES)
            })?;
            result.stride = i32::try_from(a.params.pixel_stride())
                .map_err(|_| error(AllocationError::NO_RESOURCES))?;
            result.buffers.push(Default::default());
            let h = result.buffers.last_mut().expect("just pushed");
            // handle::FD_DMABUF, handle::FD_PAGE
            h.fds = vec![
                ParcelFileDescriptor::new(a.dmabuf),
                ParcelFileDescriptor::new(a.page),
            ];
            h.ints = a.params.to_ints().to_vec();
            debug_assert_eq!(h.fds.len(), handle::NUM_FDS);
        }
        Ok(result)
    }

    fn r#isSupported(&self, info: &BufferDescriptorInfo) -> binder::Result<bool> {
        Ok(descriptor(info).check().is_ok())
    }

    fn r#getIMapperLibrarySuffix(&self) -> binder::Result<String> {
        Ok(vintf::MAPPER_SUFFIX.to_owned())
    }
}

/// The allocator service binder over `allocator`.
pub fn new_binder(allocator: Arc<Allocator>) -> SpIBinder {
    BnAllocator::new_binder(Service { allocator }, BinderFeatures::default()).as_binder()
}
