// SPDX-License-Identifier: MIT
//! The HAL entry: `HMI` (libhardware's `HAL_MODULE_INFO_SYM_AS_STR`) and the `vk0` device libvulkan opens from it.
//! The device's three entries are the chosen ICD's own (`icd::choose`), so libvulkan calls the host driver with no frame of ours in between; the ICD is chosen once per process, at the first open.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use atl_gralloc::node;

use crate::abi::hal::{
    HWVULKAN_DEVICE_0, VK_ERROR_INITIALIZATION_FAILED, VK_INCOMPLETE, VK_SUCCESS, VkInstance_T,
    VkPhysicalDeviceGroupProperties, hw_device_t, hw_module_methods_t, hw_module_t,
    hwvulkan_device_t, hwvulkan_module_t,
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

/// The opened device: one per process, alive until exit (close is a no-op: the ICD stays loaded).
struct Device(hwvulkan_device_t);
// SAFETY: immutable after construction; its pointers are the static module and the ICD's functions
unsafe impl Sync for Device {}
// SAFETY: as Sync
unsafe impl Send for Device {}

static DEVICE: OnceLock<Option<(Device, Icd)>> = OnceLock::new();

fn device() -> Option<&'static Device> {
    DEVICE
        .get_or_init(|| {
            let chosen = node::process_node(property)
                .map_err(anyhow::Error::from)
                .and_then(|n| Ok((icd::choose(&n)?, n)))
                .and_then(|(icd, n)| Ok((make(&icd)?, icd, n)));
            match chosen {
                Ok((dev, icd, n)) => {
                    let head =
                        format!("icd {} interface {}", icd.manifest.display(), icd.interface);
                    match &icd.chosen {
                        Ok((c, tier)) => log(&format!(
                            "{head} device {} type {} tier {} gralloc node {n}",
                            c.name,
                            c.kind,
                            tier.name()
                        )),
                        // instances still create; enumeration reports 0 devices
                        Err(why) => log(&format!("{head} {REFUSED}{why}")),
                    }
                    Some((dev, icd))
                }
                Err(err) => {
                    log(&format!("no device: {err:#}"));
                    None
                }
            }
        })
        .as_ref()
        .map(|(d, _)| d)
}

/// The log's mark for an ICD that loads but has no device on gralloc's node.
const REFUSED: &str = "exposes 0 devices: ";

/// The process's ICD, once open succeeded.
fn icd() -> Option<&'static Icd> {
    DEVICE.get()?.as_ref().map(|(_, i)| i)
}

/// bionic `__system_property_get` as the process's global scope has it (atl-bionic in an app process; the probe exports its own): the value, None when unset or empty.
fn property(name: &str) -> std::io::Result<Option<String>> {
    type Get = unsafe extern "C" fn(*const c_char, *mut c_char) -> c_int;
    // bionic's PROP_VALUE_MAX (sys/system_properties.h)
    const PROP_VALUE_MAX: usize = 92;
    let this = libloading::os::unix::Library::this();
    // SAFETY: the symbol, where present, is bionic's `int __system_property_get(const char*, char*)`
    let get = unsafe { this.get::<Get>(b"__system_property_get\0") }.map_err(|_| {
        std::io::Error::other(
            "__system_property_get not in the process: not an app process atld started",
        )
    })?;
    let cname = std::ffi::CString::new(name)?;
    let mut value = [0 as c_char; PROP_VALUE_MAX];
    // SAFETY: a NUL-terminated name and a PROP_VALUE_MAX buffer, bionic's contract
    let len = unsafe { get(cname.as_ptr(), value.as_mut_ptr()) };
    if len <= 0 {
        return Ok(None);
    }
    // SAFETY: bionic NUL-terminates the value within the buffer
    let v = unsafe { CStr::from_ptr(value.as_ptr()) };
    Ok(Some(v.to_string_lossy().into_owned()))
}

/// The HAL's `GetInstanceProcAddr`: the ICD's commands unchanged, except the enumerations, which expose only the chosen device.
unsafe extern "C" fn get_instance_proc_addr(
    instance: *mut VkInstance_T,
    name: *const c_char,
) -> Option<unsafe extern "C" fn()> {
    let icd = icd()?;
    // SAFETY: the caller passes a NUL-terminated command name
    let wanted = unsafe { CStr::from_ptr(name) }.to_bytes();
    match wanted {
        b"vkGetInstanceProcAddr" => {
            // SAFETY: the command of that name, returned as PFN_vkVoidFunction as the API does
            Some(unsafe {
                std::mem::transmute::<GipaField, unsafe extern "C" fn()>(get_instance_proc_addr)
            })
        }
        b"vkEnumeratePhysicalDevices" => {
            // SAFETY: as above
            Some(unsafe {
                std::mem::transmute::<EnumerateFn, unsafe extern "C" fn()>(
                    enumerate_physical_devices,
                )
            })
        }
        b"vkEnumeratePhysicalDeviceGroups" | b"vkEnumeratePhysicalDeviceGroupsKHR" => {
            // SAFETY: as above
            Some(unsafe {
                std::mem::transmute::<GroupsFn, unsafe extern "C" fn()>(
                    enumerate_physical_device_groups,
                )
            })
        }
        // SAFETY: the ICD's own lookup, called as the interface defines
        _ => unsafe { (icd.gipa)(instance.cast(), name) },
    }
}

type H = *mut c_void;
type EnumerateFn = unsafe extern "C" fn(H, *mut u32, *mut H) -> i32;
type GroupsFn = unsafe extern "C" fn(H, *mut u32, *mut VkPhysicalDeviceGroupProperties) -> i32;

/// The chosen device among `instance`'s, None when the ICD exposes none (refusal) or it is gone.
fn chosen_handle(icd: &Icd, instance: H) -> Result<Option<H>, i32> {
    let Ok((want, _)) = &icd.chosen else {
        return Ok(None);
    };
    // SAFETY: a live instance of this ICD (the app's, created through it)
    let all = unsafe { icd::physical_devices(icd.gipa, instance) }
        .map_err(|_| VK_ERROR_INITIALIZATION_FAILED)?;
    for p in all {
        // SAFETY: a handle the instance just enumerated
        let (key, _, _) = unsafe { icd::properties(icd.gipa, instance, p) }
            .map_err(|_| VK_ERROR_INITIALIZATION_FAILED)?;
        if key == want.key {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// `vkEnumeratePhysicalDevices` over [`chosen_handle`]: 0 or 1 devices, the spec's count/INCOMPLETE protocol.
unsafe extern "C" fn enumerate_physical_devices(instance: H, count: *mut u32, out: *mut H) -> i32 {
    let Some(icd) = icd() else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mine = match chosen_handle(icd, instance) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let n = u32::from(mine.is_some());
    // SAFETY: the caller's count pointer, valid per the spec
    let count = unsafe { &mut *count };
    if out.is_null() {
        *count = n;
        return VK_SUCCESS;
    }
    match mine {
        Some(p) if *count >= 1 => {
            // SAFETY: room for *count >= 1 handles
            unsafe { *out = p };
            *count = 1;
            VK_SUCCESS
        }
        Some(_) => VK_INCOMPLETE,
        None => {
            *count = 0;
            VK_SUCCESS
        }
    }
}

/// `vkEnumeratePhysicalDeviceGroups`: one group holding only the chosen device, or none.
unsafe extern "C" fn enumerate_physical_device_groups(
    instance: H,
    count: *mut u32,
    out: *mut VkPhysicalDeviceGroupProperties,
) -> i32 {
    let Some(icd) = icd() else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let mine = match chosen_handle(icd, instance) {
        Ok(m) => m,
        Err(r) => return r,
    };
    // SAFETY: the caller's count pointer, valid per the spec
    let count = unsafe { &mut *count };
    if out.is_null() {
        *count = u32::from(mine.is_some());
        return VK_SUCCESS;
    }
    match mine {
        Some(p) if *count >= 1 => {
            // SAFETY: room for *count >= 1 records; the caller set sType/pNext, which stay
            let g = unsafe { &mut *out };
            g.physicalDeviceCount = 1;
            g.physicalDevices = [std::ptr::null_mut(); _];
            g.physicalDevices[0] = p.cast();
            g.subsetAllocation = 0;
            *count = 1;
            VK_SUCCESS
        }
        Some(_) => VK_INCOMPLETE,
        None => {
            *count = 0;
            VK_SUCCESS
        }
    }
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
        // SAFETY: global commands (null instance), typed as the record's fields are (the header's PFN types)
        EnumerateInstanceExtensionProperties: Some(unsafe {
            icd::proc(icd.gipa, null, c"vkEnumerateInstanceExtensionProperties")
        }?),
        // SAFETY: as above
        CreateInstance: Some(unsafe { icd::proc(icd.gipa, null, c"vkCreateInstance") }?),
        GetInstanceProcAddr: Some(get_instance_proc_addr),
    }))
}

/// `hwvulkan_device_t::GetInstanceProcAddr` as generated.
type GipaField =
    unsafe extern "C" fn(*mut VkInstance_T, *const c_char) -> Option<unsafe extern "C" fn()>;

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
