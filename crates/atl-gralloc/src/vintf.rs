// SPDX-License-Identifier: MIT
//! What the system declares for the graphics allocator: the service name, the mapper library's suffix and the VINTF manifest that declares both.
//! libui (`Gralloc5.cpp` `waitForAllocator`, `loadIMapperLibrary`) and servicemanager (`addService` of a @VintfStability binder) consult the manifest; declaring is the AOSP way to tell them the HAL exists (rung 2: the environment supplies it).
//! One definition: atld writes it into the runtime root, the test harness into its layout.

/// The AIDL interface, instance and version (`android.hardware.graphics.allocator-V2`).
pub const ALLOCATOR_INTERFACE: &str = "android.hardware.graphics.allocator.IAllocator";
pub const ALLOCATOR_INSTANCE: &str = "default";
pub const ALLOCATOR_VERSION: u32 = 2;

/// The name the allocator registers under and libui waits for (`Gralloc5.cpp` `kIAllocatorServiceName`).
pub fn allocator_service_name() -> String {
    format!("{ALLOCATOR_INTERFACE}/{ALLOCATOR_INSTANCE}")
}

/// `getIMapperLibrarySuffix()`: libui loads `mapper.<suffix>.so`, the stable-C IMapper v5 provider (`AIMapper_loadIMapper`).
pub const MAPPER_SUFFIX: &str = "atl";
/// The IMapper stable-C version the library provides (`AIMAPPER_VERSION_5`).
pub const MAPPER_VERSION: &str = "5.0";

/// The file name of the mapper library, as libui's `AServiceManager_openDeclaredPassthroughHal("mapper", suffix)` builds it (`IServiceManager.cpp` `openDeclaredPassthroughHal`: `<interface>.<instance>.so`).
pub fn mapper_library_name() -> String {
    format!("mapper.{MAPPER_SUFFIX}.so")
}

/// The `<hal>` elements of a device manifest (/vendor/etc/vintf/manifest.xml): the shapes libvintf's parser takes for an AIDL HAL and a native one, as AOSP's own external/minigbm declares its allocator / mapper (cros_gralloc/aidl/allocator.xml, mapper_stablec/mapper.minigbm.xml).
pub fn device_hals() -> String {
    format!(
        "    <hal format=\"aidl\">\n        <name>android.hardware.graphics.allocator</name>\n        <version>{ALLOCATOR_VERSION}</version>\n        <interface>\n            <name>IAllocator</name>\n            <instance>{ALLOCATOR_INSTANCE}</instance>\n        </interface>\n    </hal>\n    <hal format=\"native\">\n        <name>mapper</name>\n        <version>{MAPPER_VERSION}</version>\n        <interface>\n            <instance>{MAPPER_SUFFIX}</instance>\n        </interface>\n    </hal>\n"
    )
}

/// A VINTF manifest of `kind` (`device` or `framework`) declaring `hals` (`<hal>` elements, [`device_hals`]'s shape): libvintf's schema (system/libvintf/parse_xml.cpp `manifest` element, version attribute required). Written to the default paths /system/etc/vintf/manifest.xml (framework), /vendor/etc/vintf/manifest.xml (device) (system/libvintf/include/vintf/constants.h).
pub fn manifest(kind: &str, hals: &str) -> String {
    format!("<manifest version=\"1.0\" type=\"{kind}\">\n{hals}</manifest>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_wraps_the_hals() {
        assert_eq!(
            manifest("framework", ""),
            "<manifest version=\"1.0\" type=\"framework\">\n</manifest>\n"
        );
        let m = manifest("device", &device_hals());
        assert!(m.starts_with("<manifest version=\"1.0\" type=\"device\">\n    <hal"));
        assert!(m.ends_with("</hal>\n</manifest>\n"));
    }

    #[test]
    fn names() {
        assert_eq!(
            allocator_service_name(),
            "android.hardware.graphics.allocator.IAllocator/default"
        );
        assert_eq!(mapper_library_name(), "mapper.atl.so");
    }

    #[test]
    fn hals_declare_both() {
        let h = device_hals();
        assert!(h.contains("<instance>default</instance>"));
        assert!(h.contains("<instance>atl</instance>"));
        assert!(h.contains("<version>2</version>") && h.contains("<version>5.0</version>"));
    }
}
