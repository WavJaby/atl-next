// SPDX-License-Identifier: MIT
//! The Android filesystem layout as an app process sees it inside its sandbox: the one place an Android path literal lives (plan S18 sandbox layout table, first cut), and the variable that names the property area on the host until the sandbox mounts it (`PROPERTY_AREA_ENV`).
//! `xtask check` step `android-paths` fails on such a literal anywhere else in the workspace, unless android-paths.toml allows it with a reason; its roots are `ROOTS`.
#![forbid(unsafe_code)]

/// `ANDROID_ROOT` on a device: the platform partition, bound read-only.
pub const SYSTEM: &str = "/system";
/// The vendor partition (`Environment.getVendorDirectory()`), bound read-only: the GL driver (libEGL's Loader.cpp `/vendor/lib64/egl`), /vendor/build.prop.
pub const VENDOR: &str = "/vendor";
/// The product partition (`Environment.getProductDirectory()`), bound read-only: preinstalled apps under `app/`, `priv-app/` (PackageManager's system scan).
pub const PRODUCT: &str = "/product";
/// The system_ext partition (`Environment.getSystemExtDirectory()`), bound read-only, laid out as [`PRODUCT`].
pub const SYSTEM_EXT: &str = "/system_ext";
/// The platform's native libraries for the process's pointer width (bionic linker's `/system/lib64` in LP64, `/system/lib` in ILP32): libEGL's Loader.cpp `SYSTEM_LIB_PATH`.
pub const SYSTEM_LIB: &str = if cfg!(target_pointer_width = "64") {
    "/system/lib64"
} else {
    "/system/lib"
};
/// The platform's executables.
pub const SYSTEM_BIN: &str = "/system/bin";

/// Guest native libraries of native-bridge ISA `isa` at guest pointer width `pointer_bits`: Soong installs a `native_bridge_supported` module's bridge variant under
/// `lib<bits>/<NativeBridgeRelativePath>` by the module's own width (`lib` for 32-bit, `lib64` for 64-bit), whatever the host's: the guest linker's default search path.
pub fn native_bridge_lib(isa: &str, pointer_bits: u32) -> String {
    let dir = if pointer_bits == 32 {
        "/system/lib"
    } else {
        "/system/lib64"
    };
    format!("{dir}/{isa}")
}

/// Guest executables of native-bridge ISA `isa` (`bin/<NativeBridgeRelativePath>`): its linker and app_process ([`native_bridge_exes`]).
pub fn native_bridge_bin(isa: &str) -> String {
    format!("{SYSTEM_BIN}/{isa}")
}

/// File names of the guest linker and main executable at guest pointer width `pointer_bits`, as Soong installs them: NBS linker `stem: "linker"` + lib64 `suffix: "64"`, app_process lib32 `suffix: "32"` / lib64 `suffix: "64"` (their Android.bp).
pub fn native_bridge_exes(pointer_bits: u32) -> (&'static str, &'static str) {
    if pointer_bits == 32 {
        ("linker", "app_process32")
    } else {
        ("linker64", "app_process64")
    }
}

/// The translating engine `.so` for guest ISA `isa` (a host library, C ABI `atl_engine`): the NB core dlopens it when ART's `initialize` names a foreign ISA. ART hands `initialize` only an app-private directory, so the location is fixed here.
pub fn engine_library(isa: &str) -> String {
    format!("{SYSTEM_LIB}/libatl_engine_{isa}.so")
}

/// The engine's boot executable for guest ISA `isa`: a static guest executable whose first instruction raises BOOT, run by [`engine_library`]'s engine at start; a guest binary, so beside the guest's `linker64`.
pub fn engine_boot(isa: &str) -> String {
    format!("{}/atl_engine_boot", native_bridge_bin(isa))
}

/// The CPU description a native-bridge app's host-side readers (Java, the runtime) see; a device bind-mounts the bridge's `cpuinfo.<isa>.txt` over it (libnativebridge `MountCpuinfo`), here the broker binds atld's guest cpuinfo ([`engine_cpuinfo`]).
pub const PROC_CPUINFO: &str = "/proc/cpuinfo";

/// The engine's cpuinfo probe for guest ISA `isa`: a static guest executable that copies the guest's `/proc/cpuinfo` (the engine's own formatter) to stdout; atld runs it once through [`engine_library`] and binds the output over translated apps' `/proc/cpuinfo`.
pub fn engine_cpuinfo(isa: &str) -> String {
    format!("{}/atl_engine_cpuinfo", native_bridge_bin(isa))
}

/// libEGL's Loader.cpp `VENDOR_LIB_EGL_DIR` for the process's pointer width: where it looks for `lib{EGL,GLESv1_CM,GLESv2}_<ro.hardware.egl>.so`.
pub const VENDOR_EGL: &str = if cfg!(target_pointer_width = "64") {
    "/vendor/lib64/egl"
} else {
    "/vendor/lib/egl"
};
/// The GL driver's three files in [`VENDOR_EGL`], Loader.cpp's `lib<kind>_<ro.hardware.egl>.so` with `ro.hardware.egl=atl`: the driver (atl-egl-driver) as the EGL library and again as the ES2 library (same file, so the dynamic loader maps it once), ANGLE's ES1 library as the ES1 one.
pub const GL_DRIVER_EGL: &str = "libEGL_atl.so";
pub const GL_DRIVER_GLES2: &str = "libGLESv2_atl.so";
pub const GL_DRIVER_GLES1: &str = "libGLESv1_CM_atl.so";
/// ANGLE's EGL library, where Loader.cpp opens the ANGLE driver (`SYSTEM_LIB_PATH`): the driver's ES1 contexts run on it (atl-egl-driver es1.rs).
pub const ANGLE_EGL: &str = if cfg!(target_pointer_width = "64") {
    "/system/lib64/libEGL_angle.so"
} else {
    "/system/lib/libEGL_angle.so"
};
/// The vendor partition's properties (`ro.hardware.egl`), joined into the property area.
pub const VENDOR_BUILD_PROP: &str = "/vendor/build.prop";
/// Framework jars and their libraries (`Environment.getRootDirectory()/framework`).
pub const SYSTEM_FRAMEWORK: &str = "/system/framework";
/// The system fonts (SystemFonts.DEVICE_FONT_DIR); the desktop's font dirs are bound read-only at `<k>/` below it.
pub const SYSTEM_FONTS: &str = "/system/fonts";
/// The CA store Conscrypt's `TrustedCertificateStore` reads (`$ANDROID_ROOT/etc/security/cacerts`).
pub const SYSTEM_CACERTS: &str = "/system/etc/security/cacerts";
/// AOSP's key layout for a keyboard with no own files (`KeyMap::load`'s `Generic` fallback under `$ANDROID_ROOT/usr/keylayout`), where the environment links it.
pub const SYSTEM_GENERIC_KEY_LAYOUT: &str = "/system/usr/keylayout/Generic.kl";
/// `ANDROID_DATA`: per-app install and data dirs, bound writable.
pub const DATA: &str = "/data";
/// Installed packages (`Environment.getDataAppDirectory`): `<DATA_APP>/<package>/base.apk`.
pub const DATA_APP: &str = "/data/app";
/// Credential-encrypted user data (`Environment.getDataUserCeDirectory`): `<DATA_USER>/<user>/<package>`.
pub const DATA_USER: &str = "/data/user";
/// Device-encrypted user data (`Environment.getDataUserDeDirectory`): `<DATA_USER_DE>/<user>/<package>`.
pub const DATA_USER_DE: &str = "/data/user_de";
/// keystore2's database dir (keystore2.rc: `/system/bin/keystore2 /data/misc/keystore`), its argv[1].
pub const KEYSTORE_DB: &str = "/data/misc/keystore";
/// bionic's default property area path (`__system_properties_init` without `$ATL_PROP_AREA`): the sandbox's `/dev` is the broker's own, so the area is mounted there and no variable is needed.
pub const PROPERTY_AREA: &str = "/dev/__properties__";
/// The variable naming the property area file a process maps instead of `PROPERTY_AREA` (`ATL_PROP_AREA`): whoever starts the process sets it until the sandbox mounts the area there. The one definition: atl-bionic reads it, the launcher names it, atl-property-service prints it.
pub const PROPERTY_AREA_ENV: &str = "ATL_PROP_AREA";
/// The shared storage root (Environment's external storage, the path the mount service reports for the emulated primary): app_spawn mounts each package's Android/data and Android/obb under it.
pub const SHARED_STORAGE: &str = "/storage/emulated/0";
/// The legacy external storage link, pointing at `SHARED_STORAGE`.
pub const SDCARD: &str = "/sdcard";

/// Path roots that make a string literal an Android path for the gate: a literal equal to one or below one (`<root>/...`). Covers partitions not laid out yet, so their first use comes here too.
pub const ROOTS: &[&str] = &[
    "/system",
    "/system_ext",
    "/vendor",
    "/odm",
    "/product",
    "/apex",
    "/data",
    "/dev/__properties__",
    "/storage",
    "/sdcard",
    "/mnt",
];

/// A package name as atld's commit accepts it, and the only form run-apk builds paths from: ASCII letters, digits, `.` and `_`, not starting with `.`. Empty is no name.
pub fn valid_package_name(package: &str) -> bool {
    !package.is_empty()
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && !package.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::valid_package_name;

    /// Each example is one clause of the rule: charset, leading `.`, separator, empty, non-ASCII.
    #[test]
    fn package_names_follow_the_commit_rule() {
        for ok in ["com.example.app", "a", "A_b.c1", "x..y"] {
            assert!(valid_package_name(ok), "{ok:?} is a package name");
        }
        for bad in [
            "",
            ".",
            "..",
            ".x",
            "/home/u/proj",
            "../x",
            "a/b",
            "a b",
            "a-b",
            "a\u{e9}",
            "a\0",
        ] {
            assert!(!valid_package_name(bad), "{bad:?} is no package name");
        }
    }
}
