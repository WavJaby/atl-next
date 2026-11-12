// SPDX-License-Identifier: MIT
//! The host Vulkan ICD this process drives: found the way the Khronos loader finds ICDs on Linux (JSON manifests in `vulkan/icd.d` under the XDG config and data dirs and `/etc`, Khronos loader `LoaderDriverInterface.md` "Driver Discovery on Linux"), loaded `RTLD_NOW | RTLD_LOCAL` so its symbols never reach the app's scope.
//! Chosen: the first manifest, in search order then file name, whose ICD creates an instance and enumerates at least one physical device. The XDG variables are the discovery spec's own inputs (the desktop sets them for every Vulkan program), not a switch of ours.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
use serde::Deserialize;

use crate::abi::hal::{
    VK_API_VERSION_1_1, VK_INCOMPLETE, VK_PHYSICAL_DEVICE_TYPE_CPU,
    VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU, VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU,
    VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU, VK_STRUCTURE_TYPE_APPLICATION_INFO,
    VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, VK_SUCCESS, VkApplicationInfo, VkInstanceCreateInfo,
    VkPhysicalDeviceProperties,
};

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

/// The loader-driver interface version we ask for: 5 = the newest whose loader duties this module meets (v5: the driver may report any `apiVersion`; v6 is Windows adapter enumeration, v7 exports through `vk_icdGetInstanceProcAddr` only, which v5 already uses). The ICD answers with the version it speaks.
const LOADER_INTERFACE: u32 = 5;
/// The Vulkan version the probe instance asks for: vulkan plan S2 "required on every host: Vulkan 1.1".
const PROBE_API: u32 = VK_API_VERSION_1_1;

/// The ICD this process uses: its library stays loaded for the process's life.
pub struct Icd {
    pub manifest: PathBuf,
    pub interface: u32,
    pub gipa: GetInstanceProcAddr,
    /// The first physical device the probe instance enumerated.
    pub device: String,
    pub device_type: &'static str,
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

/// The first usable ICD; the error names every manifest tried and why it was passed over.
pub fn choose() -> Result<Icd> {
    let all = manifests();
    ensure!(!all.is_empty(), "no ICD manifest in {:?}", manifest_dirs());
    let mut skipped = Vec::new();
    for manifest in all {
        match load(&manifest) {
            Ok(icd) => return Ok(icd),
            Err(err) => skipped.push(format!("{}: {err:#}", manifest.display())),
        }
    }
    bail!(
        "no host ICD exposes a physical device: {}",
        skipped.join("; ")
    )
}

fn load(manifest: &Path) -> Result<Icd> {
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
    if let Ok(negotiate) =
        unsafe { lib.get::<Negotiate>(b"vk_icdNegotiateLoaderICDInterfaceVersion\0") }
    {
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
    let (device, device_type) = probe(gipa)?;
    Ok(Icd {
        manifest: manifest.to_owned(),
        interface,
        gipa,
        device,
        device_type,
        _lib: lib,
    })
}

/// A command of `instance` (null: a global one), by its Vulkan name.
pub fn proc<F: Copy>(gipa: GetInstanceProcAddr, instance: *mut c_void, name: &CStr) -> Result<F> {
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

/// A probe instance on the ICD: its first physical device's name and type, or why there is none.
fn probe(gipa: GetInstanceProcAddr) -> Result<(String, &'static str)> {
    let create: CreateInstance = proc(gipa, std::ptr::null_mut(), c"vkCreateInstance")?;
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
        let enumerate: EnumeratePhysicalDevices =
            proc(gipa, instance, c"vkEnumeratePhysicalDevices")?;
        let props: GetPhysicalDeviceProperties =
            proc(gipa, instance, c"vkGetPhysicalDeviceProperties")?;
        let mut count = 1u32;
        let mut first = std::ptr::null_mut();
        // SAFETY: room for one handle; VK_INCOMPLETE when there are more
        let r = unsafe { enumerate(instance, &mut count, &mut first) };
        ensure!(
            r == VK_SUCCESS || r == VK_INCOMPLETE,
            "vkEnumeratePhysicalDevices: VkResult {r}"
        );
        ensure!(count > 0, "0 physical devices");
        // SAFETY: zeroed is a valid out-record; the ICD fills it
        let mut p: VkPhysicalDeviceProperties = unsafe { std::mem::zeroed() };
        // SAFETY: a handle this instance enumerated
        unsafe { props(first, &mut p) };
        // SAFETY: deviceName is NUL-terminated within its array (Vulkan spec)
        let device = unsafe { CStr::from_ptr(p.deviceName.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Ok((device, device_type(p.deviceType)))
    })();
    let destroy: DestroyInstance = proc(gipa, instance, c"vkDestroyInstance")?;
    // SAFETY: the instance created above, no child objects left
    unsafe { destroy(instance, std::ptr::null()) };
    result
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
