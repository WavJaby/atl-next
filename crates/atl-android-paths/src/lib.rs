// SPDX-License-Identifier: MIT
//! The Android filesystem layout as an app process sees it inside its sandbox: the one place an Android path literal lives (plan S18 sandbox layout table, first cut).
//! `xtask check` step `android-paths` fails on such a literal anywhere else in the workspace, unless android-paths.toml allows it with a reason; its roots are `ROOTS`.
#![forbid(unsafe_code)]

/// `ANDROID_ROOT` on a device: the platform partition, bound read-only.
pub const SYSTEM: &str = "/system";
/// The vendor partition (`Environment.getVendorDirectory()`), bound read-only: the GL driver (libEGL's Loader.cpp `/vendor/lib64/egl`), /vendor/build.prop.
pub const VENDOR: &str = "/vendor";
/// The platform's native libraries for the process's pointer width (bionic linker's `/system/lib64` in LP64, `/system/lib` in ILP32): libEGL's Loader.cpp `SYSTEM_LIB_PATH`.
pub const SYSTEM_LIB: &str = if cfg!(target_pointer_width = "64") {
    "/system/lib64"
} else {
    "/system/lib"
};
/// The platform's executables.
pub const SYSTEM_BIN: &str = "/system/bin";

/// Guest native libraries of native-bridge ISA `isa` (Soong installs a `native_bridge_supported` module's bridge variant under `lib64/<NativeBridgeRelativePath>`, the guest arch name): the guest linker's default search path.
pub fn native_bridge_lib(isa: &str) -> String {
    format!("{SYSTEM_LIB}/{isa}")
}

/// Guest executables of native-bridge ISA `isa` (`bin/<NativeBridgeRelativePath>`): its `linker64` and `app_process64`.
pub fn native_bridge_bin(isa: &str) -> String {
    format!("{SYSTEM_BIN}/{isa}")
}

/// The translating engine `.so` for guest ISA `isa` (a host library, C ABI `atl_engine`): the NB core dlopens it when ART's `initialize` names a foreign ISA. ART hands `initialize` only an app-private directory, so the location is fixed here.
pub fn engine_library(isa: &str) -> String {
    format!("{SYSTEM_LIB}/libatl_engine_{isa}.so")
}

/// The engine's boot executable for guest ISA `isa`: a static guest executable whose first instruction raises BOOT, run by [`engine_library`]'s engine at start; a guest binary, so beside the guest's `linker64`.
pub fn engine_boot(isa: &str) -> String {
    format!("{}/atl_engine_boot", native_bridge_bin(isa))
}

/// libEGL's Loader.cpp `VENDOR_LIB_EGL_DIR` for the process's pointer width: where it looks for `lib{EGL,GLESv1_CM,GLESv2}_<ro.hardware.egl>.so`.
pub const VENDOR_EGL: &str = if cfg!(target_pointer_width = "64") {
    "/vendor/lib64/egl"
} else {
    "/vendor/lib/egl"
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
/// bionic's default property area path (`__system_properties_init` without `$ATL_PROP_AREA`): the sandbox's `/dev` is the broker's own, so the area is mounted there and no variable is needed.
pub const PROPERTY_AREA: &str = "/dev/__properties__";
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
