// SPDX-License-Identifier: MIT
//! The system property service (plan decision 7a): this runtime's default property set, written as a property area in bionic's own format through `atl_bionic::image`, the file every app process maps.
//! The broker's role, played for now by xtask and test harnesses through the `atl-property-service` binary; atld's live service (setprop, persist.*) comes later.
//!
//! Defaults, by source (a key no AOSP code needs from us stays unset: readers take their own default, e.g. `ro.hw_timeout_multiplier` -> 1, `log.tag.*` -> INFO):
//! - the converter's build.prop (bp2ninja buildprop.go): `ro.build.id`, `ro.build.version.*` from the release config.
//! - `CONSTANTS`: what neither the tree nor the host decides, each with its reason.
//! - arches.toml as the board config, AOSP's board_config.mk rules (build.rs): `ro.product.cpu.abilist`, `abilist32`, `abilist64`.
//! - derived as init derives it when unset: `ro.build.fingerprint` (`fingerprint`).
#![forbid(unsafe_code)]

use std::fmt;

use atl_bionic::image::{AddError, Image};

pub use atl_bionic::AREA_ENV;

mod abis {
    include!(concat!(env!("OUT_DIR"), "/abis.rs"));
}

/// A property: name, value.
pub type Property = (String, String);

/// Values that are this runtime's choice, not derivable from the AOSP tree or the host.
const CONSTANTS: &[(&str, &str)] = &[
    // a release build: ATL is daily-use desktop software. ART doubles its thread-suspend timeouts on "user" (art/runtime/thread_list.cc WaitForSuspendBarrier); Build.TYPE; init's fingerprint
    ("ro.build.type", "user"),
    // what a user build sets (gen_build_prop.py: ro.debuggable=0 without target debugging): liblog __android_log_is_debuggable and libnativehelper JniInvocation (no persist.sys.dalvik.vm.lib.2 override) read it
    ("ro.debuggable", "0"),
    // gen_build_prop.py get_build_keys: "test-keys" only when the product's DefaultAppCertificate is AOSP's public test key; ATL ships nothing signed with it, and apps' root detection reads "test-keys" as a rooted or engineering device
    ("ro.build.tags", "release-keys"),
    // BUILD_NUMBER: a build server's number; ATL numbers no builds yet
    ("ro.build.version.incremental", "0"),
    // Build.BRAND / MANUFACTURER / MODEL / DEVICE / PRODUCT: the runtime, not the host hardware (apps key device quirks on them; no real device's quirks apply)
    ("ro.product.brand", "ATL"),
    ("ro.product.manufacturer", "ATL"),
    ("ro.product.model", "ATL"),
    ("ro.product.device", "atl"),
    ("ro.product.name", "atl"),
    // no boot sequence: an app process starts on a booted system. Android sets it in ActivityManagerService finishBooting
    ("sys.boot_completed", "1"),
    // the NativeBridge core (atl-native-bridge `LIBRARY`): AndroidRuntime::startVm turns it into `-XX:NativeBridge=` for zygote's children, as atl-app-launcher's app mode does
    ("ro.dalvik.vm.native.bridge", "libatl_native_bridge.so"),
    // the device's choice of Codec2 HAL, a vendor property on Android: AIDL (`IComponentStore` through servicemanager, frameworks/av codec2 HalSelection.cpp IsCodec2AidlHalSelected; the aidl_hal flag is on in our release).
    // Unset it reads "hidl", and Codec2Client::CacheServiceNames CHECK-aborts without hwservicemanager, which ATL has none of (measured 2026-11-06, native-load stage 8)
    ("media.c2.hal.selection", "aidl"),
];

/// Why the default set or its area could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A build.prop line that is neither a comment nor `name=value` (1-based line number).
    BuildProp { line: usize, text: String },
    /// bionic's add refused the property (`AddError`: duplicate, value too long, area full, ...).
    Add { name: String, error: AddError },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BuildProp { line, text } => {
                write!(f, "build.prop line {line}: not `name=value`: {text:?}")
            }
            Error::Add { name, error } => write!(f, "property {name}: not added: {error:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// `name=value` lines of a build.prop file, in order; blank lines and `#` comments skipped, name and value trimmed (init `load_properties_from_file`).
/// Only what the converter writes is accepted: `name?=value` (set unless set) and `import` are not.
pub fn parse_build_prop(text: &str) -> Result<Vec<Property>, Error> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || Error::BuildProp {
            line: i + 1,
            text: raw.to_owned(),
        };
        let (name, value) = line.split_once('=').ok_or_else(bad)?;
        let name = name.trim();
        if name.is_empty() || name.ends_with('?') || name.contains(char::is_whitespace) {
            return Err(bad());
        }
        out.push((name.to_owned(), value.trim().to_owned()));
    }
    Ok(out)
}

/// The default set: `build_prop`'s properties, `CONSTANTS`, the ABI lists, then the fingerprint derived from them.
pub fn defaults(build_prop: &str) -> Result<Vec<Property>, Error> {
    let mut props = parse_build_prop(build_prop)?;
    props.extend(
        CONSTANTS
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned())),
    );
    for (name, value) in [
        ("ro.product.cpu.abilist", abis::ABILIST),
        ("ro.product.cpu.abilist32", abis::ABILIST32),
        ("ro.product.cpu.abilist64", abis::ABILIST64),
    ] {
        props.push((name.to_owned(), value.to_owned()));
    }
    let fingerprint = fingerprint(&props);
    props.push(("ro.build.fingerprint".to_owned(), fingerprint));
    Ok(props)
}

/// init `ConstructBuildFingerprint` (system/core/init/property_service.cpp), non-legacy, without the 16 KiB-page developer option:
/// `brand/name/device:release_or_codename/id/incremental:type/tags`, each part "unknown" when unset.
fn fingerprint(props: &[Property]) -> String {
    let get = |name: &str| {
        props
            .iter()
            .find(|(n, _)| n == name)
            .map_or("unknown", |(_, v)| v.as_str())
    };
    format!(
        "{}/{}/{}:{}/{}/{}:{}/{}",
        get("ro.product.brand"),
        get("ro.product.name"),
        get("ro.product.device"),
        get("ro.build.version.release_or_codename"),
        get("ro.build.id"),
        get("ro.build.version.incremental"),
        get("ro.build.type"),
        get("ro.build.tags"),
    )
}

/// The property-area file holding `props`, built by bionic's add in order; the first property bionic refuses fails it.
pub fn area(props: &[Property]) -> Result<Vec<u8>, Error> {
    let mut image = Image::new();
    for (name, value) in props {
        image.add(name, value).map_err(|error| Error::Add {
            name: name.clone(),
            error,
        })?;
    }
    Ok(image.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_bionic::props::{Area, VALUE_MAX, value_len};
    use std::sync::atomic::AtomicU32;

    const BUILD_PROP: &str = "# generated\nro.build.id=BP2A.1\n\nro.build.version.release_or_codename = 16\nro.build.version.sdk=36\n";

    fn words(bytes: &[u8]) -> Vec<AtomicU32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| AtomicU32::new(u32::from_ne_bytes(*c)))
            .collect()
    }

    fn read(area: &Area, name: &str) -> Option<String> {
        let pi = area.find(name.as_bytes())?;
        let mut out = [0u8; VALUE_MAX];
        let len = value_len(area.read(pi, &mut out));
        Some(String::from_utf8_lossy(&out[..len]).into_owned())
    }

    #[test]
    fn build_prop_lines() {
        let p = parse_build_prop(BUILD_PROP).unwrap();
        assert_eq!(
            p[1],
            ("ro.build.version.release_or_codename".into(), "16".into())
        );
        assert_eq!(p.len(), 3);
        for bad in ["novalue", "ro.x?=1", "=1", "a b=1"] {
            assert!(
                matches!(
                    parse_build_prop(&format!("# c\n{bad}\n")),
                    Err(Error::BuildProp { line: 2, .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn defaults_readable_from_the_area() {
        let bytes = area(&defaults(BUILD_PROP).unwrap()).unwrap();
        let w = words(&bytes);
        let a = Area::new(&w).unwrap();
        assert_eq!(read(&a, "ro.build.version.sdk").as_deref(), Some("36"));
        assert_eq!(read(&a, "sys.boot_completed").as_deref(), Some("1"));
        assert_eq!(read(&a, "ro.debuggable").as_deref(), Some("0"));
        // init's layout over the constants above and the fixture's id / release
        assert_eq!(
            read(&a, "ro.build.fingerprint").as_deref(),
            Some("ATL/atl/atl:16/BP2A.1/0:user/release-keys")
        );
        // unset keys stay unset: their readers' defaults apply
        assert_eq!(read(&a, "ro.hw_timeout_multiplier"), None);
    }

    /// board_config.mk over arches.toml on an x86_64 host: CPU ABI x86_64, no 32-bit CPU ABI, bridged 64-bit arm64-v8a then riscv64, bridged 32-bit armeabi-v7a, armeabi, x86 (rows arm, x86).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn abi_lists_on_x86_64() {
        assert_eq!(
            abis::ABILIST,
            "x86_64,arm64-v8a,riscv64,armeabi-v7a,armeabi,x86"
        );
        assert_eq!(abis::ABILIST64, "x86_64,arm64-v8a,riscv64");
        assert_eq!(abis::ABILIST32, "armeabi-v7a,armeabi,x86");
    }

    #[test]
    fn fingerprint_unknown_parts() {
        let props = vec![("ro.product.brand".to_owned(), "b".to_owned())];
        assert_eq!(
            fingerprint(&props),
            "b/unknown/unknown:unknown/unknown/unknown:unknown/unknown"
        );
    }

    #[test]
    fn add_errors_fail_the_area() {
        // a build.prop setting a constant: bionic's add refuses the second
        let dup = format!("{BUILD_PROP}ro.build.type=eng\n");
        assert_eq!(
            area(&defaults(&dup).unwrap()),
            Err(Error::Add {
                name: "ro.build.type".into(),
                error: AddError::Exists
            })
        );
        // a mutable property must fit PROP_VALUE_MAX (92) with its NUL
        let long = vec![("persist.x".to_owned(), "v".repeat(VALUE_MAX))];
        assert_eq!(
            area(&long),
            Err(Error::Add {
                name: "persist.x".into(),
                error: AddError::ValueTooLong
            })
        );
    }
}
