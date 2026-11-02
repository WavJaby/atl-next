// SPDX-License-Identifier: MIT
//! JNI library of `xtask test parcel` (ParcelTruth.java, xtask/testdata/parcel): `roundTrip(class, bytes, mode)` decodes a Java Parcel's marshalled bytes with atl-parcelables' codec of
//! that class, requires every byte consumed, re-encodes, and returns the bytes; `lastError()` says why it returned null; `refused()` lists atl-parcelables' refusal counters.
//! Bytes enter and leave AOSP's C++ Parcel through libbinder_ndk's AParcel_unmarshal / AParcel_marshal, the libbinder_rs parcel the codecs work on. Test-only, never shipped.
//! JNI is reached through the JNIEnv function table by index (libnativehelper/include_jni/jni.h struct JNINativeInterface, stable since JNI 1.1).
#![cfg(feature = "converter")]
#![allow(non_snake_case)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate atl_parcelables;
extern crate binder;

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;

use atl_parcelables::android::app::ContentProviderHolder;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, PackageInfo, ProviderInfo, ProviderInfoList,
};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::java::util::Map;
use binder::binder_impl::{Parcel, UnstructuredParcelable};
use binder::unstable_api::{AParcel, AsNative};

/// `roundTrip` modes: as is, or the re-encoded first byte flipped (ParcelTruth.java's check that a wrong encoding fails the comparison).
const MODE_PLAIN: i32 = 0;
const MODE_CORRUPT: i32 = 1;

unsafe extern "C" {
    fn AParcel_marshal(parcel: *const AParcel, buffer: *mut u8, start: usize, len: usize) -> i32;
    fn AParcel_unmarshal(parcel: *mut AParcel, buffer: *const u8, len: usize) -> i32;
}

static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn marshal(p: &Parcel) -> Result<Vec<u8>, String> {
    let n = usize::try_from(p.get_data_size()).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; n];
    // SAFETY: `p` is a live parcel; `buf` holds `n` bytes, and [0, n) is the parcel's whole data
    let st = unsafe { AParcel_marshal(p.as_native(), buf.as_mut_ptr(), 0, n) };
    if st != 0 {
        return Err(format!("AParcel_marshal: {st}"));
    }
    Ok(buf)
}

fn unmarshal(bytes: &[u8]) -> Result<Parcel, String> {
    let mut p = Parcel::new();
    // SAFETY: `p` is a live parcel; `bytes` is a readable slice of its length
    let st = unsafe { AParcel_unmarshal(p.as_native_mut(), bytes.as_ptr(), bytes.len()) };
    if st != 0 {
        return Err(format!("AParcel_unmarshal: {st}"));
    }
    // SAFETY: position 0 lies inside every parcel (libbinder_rs's own ParcelableHolder relies on the same)
    unsafe { p.set_data_position(0) }.map_err(|e| format!("set_data_position: {e:?}"))?;
    Ok(p)
}

/// Decode with T's codec, every byte consumed, then re-encode.
fn round_trip_as<T: UnstructuredParcelable>(input: &[u8], mode: i32) -> Result<Vec<u8>, String> {
    let p = unmarshal(input)?;
    let v = T::from_parcel(p.borrowed_ref()).map_err(|e| format!("decode refused: {e:?}"))?;
    if p.get_data_position() != p.get_data_size() {
        return Err(format!(
            "decode stopped at byte {} of {}",
            p.get_data_position(),
            p.get_data_size()
        ));
    }
    let mut out = Parcel::new();
    v.write_to_parcel(&mut out.borrowed())
        .map_err(|e| format!("encode refused: {e:?}"))?;
    finish(&out, mode)
}

/// java.util.Map as Parcel.writeMap writes it (no parcelable null flag: null is size -1), so through its Option form.
fn round_trip_map(input: &[u8], mode: i32) -> Result<Vec<u8>, String> {
    let p = unmarshal(input)?;
    let v: Option<Map> = p.read().map_err(|e| format!("decode refused: {e:?}"))?;
    if p.get_data_position() != p.get_data_size() {
        return Err(format!(
            "decode stopped at byte {} of {}",
            p.get_data_position(),
            p.get_data_size()
        ));
    }
    let mut out = Parcel::new();
    out.write(&v)
        .map_err(|e| format!("encode refused: {e:?}"))?;
    finish(&out, mode)
}

/// The re-encoded bytes, corrupted in MODE_CORRUPT.
fn finish(out: &Parcel, mode: i32) -> Result<Vec<u8>, String> {
    let mut bytes = marshal(out)?;
    if mode == MODE_CORRUPT {
        if let Some(b) = bytes.first_mut() {
            *b ^= 1;
        }
    } else if mode != MODE_PLAIN {
        return Err(format!("unknown mode {mode}"));
    }
    Ok(bytes)
}

/// The codec of Java class `class`.
fn round_trip(class: &str, input: &[u8], mode: i32) -> Result<Vec<u8>, String> {
    match class {
        "android.content.pm.ApplicationInfo" => round_trip_as::<ApplicationInfo>(input, mode),
        "android.content.pm.ActivityInfo" => round_trip_as::<ActivityInfo>(input, mode),
        "android.content.pm.ProviderInfo" => round_trip_as::<ProviderInfo>(input, mode),
        "android.content.pm.ProviderInfoList" => round_trip_as::<ProviderInfoList>(input, mode),
        "android.content.pm.PackageInfo" => round_trip_as::<PackageInfo>(input, mode),
        "android.content.res.Configuration" => round_trip_as::<Configuration>(input, mode),
        "android.content.res.CompatibilityInfo" => round_trip_as::<CompatibilityInfo>(input, mode),
        "android.content.Intent" => round_trip_as::<Intent>(input, mode),
        "android.app.servertransaction.ClientTransaction" => {
            round_trip_as::<ClientTransaction>(input, mode)
        }
        "android.app.ContentProviderHolder" => round_trip_as::<ContentProviderHolder>(input, mode),
        "java.util.Map" => round_trip_map(input, mode),
        _ => Err(format!("no codec for {class}")),
    }
}

// ---- JNI ---------------------------------------------------------------------------------------

/// `JNIEnv*`: a pointer to the function table pointer.
type JniEnv = *mut *const *const c_void;
type JObject = *mut c_void;

/// JNINativeInterface slots (jni.h order, counted from the struct's first member `reserved0`).
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;
const GET_ARRAY_LENGTH: usize = 171;
const NEW_BYTE_ARRAY: usize = 176;
const GET_BYTE_ARRAY_REGION: usize = 200;
const SET_BYTE_ARRAY_REGION: usize = 208;

/// Slot `i` of the calling thread's function table.
///
/// # Safety
/// `env` is the JNIEnv ART passed to a native method of this thread.
unsafe fn slot(env: JniEnv, i: usize) -> *const c_void {
    // SAFETY: the table has every JNI 1.6 slot (NEW_STRING_UTF .. SET_BYTE_ARRAY_REGION are below its end)
    unsafe { *(*env).add(i) }
}

/// A Java String as Rust; None for null.
///
/// # Safety
/// `env` as for `slot`; `s` a local reference to a java.lang.String or null.
unsafe fn string(env: JniEnv, s: JObject) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: slots 169 / 170 are GetStringUTFChars / ReleaseStringUTFChars of these signatures
    unsafe {
        let get: unsafe extern "C" fn(JniEnv, JObject, *mut u8) -> *const c_char =
            std::mem::transmute(slot(env, GET_STRING_UTF_CHARS));
        let release: unsafe extern "C" fn(JniEnv, JObject, *const c_char) =
            std::mem::transmute(slot(env, RELEASE_STRING_UTF_CHARS));
        let chars = get(env, s, std::ptr::null_mut());
        if chars.is_null() {
            return None;
        }
        let out = CStr::from_ptr(chars).to_string_lossy().into_owned();
        release(env, s, chars);
        Some(out)
    }
}

/// A Java byte[] as Rust; None for null.
///
/// # Safety
/// `env` as for `slot`; `a` a local reference to a byte[] or null.
unsafe fn bytes(env: JniEnv, a: JObject) -> Option<Vec<u8>> {
    if a.is_null() {
        return None;
    }
    // SAFETY: slots 171 / 200 are GetArrayLength / GetByteArrayRegion of these signatures; the region [0, len) is the array's
    unsafe {
        let len: unsafe extern "C" fn(JniEnv, JObject) -> i32 =
            std::mem::transmute(slot(env, GET_ARRAY_LENGTH));
        let region: unsafe extern "C" fn(JniEnv, JObject, i32, i32, *mut i8) =
            std::mem::transmute(slot(env, GET_BYTE_ARRAY_REGION));
        let n = len(env, a);
        let mut out = vec![0u8; usize::try_from(n).ok()?];
        region(env, a, 0, n, out.as_mut_ptr().cast());
        Some(out)
    }
}

/// A new Java byte[] holding `b`; null if ART could not allocate it (with an OutOfMemoryError pending).
///
/// # Safety
/// `env` as for `slot`.
unsafe fn byte_array(env: JniEnv, b: &[u8]) -> JObject {
    let Ok(n) = i32::try_from(b.len()) else {
        return std::ptr::null_mut();
    };
    // SAFETY: slots 176 / 208 are NewByteArray / SetByteArrayRegion of these signatures; the region [0, n) is the new array's
    unsafe {
        let new: unsafe extern "C" fn(JniEnv, i32) -> JObject =
            std::mem::transmute(slot(env, NEW_BYTE_ARRAY));
        let set: unsafe extern "C" fn(JniEnv, JObject, i32, i32, *const i8) =
            std::mem::transmute(slot(env, SET_BYTE_ARRAY_REGION));
        let a = new(env, n);
        if !a.is_null() {
            set(env, a, 0, n, b.as_ptr().cast());
        }
        a
    }
}

/// A new Java String of `s` (interior NULs cut).
///
/// # Safety
/// `env` as for `slot`.
unsafe fn java_string(env: JniEnv, s: &str) -> JObject {
    let c = CString::new(s.replace('\0', " ")).unwrap_or_default();
    // SAFETY: slot 167 is NewStringUTF of this signature; `c` is NUL-terminated
    unsafe {
        let new: unsafe extern "C" fn(JniEnv, *const c_char) -> JObject =
            std::mem::transmute(slot(env, NEW_STRING_UTF));
        new(env, c.as_ptr())
    }
}

fn set_error(e: String) {
    *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = e;
}

/// `static native byte[] roundTrip(String className, byte[] marshalled, int mode)`: null on failure (`lastError`).
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_roundTrip(
    env: JniEnv,
    _class: JObject,
    class: JObject,
    input: JObject,
    mode: i32,
) -> JObject {
    // SAFETY: ART's arguments to this native method
    let (class, input) = unsafe { (string(env, class), bytes(env, input)) };
    let (Some(class), Some(input)) = (class, input) else {
        set_error("null argument".to_owned());
        return std::ptr::null_mut();
    };
    match round_trip(&class, &input, mode) {
        // SAFETY: as above
        Ok(out) => unsafe { byte_array(env, &out) },
        Err(e) => {
            set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// libandroid_runtime's soname and `android::AndroidRuntime::startReg(JNIEnv*)` (a static member: the C ABI of `int(JNIEnv*)`), Itanium-mangled.
const RUNTIME: &CStr = c"libandroid_runtime.so";
const START_REG: &CStr = c"_ZN7android14AndroidRuntime8startRegEP7_JNIEnv";
const RTLD_NOW: i32 = 2;
const RTLD_NOLOAD: i32 = 4;

unsafe extern "C" {
    fn dlopen(file: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

/// `static native int startReg()`: registers the framework's natives (Parcel, SystemClock, ...) as app_process does at start; libandroid_runtime must already be loaded (System.load). -100 = not found.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_startReg(env: JniEnv, _class: JObject) -> i32 {
    // SAFETY: NUL-terminated constants; RTLD_NOLOAD only looks up an already loaded object
    let sym = unsafe {
        let h = dlopen(RUNTIME.as_ptr(), RTLD_NOW | RTLD_NOLOAD);
        if h.is_null() {
            return -100;
        }
        dlsym(h, START_REG.as_ptr())
    };
    if sym.is_null() {
        return -100;
    }
    // SAFETY: the symbol is AndroidRuntime::startReg, int(JNIEnv*); env is this thread's JNIEnv
    unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(JniEnv) -> i32>(sym)(env) }
}

/// `static native String lastError()`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_lastError(env: JniEnv, _class: JObject) -> JObject {
    let e = LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone();
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &e) }
}

/// `static native String refused()`: atl-parcelables' refusal counters, `name=count` per line.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_refused(env: JniEnv, _class: JObject) -> JObject {
    let s: String = atl_parcelables::refused()
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &s) }
}
