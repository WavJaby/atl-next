// SPDX-License-Identifier: MIT
//! The HAL entry: `HMI` (libhardware's `HAL_MODULE_INFO_SYM_AS_STR`) and the `vk0` device libvulkan opens from it.
//! The device's three entries are the chosen ICD's own (`icd::choose`), so libvulkan calls the host driver with no frame of ours in between; the ICD is chosen once per process, at the first open.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int};
use std::sync::OnceLock;

use crate::abi::hal::{
    HWVULKAN_DEVICE_0, hw_device_t, hw_module_methods_t, hw_module_t, hwvulkan_device_t,
    hwvulkan_module_t,
};
use crate::icd::{self, Icd};

/// hardware.h `MAKE_TAG_CONSTANT(A,B,C,D)`: the four bytes, first one highest.
const fn tag(t: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*t)
}
/// hardware.h `HARDWARE_MAKE_API_VERSION(maj,min)`.
const fn api_version(maj: u16, min: u16) -> u16 {
    ((maj & 0xff) << 8) | (min & 0xff)
}
/// hardware.h `HARDWARE_MAKE_API_VERSION_2(maj,min,hdr)`.
const fn api_version_2(maj: u32, min: u32, hdr: u32) -> u32 {
    ((maj & 0xff) << 24) | ((min & 0xff) << 16) | (hdr & 0xffff)
}

/// `-ENODEV`: open's answer when no host ICD exposes a device; libvulkan then reports no Vulkan driver.
const ENODEV: c_int = 19;
/// `-EINVAL`: an id other than `vk0`.
const EINVAL: c_int = 22;

/// The module record, writable: libvulkan stores its `dlopen` handle in `dso` (driver.cpp `LoadDriver`).
#[repr(transparent)]
pub struct Module(UnsafeCell<hwvulkan_module_t>);
// SAFETY: written once by libvulkan (`dso`) before any other thread can reach the module; read-only otherwise
unsafe impl Sync for Module {}

static METHODS: hw_module_methods_t = hw_module_methods_t { open: Some(open) };

/// The symbol libvulkan looks up.
#[unsafe(no_mangle)]
pub static HMI: Module = Module(UnsafeCell::new(hwvulkan_module_t {
    common: hw_module_t {
        tag: tag(b"HWMT"),
        // hwvulkan.h HWVULKAN_MODULE_API_VERSION_0_1
        module_api_version: api_version(0, 1),
        // hardware.h HARDWARE_HAL_API_VERSION
        hal_api_version: api_version(1, 0),
        id: MODULE_ID.as_ptr(),
        name: c"ATL Vulkan driver (host ICD)".as_ptr(),
        author: c"ATL".as_ptr(),
        methods: (&raw const METHODS).cast_mut(),
        dso: std::ptr::null_mut(),
        reserved: [0; 25],
    },
}));
/// hwvulkan.h `HWVULKAN_HARDWARE_MODULE_ID`, as the C string the record points at (the test ties it to the generated constant).
const MODULE_ID: &CStr = c"vulkan";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::hal::HWVULKAN_HARDWARE_MODULE_ID;

    #[test]
    fn module_id_is_the_headers() {
        assert_eq!(MODULE_ID.to_bytes(), HWVULKAN_HARDWARE_MODULE_ID.as_bytes());
    }

    /// hardware.h's macros, evaluated by hand: 'H'=0x48 'W'=0x57 'M'=0x4d 'T'=0x54 'D'=0x44.
    #[test]
    fn tags_and_versions() {
        assert_eq!(tag(b"HWMT"), 0x4857_4d54);
        assert_eq!(tag(b"HWDT"), 0x4857_4454);
        assert_eq!(api_version(1, 0), 0x0100);
        assert_eq!(api_version_2(0, 1, 0), 0x0001_0000);
    }
}

/// The opened device: one per process, alive until exit (close is a no-op: the ICD stays loaded).
struct Device(hwvulkan_device_t);
// SAFETY: immutable after construction; its pointers are the static module and the ICD's functions
unsafe impl Sync for Device {}
// SAFETY: as Sync
unsafe impl Send for Device {}

static DEVICE: OnceLock<Option<(Device, Icd)>> = OnceLock::new();

fn device() -> Option<&'static Device> {
    DEVICE
        .get_or_init(
            || match icd::choose().and_then(|icd| Ok((make(&icd)?, icd))) {
                Ok(pair) => {
                    log(&format!(
                        "icd {} interface {} device {} type {}",
                        pair.1.manifest.display(),
                        pair.1.interface,
                        pair.1.device,
                        pair.1.device_type
                    ));
                    Some(pair)
                }
                Err(err) => {
                    log(&format!("no device: {err:#}"));
                    None
                }
            },
        )
        .as_ref()
        .map(|(d, _)| d)
}

fn make(icd: &Icd) -> anyhow::Result<Device> {
    let null = std::ptr::null_mut();
    Ok(Device(hwvulkan_device_t {
        common: hw_device_t {
            tag: tag(b"HWDT"),
            // hwvulkan.h HWVULKAN_DEVICE_API_VERSION_0_1
            version: api_version_2(0, 1, 0),
            module: HMI.0.get().cast(),
            reserved: [0; 12],
            close: Some(close),
        },
        EnumerateInstanceExtensionProperties: Some(icd::proc(
            icd.gipa,
            null,
            c"vkEnumerateInstanceExtensionProperties",
        )?),
        CreateInstance: Some(icd::proc(icd.gipa, null, c"vkCreateInstance")?),
        // SAFETY: same signature; the generated record names the instance handle by its own opaque type
        GetInstanceProcAddr: Some(unsafe {
            std::mem::transmute::<icd::GetInstanceProcAddr, _>(icd.gipa)
        }),
    }))
}

/// One line on stderr, which is the app process's log (atl-broker).
fn log(line: &str) {
    #[allow(
        clippy::print_stderr,
        reason = "one line per open; the process's stderr is its log"
    )]
    {
        eprintln!("vulkan.atl: {line}");
    }
}

/// `hw_module_methods_t::open`.
unsafe extern "C" fn open(
    _module: *const hw_module_t,
    id: *const c_char,
    out: *mut *mut hw_device_t,
) -> c_int {
    // SAFETY: libvulkan passes a NUL-terminated id (HWVULKAN_DEVICE_0)
    let id = unsafe { CStr::from_ptr(id) };
    if id.to_bytes() != HWVULKAN_DEVICE_0.as_bytes() {
        log(&format!("open: unknown device id {id:?}"));
        return -EINVAL;
    }
    let Some(dev) = device() else {
        return -ENODEV;
    };
    // SAFETY: libvulkan passes a valid out-pointer; the device is static, so the pointer stays valid
    unsafe { *out = (&raw const dev.0.common).cast_mut() };
    0
}

/// `hw_device_t::close`: nothing to free, the device and ICD live for the process.
unsafe extern "C" fn close(_dev: *mut hw_device_t) -> c_int {
    0
}
