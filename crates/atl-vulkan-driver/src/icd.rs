// SPDX-License-Identifier: MIT
//! The host Vulkan ICD this process drives: found the way the Khronos loader finds ICDs on Linux (JSON manifests in `vulkan/icd.d` under the XDG config and data dirs and `/etc`, Khronos loader `LoaderDriverInterface.md` "Driver Discovery on Linux"), loaded `RTLD_NOW | RTLD_LOCAL` so its symbols never reach the app's scope.
//! Chosen: the first manifest, in search order then file name, whose ICD creates an instance and enumerates at least one physical device. The XDG variables are the discovery spec's own inputs (the desktop sets them for every Vulkan program), not a switch of ours.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
use serde::Deserialize;

use atl_gralloc::node::ProcessNode;

use crate::abi::hal::{
    VK_API_VERSION_1_1, VK_INCOMPLETE, VK_PHYSICAL_DEVICE_TYPE_CPU,
    VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU, VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU,
    VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU, VK_STRUCTURE_TYPE_APPLICATION_INFO,
    VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT,
    VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, VK_SUCCESS, VkApplicationInfo,
    VkExtensionProperties, VkInstanceCreateInfo, VkPhysicalDeviceDrmPropertiesEXT,
    VkPhysicalDeviceProperties, VkPhysicalDeviceProperties2,
};
use crate::device::{self, Candidate, Key, Tier};

/// `VK_EXT_PHYSICAL_DEVICE_DRM_EXTENSION_NAME` (vulkan_core.h).
const DRM_EXT: &CStr = c"VK_EXT_physical_device_drm";

/// `vk_icdGetInstanceProcAddr` / `vkGetInstanceProcAddr`.
pub type GetInstanceProcAddr =
    unsafe extern "C" fn(*mut c_void, *const c_char) -> Option<unsafe extern "C" fn()>;
type Negotiate = unsafe extern "C" fn(*mut u32) -> i32;
type CreateInstance =
    unsafe extern "C" fn(*const VkInstanceCreateInfo, *const c_void, *mut *mut c_void) -> i32;
type DestroyInstance = unsafe extern "C" fn(*mut c_void, *const c_void);
type EnumeratePhysicalDevices =
    unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type GetPhysicalDeviceProperties =
    unsafe extern "C" fn(*mut c_void, *mut VkPhysicalDeviceProperties);
type GetPhysicalDeviceProperties2 =
    unsafe extern "C" fn(*mut c_void, *mut VkPhysicalDeviceProperties2);
type EnumerateDeviceExtensionProperties =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut u32, *mut VkExtensionProperties) -> i32;

/// The loader-driver interface version we ask for: 5 = the newest whose loader duties this module meets (v5: the driver may report any `apiVersion`; v6 is Windows adapter enumeration, v7 exports through `vk_icdGetInstanceProcAddr` only, which v5 already uses). The ICD answers with the version it speaks.
const LOADER_INTERFACE: u32 = 5;
/// The Vulkan version the probe instance asks for: vulkan plan S2 "required on every host: Vulkan 1.1".
const PROBE_API: u32 = VK_API_VERSION_1_1;

/// The ICD this process uses: its library stays loaded for the process's life.
pub struct Icd {
    pub manifest: PathBuf,
    pub interface: u32,
    pub gipa: GetInstanceProcAddr,
    /// The device exposed and its tier; Err: none is, the refusal naming gralloc's node and the devices.
    pub chosen: std::result::Result<(Candidate, Tier), String>,
    _lib: Library,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "ICD")]
    icd: ManifestIcd,
}

#[derive(Deserialize)]
struct ManifestIcd {
    library_path: String,
    /// "32" or "64" where the manifest says; a mismatch is skipped as the Khronos loader does.
    library_arch: Option<String>,
}

/// The manifest dirs, in the Khronos loader's Linux order.
fn manifest_dirs() -> Vec<PathBuf> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let list = |name: &str, default: &str| -> Vec<PathBuf> {
        var(name)
            .unwrap_or_else(|| default.to_owned())
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect()
    };
    let home = var("HOME").map(PathBuf::from);
    let mut roots = Vec::new();
    roots.extend(
        var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config"))),
    );
    roots.extend(list("XDG_CONFIG_DIRS", "/etc/xdg"));
    roots.push(PathBuf::from("/etc"));
    roots.extend(
        var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share"))),
    );
    roots.extend(list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    roots.into_iter().map(|r| r.join("vulkan/icd.d")).collect()
}

/// Every manifest, in search order; per dir by file name.
pub fn manifests() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in manifest_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        out.extend(files);
    }
    out
}

/// The first ICD with a device on gralloc's `node` (device::pick); none has one -> the first ICD that loads, exposing no device, its refusal naming every ICD's devices. The error names every manifest that failed to load and why.
pub fn choose(node: &ProcessNode) -> Result<Icd> {
    let all = manifests();
    ensure!(!all.is_empty(), "no ICD manifest in {:?}", manifest_dirs());
    let mut skipped = Vec::new();
    let mut refused: Option<Icd> = None;
    let mut refusals = Vec::new();
    for manifest in all {
        match load(&manifest, node) {
            Ok(icd) if icd.chosen.is_ok() => return Ok(icd),
            Ok(icd) => {
                if let Err(why) = &icd.chosen {
                    refusals.push(format!("{}: {why}", manifest.display()));
                }
                refused.get_or_insert(icd);
            }
            Err(err) => skipped.push(format!("{}: {err:#}", manifest.display())),
        }
    }
    match refused {
        Some(mut icd) => {
            icd.chosen = Err(refusals.join(" | "));
            Ok(icd)
        }
        None => bail!("no host ICD loads: {}", skipped.join("; ")),
    }
}

fn load(manifest: &Path, node: &ProcessNode) -> Result<Icd> {
    let text = std::fs::read_to_string(manifest)?;
    let m: Manifest = serde_json::from_str(&text).context("manifest")?;
    let width = (usize::BITS).to_string();
    if let Some(arch) = &m.icd.library_arch {
        ensure!(
            *arch == width,
            "library_arch {arch}, this process is {width}-bit"
        );
    }
    // a path with a '/' is relative to the manifest; a bare name goes through the dynamic loader's search
    let path = if m.icd.library_path.contains('/') {
        manifest
            .parent()
            .context("manifest dir")?
            .join(&m.icd.library_path)
    } else {
        PathBuf::from(&m.icd.library_path)
    };
    // SAFETY: loading a host Vulkan ICD runs its initialisers; the manifest named it as one, which is all the Khronos loader checks either
    let lib = unsafe { Library::open(Some(&path), RTLD_NOW | RTLD_LOCAL) }
        .with_context(|| format!("dlopen {}", path.display()))?;
    let mut interface = LOADER_INTERFACE;
    // SAFETY: the symbol's type is the loader-driver interface's
    let negotiate = unsafe { lib.get::<Negotiate>(b"vk_icdNegotiateLoaderICDInterfaceVersion\0") };
    if let Ok(negotiate) = negotiate {
        // SAFETY: a valid out-pointer, per the interface
        let r = unsafe { negotiate(&mut interface) };
        ensure!(r == VK_SUCCESS, "interface negotiation: VkResult {r}");
    } else {
        interface = 1;
    }
    // SAFETY: the symbol's type is the loader-driver interface's
    let gipa: GetInstanceProcAddr =
        *unsafe { lib.get::<GetInstanceProcAddr>(b"vk_icdGetInstanceProcAddr\0") }
            .context("no vk_icdGetInstanceProcAddr")?;
    let devices = probe(gipa)?;
    let chosen = device::pick(node, &devices).map(|(i, tier)| (devices[i].clone(), tier));
    Ok(Icd {
        manifest: manifest.to_owned(),
        interface,
        gipa,
        chosen,
        _lib: lib,
    })
}

/// A command of `instance` (null: a global one), by its Vulkan name.
///
/// # Safety
/// `instance` is null or a live instance of the ICD `gipa` belongs to; `F` is the command's function-pointer type.
pub unsafe fn proc<F: Copy>(
    gipa: GetInstanceProcAddr,
    instance: *mut c_void,
    name: &CStr,
) -> Result<F> {
    ensure!(
        size_of::<F>() == size_of::<unsafe extern "C" fn()>(),
        "not a function pointer type"
    );
    // SAFETY: gipa is the ICD's, called as the interface defines
    let f = unsafe { gipa(instance, name.as_ptr()) }
        .with_context(|| format!("{name:?} not provided"))?;
    // SAFETY: F is the command's function-pointer type (same size, checked); the ICD returned that command
    Ok(unsafe { std::mem::transmute_copy::<unsafe extern "C" fn(), F>(&f) })
}

/// A probe instance on the ICD: every physical device as [`device::pick`] sees it, or why there is none.
fn probe(gipa: GetInstanceProcAddr) -> Result<Vec<Candidate>> {
    // SAFETY: a global command, typed per the spec
    let create: CreateInstance = unsafe { proc(gipa, std::ptr::null_mut(), c"vkCreateInstance") }?;
    let name = CString::new("atl-vulkan-driver probe")?;
    let app = VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: std::ptr::null(),
        pApplicationName: name.as_ptr(),
        applicationVersion: 0,
        pEngineName: std::ptr::null(),
        engineVersion: 0,
        apiVersion: PROBE_API,
    };
    // SAFETY: an all-zero create info is valid (no layers, no extensions); the fields set below complete it
    let mut info: VkInstanceCreateInfo = unsafe { std::mem::zeroed() };
    info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    info.pApplicationInfo = &app;
    let mut instance = std::ptr::null_mut();
    // SAFETY: valid create info and out-pointer
    let r = unsafe { create(&info, std::ptr::null(), &mut instance) };
    ensure!(r == VK_SUCCESS, "vkCreateInstance: VkResult {r}");
    let result = (|| {
        // SAFETY: the live instance created above
        let devices = unsafe { physical_devices(gipa, instance) }?;
        ensure!(!devices.is_empty(), "0 physical devices");
        devices
            .into_iter()
            // SAFETY: handles this live 1.1 instance enumerated
            .map(|p| unsafe { candidate(gipa, instance, p) })
            .collect()
    })();
    // SAFETY: the live instance created above
    let destroy: DestroyInstance = unsafe { proc(gipa, instance, c"vkDestroyInstance") }?;
    // SAFETY: the instance created above, no child objects left
    unsafe { destroy(instance, std::ptr::null()) };
    result
}

/// Every physical device of `instance`, as the ICD enumerates them.
///
/// # Safety
/// `instance` is a live instance of the ICD `gipa` belongs to.
pub unsafe fn physical_devices(
    gipa: GetInstanceProcAddr,
    instance: *mut c_void,
) -> Result<Vec<*mut c_void>> {
    // SAFETY: the caller's instance; command typed per the spec
    let enumerate: EnumeratePhysicalDevices =
        unsafe { proc(gipa, instance, c"vkEnumeratePhysicalDevices") }?;
    loop {
        let mut count = 0u32;
        // SAFETY: count query
        let r = unsafe { enumerate(instance, &mut count, std::ptr::null_mut()) };
        ensure!(r == VK_SUCCESS, "vkEnumeratePhysicalDevices: VkResult {r}");
        let mut out = vec![std::ptr::null_mut(); count as usize];
        // SAFETY: room for count handles
        let r = unsafe { enumerate(instance, &mut count, out.as_mut_ptr()) };
        // VK_INCOMPLETE: a device appeared between the calls; ask again
        if r == VK_INCOMPLETE {
            continue;
        }
        ensure!(r == VK_SUCCESS, "vkEnumeratePhysicalDevices: VkResult {r}");
        out.truncate(count as usize);
        return Ok(out);
    }
}

/// `p`'s 1.0 properties: what [`Key`] and the log name it by.
///
/// # Safety
/// `p` is a physical device of a live instance of the ICD `gipa` belongs to.
pub unsafe fn properties(
    gipa: GetInstanceProcAddr,
    instance: *mut c_void,
    p: *mut c_void,
) -> Result<(Key, &'static str, bool)> {
    // SAFETY: the caller's instance; command typed per the spec
    let props: GetPhysicalDeviceProperties =
        unsafe { proc(gipa, instance, c"vkGetPhysicalDeviceProperties") }?;
    // SAFETY: zeroed is a valid out-record; the ICD fills it
    let mut pp: VkPhysicalDeviceProperties = unsafe { std::mem::zeroed() };
    // SAFETY: the caller's handle
    unsafe { props(p, &mut pp) };
    // SAFETY: deviceName is NUL-terminated within its array (Vulkan spec)
    let name = unsafe { CStr::from_ptr(pp.deviceName.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let key = Key {
        vendor: pp.vendorID,
        device: pp.deviceID,
        name,
    };
    Ok((
        key,
        device_type(pp.deviceType),
        pp.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU,
    ))
}

/// `p` as [`device::pick`] sees it: its properties and, with `VK_EXT_physical_device_drm`, its render node.
///
/// # Safety
/// `p` is a physical device of a live Vulkan 1.1 instance of the ICD `gipa` belongs to.
unsafe fn candidate(
    gipa: GetInstanceProcAddr,
    instance: *mut c_void,
    p: *mut c_void,
) -> Result<Candidate> {
    // SAFETY: the caller's
    let (key, kind, cpu) = unsafe { properties(gipa, instance, p) }?;
    // SAFETY: the caller's
    let render = if unsafe { has_device_extension(gipa, instance, p, DRM_EXT) }? {
        // SAFETY: a 1.1 instance has the core command; typed per the spec
        let props2: GetPhysicalDeviceProperties2 =
            unsafe { proc(gipa, instance, c"vkGetPhysicalDeviceProperties2") }?;
        // SAFETY: zeroed out-records, chained below; the ICD fills them
        let mut drm: VkPhysicalDeviceDrmPropertiesEXT = unsafe { std::mem::zeroed() };
        drm.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT;
        // SAFETY: as above
        let mut p2: VkPhysicalDeviceProperties2 = unsafe { std::mem::zeroed() };
        p2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
        p2.pNext = (&raw mut drm).cast();
        // SAFETY: the device supports the chained struct's extension (checked above)
        unsafe { props2(p, &mut p2) };
        (drm.hasRender != 0).then(|| device::dev_number(drm.renderMajor, drm.renderMinor))
    } else {
        None
    };
    Ok(Candidate {
        name: key.name.clone(),
        kind,
        cpu,
        render,
        key,
    })
}

/// Whether `p` lists device extension `name`.
///
/// # Safety
/// `p` is a physical device of a live instance of the ICD `gipa` belongs to.
unsafe fn has_device_extension(
    gipa: GetInstanceProcAddr,
    instance: *mut c_void,
    p: *mut c_void,
    name: &CStr,
) -> Result<bool> {
    // SAFETY: the caller's instance; command typed per the spec
    let enumerate: EnumerateDeviceExtensionProperties =
        unsafe { proc(gipa, instance, c"vkEnumerateDeviceExtensionProperties") }?;
    let mut count = 0u32;
    // SAFETY: count query, no layer
    let r = unsafe { enumerate(p, std::ptr::null(), &mut count, std::ptr::null_mut()) };
    ensure!(
        r == VK_SUCCESS,
        "vkEnumerateDeviceExtensionProperties: VkResult {r}"
    );
    // SAFETY: zeroed records are valid out-records
    let mut exts: Vec<VkExtensionProperties> =
        (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    // SAFETY: room for count records
    let r = unsafe { enumerate(p, std::ptr::null(), &mut count, exts.as_mut_ptr()) };
    ensure!(
        r == VK_SUCCESS || r == VK_INCOMPLETE,
        "vkEnumerateDeviceExtensionProperties: VkResult {r}"
    );
    Ok(exts[..count as usize].iter().any(|e| {
        // SAFETY: extensionName is NUL-terminated within its array (Vulkan spec)
        let ext = unsafe { CStr::from_ptr(e.extensionName.as_ptr()) };
        ext == name
    }))
}

/// `VkPhysicalDeviceType`'s name (Vulkan spec values).
fn device_type(t: u32) -> &'static str {
    match t {
        VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU => "integrated",
        VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU => "discrete",
        VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU => "virtual",
        VK_PHYSICAL_DEVICE_TYPE_CPU => "cpu",
        _ => "other",
    }
}
