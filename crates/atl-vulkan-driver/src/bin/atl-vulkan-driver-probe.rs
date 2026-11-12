// SPDX-License-Identifier: MIT
//! atl-vulkan-driver-probe: loads `vulkan.atl.so` as AOSP's libvulkan does (driver.cpp `LoadDriver` + `Hal::Open`: `dlopen(RTLD_LOCAL | RTLD_NOW)`, `HMI`, id `vulkan`, `open("vk0")`) and drives the device it returns: instance -> every physical device -> a device with one queue.
//! Judge: libvulkan's own acceptance test of a driver's handles (driver.h `SetDataInternal`): the first pointer of the VkInstance, each VkPhysicalDevice, the VkDevice and its VkQueue holds `HWVULKAN_DISPATCH_MAGIC` before libvulkan writes its table there. Prints `vk-driver: PASS ...` or `vk-driver: FAIL ...`, exit status to match.
//! `--break magic`: the judge expects another value, so every slot must be reported red (the check discriminates).

use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use atl_vulkan_driver::abi::hal::{
    HWVULKAN_DEVICE_0, HWVULKAN_DISPATCH_MAGIC, HWVULKAN_HARDWARE_MODULE_ID, VK_API_VERSION_1_1,
    VK_INCOMPLETE, VK_STRUCTURE_TYPE_APPLICATION_INFO, VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
    VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, VK_SUCCESS,
    VkApplicationInfo, VkDeviceCreateInfo, VkDeviceQueueCreateInfo, VkInstance_T,
    VkInstanceCreateInfo, VkPhysicalDeviceProperties, hw_device_t, hw_module_t, hwvulkan_device_t,
};
use atl_vulkan_driver::icd::{GetInstanceProcAddr, proc};
use clap::Parser;
use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};

type H = *mut c_void;
type EnumeratePhysicalDevices = unsafe extern "C" fn(H, *mut u32, *mut H) -> i32;
type GetPhysicalDeviceProperties = unsafe extern "C" fn(H, *mut VkPhysicalDeviceProperties);
type CreateDevice =
    unsafe extern "C" fn(H, *const VkDeviceCreateInfo, *const c_void, *mut H) -> i32;
type GetDeviceProcAddr = unsafe extern "C" fn(H, *const c_char) -> Option<unsafe extern "C" fn()>;
type GetDeviceQueue = unsafe extern "C" fn(H, u32, u32, *mut H);
type DestroyDevice = unsafe extern "C" fn(H, *const c_void);
type DestroyInstance = unsafe extern "C" fn(H, *const c_void);

#[derive(Parser)]
struct Args {
    /// The driver library (vulkan.atl.so).
    #[arg(long)]
    driver: PathBuf,
    /// `magic`: the judge expects a wrong value (control).
    #[arg(long = "break")]
    brk: Option<String>,
}

#[allow(
    clippy::print_stdout,
    reason = "the probe's verdict lines are its output"
)]
fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(line) => {
            println!("vk-driver: PASS {line}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("vk-driver: FAIL {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[allow(
    clippy::print_stdout,
    reason = "the probe's per-handle lines are its output"
)]
fn run(args: &Args) -> Result<String> {
    let expected = match args.brk.as_deref() {
        None => HWVULKAN_DISPATCH_MAGIC as usize,
        Some("magic") => !(HWVULKAN_DISPATCH_MAGIC as usize),
        Some(other) => bail!("unknown break {other} (known: magic)"),
    };
    // SAFETY: loading the driver under test, as libvulkan does
    let lib = unsafe { Library::open(Some(&args.driver), RTLD_NOW | RTLD_LOCAL) }
        .context("dlopen driver")?;
    // SAFETY: HMI is a hw_module_t-headed record (hwvulkan_module_t)
    let hmi = *unsafe { lib.get::<*mut hw_module_t>(b"HMI\0") }.context("no HMI")?;
    // SAFETY: the symbol's address is the record
    let module = unsafe { &*hmi };
    // SAFETY: id is a NUL-terminated string in the record
    let id = unsafe { CStr::from_ptr(module.id) };
    ensure!(
        id.to_bytes() == HWVULKAN_HARDWARE_MODULE_ID.as_bytes(),
        "HAL id {id:?}"
    );
    // SAFETY: methods points at the module's methods record
    let open = unsafe { (*module.methods).open }.context("no open")?;
    let dev_id = std::ffi::CString::new(HWVULKAN_DEVICE_0)?;
    let mut common: *mut hw_device_t = std::ptr::null_mut();
    // SAFETY: as libvulkan calls it
    let r = unsafe { open(hmi, dev_id.as_ptr(), &mut common) };
    ensure!(r == 0 && !common.is_null(), "open(vk0): {r}");
    // SAFETY: a vk0 device is a hwvulkan_device_t headed by hw_device_t
    let dev = unsafe { &*common.cast::<hwvulkan_device_t>() };
    let enum_ext = dev
        .EnumerateInstanceExtensionProperties
        .context("no EnumerateInstanceExtensionProperties")?;
    let create = dev.CreateInstance.context("no CreateInstance")?;
    // SAFETY: same signature, the instance handle typed opaque here
    let gipa = unsafe {
        std::mem::transmute::<
            unsafe extern "C" fn(
                *mut VkInstance_T,
                *const c_char,
            ) -> Option<unsafe extern "C" fn()>,
            GetInstanceProcAddr,
        >(dev.GetInstanceProcAddr.context("no GetInstanceProcAddr")?)
    };

    let mut n_ext = 0u32;
    // SAFETY: count query
    let r = unsafe { enum_ext(std::ptr::null(), &mut n_ext, std::ptr::null_mut()) };
    ensure!(
        r == VK_SUCCESS,
        "vkEnumerateInstanceExtensionProperties: {r}"
    );
    println!("vk-driver: {n_ext} instance extensions");

    let app = VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: std::ptr::null(),
        pApplicationName: c"atl-vulkan-driver-probe".as_ptr(),
        applicationVersion: 0,
        pEngineName: std::ptr::null(),
        engineVersion: 0,
        apiVersion: VK_API_VERSION_1_1,
    };
    // SAFETY: zeroed create info is valid; completed below
    let mut info: VkInstanceCreateInfo = unsafe { std::mem::zeroed() };
    info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    info.pApplicationInfo = &app;
    let mut instance: H = std::ptr::null_mut();
    // SAFETY: valid create info and out-pointer
    let r = unsafe { create(&info, std::ptr::null(), (&raw mut instance).cast()) };
    ensure!(r == VK_SUCCESS, "vkCreateInstance: {r}");
    let mut red = Vec::new();
    let mut slot = |kind: &str, h: H| {
        // SAFETY: a dispatchable handle points at an object whose first member is the dispatch slot
        let v = unsafe { *h.cast::<usize>() };
        let ok = v == expected;
        println!(
            "vk-driver: dispatch slot {kind} {v:#x} {}",
            if ok { "ok" } else { "red" }
        );
        if !ok {
            red.push(kind.to_owned());
        }
    };
    slot("VkInstance", instance);

    // SAFETY: the live instance; commands typed per the spec
    let enumerate: EnumeratePhysicalDevices =
        unsafe { proc(gipa, instance, c"vkEnumeratePhysicalDevices") }?;
    // SAFETY: as above
    let props: GetPhysicalDeviceProperties =
        unsafe { proc(gipa, instance, c"vkGetPhysicalDeviceProperties") }?;
    let mut count = 0u32;
    // SAFETY: count query
    let r = unsafe { enumerate(instance, &mut count, std::ptr::null_mut()) };
    ensure!(r == VK_SUCCESS, "vkEnumeratePhysicalDevices count: {r}");
    ensure!(count > 0, "0 physical devices");
    let mut phys = vec![std::ptr::null_mut(); count as usize];
    // SAFETY: room for count handles
    let r = unsafe { enumerate(instance, &mut count, phys.as_mut_ptr()) };
    ensure!(
        r == VK_SUCCESS || r == VK_INCOMPLETE,
        "vkEnumeratePhysicalDevices: {r}"
    );
    let mut name = String::new();
    for &p in &phys[..count as usize] {
        slot("VkPhysicalDevice", p);
        // SAFETY: zeroed out-record, filled by the driver
        let mut pp: VkPhysicalDeviceProperties = unsafe { std::mem::zeroed() };
        // SAFETY: an enumerated handle
        unsafe { props(p, &mut pp) };
        // SAFETY: NUL-terminated per the spec
        let n = unsafe { CStr::from_ptr(pp.deviceName.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        println!("vk-driver: physical device {n} api {:#x}", pp.apiVersion);
        if name.is_empty() {
            name = n;
        }
    }

    // SAFETY: as above
    let create_device: CreateDevice = unsafe { proc(gipa, instance, c"vkCreateDevice") }?;
    // SAFETY: as above
    let gdpa: GetDeviceProcAddr = unsafe { proc(gipa, instance, c"vkGetDeviceProcAddr") }?;
    let priority = 1.0f32;
    // SAFETY: zeroed create infos are valid; completed below
    let mut queue_info: VkDeviceQueueCreateInfo = unsafe { std::mem::zeroed() };
    queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    queue_info.queueFamilyIndex = 0;
    queue_info.queueCount = 1;
    queue_info.pQueuePriorities = &priority;
    // SAFETY: as above
    let mut dev_info: VkDeviceCreateInfo = unsafe { std::mem::zeroed() };
    dev_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    dev_info.queueCreateInfoCount = 1;
    dev_info.pQueueCreateInfos = &queue_info;
    let mut device: H = std::ptr::null_mut();
    // SAFETY: valid create info (family 0 always exists), out-pointer
    let r = unsafe { create_device(phys[0], &dev_info, std::ptr::null(), &mut device) };
    ensure!(r == VK_SUCCESS, "vkCreateDevice: {r}");
    slot("VkDevice", device);
    let dproc = |n: &CStr| -> Result<unsafe extern "C" fn()> {
        // SAFETY: the device's own proc lookup
        unsafe { gdpa(device, n.as_ptr()) }.with_context(|| format!("{n:?} not provided"))
    };
    // SAFETY: the looked-up commands have these types (Vulkan spec)
    let get_queue = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), GetDeviceQueue>(dproc(c"vkGetDeviceQueue")?)
    };
    // SAFETY: as above
    let destroy_device = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), DestroyDevice>(dproc(c"vkDestroyDevice")?)
    };
    let mut queue: H = std::ptr::null_mut();
    // SAFETY: family 0 index 0 was created
    unsafe { get_queue(device, 0, 0, &mut queue) };
    slot("VkQueue", queue);
    // SAFETY: no child objects left
    unsafe { destroy_device(device, std::ptr::null()) };
    // SAFETY: the live instance
    let destroy: DestroyInstance = unsafe { proc(gipa, instance, c"vkDestroyInstance") }?;
    // SAFETY: as above
    unsafe { destroy(instance, std::ptr::null()) };
    drop(lib);
    ensure!(
        red.is_empty(),
        "dispatch slot not {expected:#x}: {}",
        red.join(", ")
    );
    Ok(format!(
        "device {name}, {count} physical, dispatch slots = HWVULKAN_DISPATCH_MAGIC"
    ))
}
