// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen (bridge controls 1 and 2) `OUT_DIR/hal.rs`: the hwvulkan HAL records (`hwvulkan.h` over libhardware's `hardware.h`) and the Vulkan records the probe passes, with clang's layout asserted.

use atl_bridgegen::buildscript::{HeaderImport, import_headers};

const IMPORTS: &[HeaderImport] = &[HeaderImport {
    name: "hal",
    source: "frameworks/native/vulkan/include/hardware/hwvulkan.h",
    also: &[
        "hardware/libhardware/include/hardware/hardware.h",
        "external/vulkan-headers/include/vulkan/vulkan_core.h",
    ],
    include_dirs: &[
        "frameworks/native/vulkan/include",
        "hardware/libhardware/include",
        "external/vulkan-headers/include",
        "system/core/libcutils/include",
        "system/core/libsystem/include",
    ],
    defines: &[],
    force_include: None,
    functions: &[],
    types: &[
        "hwvulkan_module_t",
        "hwvulkan_device_t",
        "VkInstanceCreateInfo",
        "VkDeviceCreateInfo",
        "VkPhysicalDeviceProperties",
        "VkQueueFamilyProperties",
        "VkExtensionProperties",
        "PFN_vkEnumeratePhysicalDevices",
        "PFN_vkGetPhysicalDeviceProperties",
        "PFN_vkGetPhysicalDeviceQueueFamilyProperties",
        "PFN_vkCreateDevice",
        "PFN_vkGetDeviceProcAddr",
        "PFN_vkGetDeviceQueue",
        "PFN_vkDestroyDevice",
        "PFN_vkDestroyInstance",
    ],
    macros: &[
        "HWVULKAN_DISPATCH_MAGIC",
        "HWVULKAN_HARDWARE_MODULE_ID",
        "HWVULKAN_DEVICE_0",
        "VK_API_VERSION_1_1",
    ],
}];

fn main() -> anyhow::Result<()> {
    import_headers(IMPORTS)
}
