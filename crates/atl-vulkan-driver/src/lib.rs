// SPDX-License-Identifier: MIT
//! `vulkan.atl.so`: the hwvulkan HAL module AOSP's libvulkan loads (driver.cpp `LoadDriver`: `vulkan.<ro.hardware.vulkan>.so`, `RTLD_LOCAL | RTLD_NOW`, symbol `HMI` with id `vulkan`, then `open("vk0")`).
//! One host Vulkan ICD drives it directly, no Khronos loader in between ([`icd`]): at open the first ICD manifest whose ICD has a physical device on gralloc's node is chosen ([`device`]), and libvulkan gets that ICD's own `vkEnumerateInstanceExtensionProperties`, `vkCreateInstance` and commands, except the physical-device enumerations, which expose only that device ([`hal`]).
//! No wrapper per command: the Khronos ICD contract stamps `ICD_LOADER_MAGIC` (`vk_icd.h`, 0x01CDC0DE) into the first pointer of every dispatchable object, the same value libvulkan requires there (`hwvulkan.h` `HWVULKAN_DISPATCH_MAGIC`, driver.h `SetDataInternal`), so the ICD's handles pass unchanged (the probe checks each kind).

pub mod abi {
    //! The HAL and Vulkan records, generated from AOSP's headers (build.rs).
    #[allow(
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        dead_code,
        clippy::all
    )]
    pub mod hal {
        include!(concat!(env!("OUT_DIR"), "/hal.rs"));
    }
}

pub mod device;
pub mod hal;
pub mod icd;
