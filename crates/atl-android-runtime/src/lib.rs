// SPDX-License-Identifier: MIT
//! The `register_*` functions of the `libandroid_runtime` sources ATL does not build (providers.toml row `libandroid_runtime`, `srcs`).
//! `AndroidRuntime.cpp` calls each from its `gRegJNI` table and fails start-up on a negative result; these return 0 and register no natives, so a Java call to one of those natives throws `UnsatisfiedLinkError`.
//!
//! - `android_hardware_camera2_DngCreator.cpp`: links Adobe's dng_sdk, whose patent licence covers DNG use only (field of use), which the GPL-3 QEMU bridge in the same process cannot accept.
//!   `android.hardware.camera2.DngCreator` stays public API; its static initializer calls `nativeClassInit`, so the class fails to initialise.
//! - `com_android_internal_os_Zygote*.cpp`: no zygote in v1, atld starts every app process.
//!
//! Boundary crate (C ABI exports): the only `unsafe` is `export_name`, giving the C++ (Itanium-mangled) names AndroidRuntime.cpp's declarations link against.
#![deny(unsafe_code)]

use core::ffi::{c_int, c_void};

/// Success without registering anything; `env` (a `JNIEnv*`) is not read.
fn registered(_env: *mut c_void) -> c_int {
    0
}

/// One export per registration, as AndroidRuntime.cpp declares it (`extern int register_X(JNIEnv*)`): Rust name, C++ namespace, C++ name => mangled symbol.
/// The test re-derives each symbol from namespace and name, so the table cannot drift from the attribute.
macro_rules! registrations {
    ($($rust:ident: $namespace:expr, $name:literal => $symbol:literal;)*) => {
        #[allow(unsafe_code)]
        mod exports {
            use core::ffi::{c_int, c_void};
            $(
                // SAFETY: a name nothing else in the process defines (the sources defining it are not built); the signature is the declaration's, `int(JNIEnv*)`, whose C++ ABI is the C one.
                #[unsafe(export_name = $symbol)]
                extern "C" fn $rust(env: *mut c_void) -> c_int {
                    super::registered(env)
                }
            )*
        }

        #[cfg(test)]
        const REGISTRATIONS: &[(Option<&str>, &str, &str)] = &[$(($namespace, $name, $symbol)),*];
    };
}

registrations! {
    // declared before AndroidRuntime.cpp opens `namespace android`
    register_android_hardware_camera2_dng_creator: None, "register_android_hardware_camera2_DngCreator"
        => "_Z44register_android_hardware_camera2_DngCreatorP7_JNIEnv";
    register_com_android_internal_os_zygote: Some("android"), "register_com_android_internal_os_Zygote"
        => "_ZN7android39register_com_android_internal_os_ZygoteEP7_JNIEnv";
    register_com_android_internal_os_zygote_command_buffer: Some("android"), "register_com_android_internal_os_ZygoteCommandBuffer"
        => "_ZN7android52register_com_android_internal_os_ZygoteCommandBufferEP7_JNIEnv";
    register_com_android_internal_os_zygote_init: Some("android"), "register_com_android_internal_os_ZygoteInit"
        => "_ZN7android43register_com_android_internal_os_ZygoteInitEP7_JNIEnv";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Itanium C++ ABI 5.1 (mangling): `_Z` + (`N` <len><namespace> <len><name> `E` | <len><name>) + parameter types; `JNIEnv*` = `P` + `7_JNIEnv` (jni.h: `typedef _JNIEnv JNIEnv` in C++).
    fn mangle(namespace: Option<&str>, name: &str) -> String {
        let source = |s: &str| format!("{}{s}", s.len());
        let qualified = match namespace {
            Some(ns) => format!("N{}{}E", source(ns), source(name)),
            None => source(name),
        };
        format!("_Z{qualified}P{}", source("_JNIEnv"))
    }

    #[test]
    fn mangled_names_match_the_declarations() {
        for (namespace, name, symbol) in REGISTRATIONS {
            assert_eq!(&mangle(*namespace, name), symbol, "{name}");
        }
    }

    #[test]
    fn registration_succeeds() {
        // AndroidRuntime.cpp register_jni_procs: a negative result aborts start-up
        assert_eq!(registered(core::ptr::null_mut()), 0);
    }
}
