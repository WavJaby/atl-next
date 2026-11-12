// SPDX-License-Identifier: MIT
//! Host sides of the NDK functions that take a `JNIEnv*` (their rows of build.rs `PROXY_SHIMS`). The guest passes its own env (jni.rs `GuestObj`), so the generic crossing would hand the host library a guest env: each shim maps it to the host env (jni.rs `host_env`) and calls the same function of the host library of its soname (bindings.rs `system_symbol`).
//! The other arguments and the results are host values already: object references are the host's, and an AAssetManager or a pixel pointer names host memory the guest reads in the same address space.

use std::ffi::c_void;

use super::{bindings, jni, unsupported};

/// The NDK functions' host libraries: the system libraries of their sonames (SYSTEM_LIB).
const LIBANDROID: &str = "libandroid.so";
const LIBJNIGRAPHICS: &str = "libjnigraphics.so";

/// `name` of the host library `soname`, as the NDK header declares it (the caller types it); aborts when the host library lacks it.
fn host_fn(soname: &str, name: &str) -> usize {
    bindings::system_symbol(soname, name)
        .unwrap_or_else(|| unsupported(&format!("{name}: NDK shim with no {soname} in SYSTEM_LIB")))
}

/// The host `JNIEnv*` standing behind the guest's `env`.
fn to_host_env(env: *mut c_void) -> *mut c_void {
    // SAFETY: `env` is the guest's own `JNIEnv*` (made by jni.rs `guest_env`), as an NDK function receives it.
    unsafe { jni::host_env(env as usize) as *mut c_void }
}

/// `AAssetManager* AAssetManager_fromJava(JNIEnv* env, jobject assetManager)`.
pub(super) extern "C" fn asset_manager_from_java(
    env: *mut c_void,
    asset_manager: *mut c_void,
) -> *mut c_void {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    // SAFETY: the host libandroid's export of this name; its prototype is asset_manager_jni.h's.
    let f: F = unsafe { std::mem::transmute(host_fn(LIBANDROID, "AAssetManager_fromJava")) };
    // SAFETY: the host env and the guest's jobject, which is the host's reference.
    unsafe { f(to_host_env(env), asset_manager) }
}

/// `int AndroidBitmap_getInfo(JNIEnv* env, jobject jbitmap, AndroidBitmapInfo* info)`.
pub(super) extern "C" fn bitmap_get_info(
    env: *mut c_void,
    bitmap: *mut c_void,
    info: *mut c_void,
) -> i32 {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
    // SAFETY: the host libjnigraphics' export of this name; its prototype is bitmap.h's.
    let f: F = unsafe { std::mem::transmute(host_fn(LIBJNIGRAPHICS, "AndroidBitmap_getInfo")) };
    // SAFETY: the host env, the guest's jobject and the guest's info struct, all in the one address space.
    unsafe { f(to_host_env(env), bitmap, info) }
}

/// `int AndroidBitmap_lockPixels(JNIEnv* env, jobject jbitmap, void** addrPtr)`.
pub(super) extern "C" fn bitmap_lock_pixels(
    env: *mut c_void,
    bitmap: *mut c_void,
    addr: *mut c_void,
) -> i32 {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
    // SAFETY: the host libjnigraphics' export of this name; its prototype is bitmap.h's.
    let f: F = unsafe { std::mem::transmute(host_fn(LIBJNIGRAPHICS, "AndroidBitmap_lockPixels")) };
    // SAFETY: the host env, the guest's jobject and out-parameter.
    unsafe { f(to_host_env(env), bitmap, addr) }
}

/// `int AndroidBitmap_unlockPixels(JNIEnv* env, jobject jbitmap)`.
pub(super) extern "C" fn bitmap_unlock_pixels(env: *mut c_void, bitmap: *mut c_void) -> i32 {
    type F = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
    // SAFETY: the host libjnigraphics' export of this name; its prototype is bitmap.h's.
    let f: F =
        unsafe { std::mem::transmute(host_fn(LIBJNIGRAPHICS, "AndroidBitmap_unlockPixels")) };
    // SAFETY: the host env and the guest's jobject.
    unsafe { f(to_host_env(env), bitmap) }
}
