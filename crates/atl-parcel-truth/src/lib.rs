// SPDX-License-Identifier: MIT
//! JNI library of `xtask test parcel` (ParcelTruth.java, xtask-harness/testdata/parcel): `roundTrip(class, bytes, mode)` decodes a Java Parcel's marshalled bytes with atl-parcelables' codec of
//! that class, requires every byte consumed, re-encodes, and returns the bytes; `lastError()` says why it returned null; `refused()` lists atl-parcelables' refusal counters.
//! Bytes enter and leave AOSP's C++ Parcel through libbinder_ndk's AParcel_unmarshal / AParcel_marshal, the libbinder_rs parcel the codecs work on. Test-only, never shipped.
//! `roundTripParcel(class, in, out)` and `rustHasFileDescriptors(class, in)` work on Java `Parcel` objects themselves (AParcel_fromJavaParcel: a view of the Parcel's android::Parcel), since marshalled bytes carry no binder or descriptor objects.
//! JNI is reached through the JNIEnv function table by index (libnativehelper/include_jni/jni.h struct JNINativeInterface, stable since JNI 1.1).
#![cfg(feature = "converter")]
#![allow(non_snake_case)]

// the converter's rlibs, on the crate search path build.rs sets (no cargo dependency names them)
extern crate atl_diag;
extern crate atl_parcelables;
extern crate binder;

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;

#[cfg(test)]
extern crate gui_aidl_types_rs;
#[cfg(test)]
mod capture;
mod compositor;

use atl_parcelables::android::app::ActivityManager_PendingIntentInfo;
use atl_parcelables::android::app::ActivityManager_RunningAppProcessInfo;
use atl_parcelables::android::app::ActivityManager_TaskDescription;
use atl_parcelables::android::app::ApplicationErrorReport_ParcelableCrashInfo;
use atl_parcelables::android::app::ContentProviderHolder;
use atl_parcelables::android::app::NotificationChannel;
use atl_parcelables::android::app::PendingIntent;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, ConfigurationInfo, InstrumentationInfo, PackageInfo,
    ParceledListSlice, PermissionInfo, ProviderInfo, ProviderInfoList, ServiceInfo,
};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::android::content::{ComponentName, Intent, IntentFilter};
use atl_parcelables::android::graphics::Rect;
use atl_parcelables::android::hardware::OverlayProperties;
use atl_parcelables::android::media::{AudioAttributes, PlayerBase_PlayerIdCard};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::android::os::storage::StorageVolume;
use atl_parcelables::android::util::MergedConfiguration;
use atl_parcelables::android::view::inputmethod::{
    EditorInfo, ImeTracker_Token, InputBinding, SurroundingText,
};
use atl_parcelables::android::view::{
    DisplayInfo, InputChannel, InputDevice, InsetsSourceControl_Array, InsetsState, PointerIcon,
    SurfaceControl, WindowManager_LayoutParams, WindowRelayoutResult,
};
use atl_parcelables::android::webkit::WebViewProviderResponse;
use atl_parcelables::android::window::{
    ClientWindowFrames, ImeOnBackInvokedDispatcher, SizeConfigurationBuckets,
};
use atl_parcelables::com::android::internal::infra::AndroidFuture;
use atl_parcelables::com::android::internal::inputmethod::{
    InputBindResult, InputConnectionCommandHeader, InputMethodInfoSafeList,
};
use atl_parcelables::java::io::FileDescriptor;
use atl_parcelables::java::lang::CharSequence;
use atl_parcelables::java::util::Map;
use atl_parcelables::parts::{ParsedIntentInfo, Part};
use binder::binder_impl::{BorrowedParcel, Parcel, UnstructuredParcelable};
use binder::unstable_api::{AParcel, AsNative};

/// `roundTrip` modes: as is, or the re-encoded first byte flipped (ParcelTruth.java's check that a wrong encoding fails the comparison).
const MODE_PLAIN: i32 = 0;
const MODE_CORRUPT: i32 = 1;
/// java.io.FileDescriptor only: the codec writes a descriptor and reads it back (`fd_self_test`).
const MODE_FD_SELF: i32 = 3;
/// android.os.Bundle only: the input holds two marshalled Bundles, A then B; the result is A with `putAll(B)` (`put_all_test`).
const MODE_PUT_ALL: i32 = 4;

unsafe extern "C" {
    fn AParcel_marshal(parcel: *const AParcel, buffer: *mut u8, start: usize, len: usize) -> i32;
    fn AParcel_unmarshal(parcel: *mut AParcel, buffer: *const u8, len: usize) -> i32;
    /// libbinder_ndk (binder_parcel_jni.h): a non-owning AParcel over a Java Parcel's own android::Parcel, so reads and writes act on Java's parcel, binder and descriptor objects included.
    fn AParcel_fromJavaParcel(env: *mut *const *const c_void, parcel: *mut c_void) -> *mut AParcel;
    fn AParcel_delete(parcel: *mut AParcel);
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
    decode_as::<T>(&unmarshal(input)?, mode)
}

fn decode_as<T: UnstructuredParcelable>(p: &Parcel, mode: i32) -> Result<Vec<u8>, String> {
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

/// `round_trip_as` for a layout that is a `Part` but no AIDL parcelable.
fn round_trip_part<T: Part>(input: &[u8], mode: i32) -> Result<Vec<u8>, String> {
    let p = unmarshal(input)?;
    let v = T::read(p.borrowed_ref()).map_err(|e| format!("decode refused: {e:?}"))?;
    if p.get_data_position() != p.get_data_size() {
        return Err(format!(
            "decode stopped at byte {} of {}",
            p.get_data_position(),
            p.get_data_size()
        ));
    }
    let mut out = Parcel::new();
    v.write(&mut out.borrowed())
        .map_err(|e| format!("encode refused: {e:?}"))?;
    finish(&out, mode)
}

/// java.util.Map as AIDL's typed `in Map<String, String>` carries it (no parcelable null flag: null is size -1), so through its Option form.
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

/// Decode with T's codec from the Java parcel `src` (every byte consumed), then encode into `dst`: binder and descriptor objects travel with the bytes.
fn copy_as<T: UnstructuredParcelable>(
    src: &BorrowedParcel<'_>,
    dst: &mut BorrowedParcel<'_>,
) -> Result<(), String> {
    let v = T::from_parcel(src).map_err(|e| format!("decode refused: {e:?}"))?;
    if src.get_data_position() != src.get_data_size() {
        return Err(format!(
            "decode stopped at byte {} of {}",
            src.get_data_position(),
            src.get_data_size()
        ));
    }
    v.write_to_parcel(dst)
        .map_err(|e| format!("encode refused: {e:?}"))
}

/// The codec of `class` over Java parcels (see `Java_ParcelTruth_roundTripParcel`).
fn copy_class(
    class: &str,
    src: &BorrowedParcel<'_>,
    dst: &mut BorrowedParcel<'_>,
) -> Result<(), String> {
    match class {
        "android.os.Bundle" => copy_as::<Bundle>(src, dst),
        "android.content.Intent" => copy_as::<Intent>(src, dst),
        "android.app.PendingIntent" => copy_as::<PendingIntent>(src, dst),
        "android.view.InputChannel" => copy_as::<InputChannel>(src, dst),
        "android.view.SurfaceControl" => copy_as::<SurfaceControl>(src, dst),
        "android.app.servertransaction.ClientTransaction" => copy_as::<ClientTransaction>(src, dst),
        "android.window.ImeOnBackInvokedDispatcher" => {
            copy_as::<ImeOnBackInvokedDispatcher>(src, dst)
        }
        "android.view.inputmethod.ImeTracker$Token" => copy_as::<ImeTracker_Token>(src, dst),
        "android.media.PlayerBase$PlayerIdCard" => copy_as::<PlayerBase_PlayerIdCard>(src, dst),
        _ => Err(format!("no parcel-copy codec for {class}")),
    }
}

/// Whether the codec's decoded `class` value reports file descriptors (Bundle / Intent `has_file_descriptors`).
fn has_file_descriptors(class: &str, src: &BorrowedParcel<'_>) -> Result<bool, String> {
    let refused = |e: binder::StatusCode| format!("decode refused: {e:?}");
    match class {
        "android.os.Bundle" => Bundle::from_parcel(src)
            .map(|b| b.has_file_descriptors())
            .map_err(refused),
        "android.content.Intent" => Intent::from_parcel(src)
            .map(|i| i.has_file_descriptors())
            .map_err(refused),
        _ => Err(format!("no codec with has_file_descriptors for {class}")),
    }
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
/// MODE_FD_SELF: a descriptor the codec writes, then reads back from the parcel that holds the object. Returns [1] when the read
/// consumes exactly what the write put down and yields a descriptor; the read's refusal, if any, is the error.
fn fd_self_test() -> Result<Vec<u8>, String> {
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let mut p = Parcel::new();
    p.write(&FileDescriptor::new(null.into()))
        .map_err(|e| format!("write refused: {e:?}"))?;
    let size = p.get_data_size();
    p.set_data_position_checked(0)
        .map_err(|e| format!("rewind: {e:?}"))?;
    let got: FileDescriptor = p
        .borrowed_ref()
        .read()
        .map_err(|e| format!("read refused: {e:?}"))?;
    if p.get_data_position() != size {
        return Err(format!(
            "read stopped at byte {} of {size}",
            p.get_data_position()
        ));
    }
    if got.fd.is_none() {
        return Err("read gave no descriptor".to_owned());
    }
    Ok(vec![1])
}

/// Bundle.putAll on the codec's typed entries: A and B decoded in turn from one parcel, A.put_all(B), encoded.
fn put_all_test(input: &[u8]) -> Result<Vec<u8>, String> {
    let p = unmarshal(input)?;
    let mut a = Bundle::from_parcel(p.borrowed_ref()).map_err(|e| format!("A refused: {e:?}"))?;
    let b = Bundle::from_parcel(p.borrowed_ref()).map_err(|e| format!("B refused: {e:?}"))?;
    if p.get_data_position() != p.get_data_size() {
        return Err(format!(
            "decode stopped at byte {} of {}",
            p.get_data_position(),
            p.get_data_size()
        ));
    }
    a.put_all(&b)
        .map_err(|e| format!("put_all refused: {e:?}"))?;
    let mut out = Parcel::new();
    a.write_to_parcel(&mut out.borrowed())
        .map_err(|e| format!("encode refused: {e:?}"))?;
    finish(&out, MODE_PLAIN)
}

fn round_trip(class: &str, input: &[u8], mode: i32) -> Result<Vec<u8>, String> {
    if mode == MODE_PUT_ALL {
        return match class {
            "android.os.Bundle" => put_all_test(input),
            _ => Err(format!("MODE_PUT_ALL merges Bundles, not {class}")),
        };
    }
    if mode == MODE_FD_SELF {
        return match class {
            "java.io.FileDescriptor" => fd_self_test(),
            _ => Err(format!(
                "MODE_FD_SELF tests java.io.FileDescriptor, not {class}"
            )),
        };
    }
    match class {
        "android.content.pm.ApplicationInfo" => round_trip_as::<ApplicationInfo>(input, mode),
        "android.content.pm.ActivityInfo" => round_trip_as::<ActivityInfo>(input, mode),
        "android.content.pm.ProviderInfo" => round_trip_as::<ProviderInfo>(input, mode),
        "android.content.pm.ProviderInfoList" => round_trip_as::<ProviderInfoList>(input, mode),
        "android.content.pm.PackageInfo" => round_trip_as::<PackageInfo>(input, mode),
        "android.content.pm.ServiceInfo" => round_trip_as::<ServiceInfo>(input, mode),
        "android.content.pm.PermissionInfo" => round_trip_as::<PermissionInfo>(input, mode),
        // not an AIDL parcelable: the installer's intents frames carry it (atl-parcelables parts.rs)
        "com.android.internal.pm.pkg.component.ParsedIntentInfoImpl" => {
            round_trip_part::<ParsedIntentInfo>(input, mode)
        }
        "android.content.pm.ParceledListSlice" => round_trip_as::<ParceledListSlice>(input, mode),
        "android.app.NotificationChannel" => round_trip_as::<NotificationChannel>(input, mode),
        "android.content.pm.InstrumentationInfo" => {
            round_trip_as::<InstrumentationInfo>(input, mode)
        }
        "android.content.res.Configuration" => round_trip_as::<Configuration>(input, mode),
        "android.content.res.CompatibilityInfo" => round_trip_as::<CompatibilityInfo>(input, mode),
        "android.content.Intent" => round_trip_as::<Intent>(input, mode),
        "android.content.IntentFilter" => round_trip_as::<IntentFilter>(input, mode),
        "android.content.ComponentName" => round_trip_as::<ComponentName>(input, mode),
        "android.content.pm.ConfigurationInfo" => round_trip_as::<ConfigurationInfo>(input, mode),
        "android.webkit.WebViewProviderResponse" => {
            round_trip_as::<WebViewProviderResponse>(input, mode)
        }
        "android.os.Bundle" => round_trip_as::<Bundle>(input, mode),
        "android.app.servertransaction.ClientTransaction" => {
            round_trip_as::<ClientTransaction>(input, mode)
        }
        "android.app.ContentProviderHolder" => round_trip_as::<ContentProviderHolder>(input, mode),
        "android.app.PendingIntent" => round_trip_as::<PendingIntent>(input, mode),
        "android.app.ActivityManager$PendingIntentInfo" => {
            round_trip_as::<ActivityManager_PendingIntentInfo>(input, mode)
        }
        "android.app.ActivityManager$RunningAppProcessInfo" => {
            round_trip_as::<ActivityManager_RunningAppProcessInfo>(input, mode)
        }
        "android.app.ActivityManager$TaskDescription" => {
            round_trip_as::<ActivityManager_TaskDescription>(input, mode)
        }
        "android.app.ApplicationErrorReport$ParcelableCrashInfo" => {
            round_trip_as::<ApplicationErrorReport_ParcelableCrashInfo>(input, mode)
        }
        "android.view.DisplayInfo" => round_trip_as::<DisplayInfo>(input, mode),
        "android.graphics.Rect" => round_trip_as::<Rect>(input, mode),
        "android.hardware.OverlayProperties" => round_trip_as::<OverlayProperties>(input, mode),
        "android.media.AudioAttributes" => round_trip_as::<AudioAttributes>(input, mode),
        "android.media.PlayerBase$PlayerIdCard" => {
            round_trip_as::<PlayerBase_PlayerIdCard>(input, mode)
        }
        "com.android.internal.inputmethod.InputBindResult" => {
            round_trip_as::<InputBindResult>(input, mode)
        }
        "com.android.internal.inputmethod.InputConnectionCommandHeader" => {
            round_trip_as::<InputConnectionCommandHeader>(input, mode)
        }
        "android.view.inputmethod.InputBinding" => round_trip_as::<InputBinding>(input, mode),
        "java.lang.CharSequence" => round_trip_as::<CharSequence>(input, mode),
        "android.view.inputmethod.SurroundingText" => round_trip_as::<SurroundingText>(input, mode),
        "com.android.internal.infra.AndroidFuture" => round_trip_as::<AndroidFuture>(input, mode),
        "com.android.internal.inputmethod.InputMethodInfoSafeList" => {
            round_trip_as::<InputMethodInfoSafeList>(input, mode)
        }
        "android.os.storage.StorageVolume" => round_trip_as::<StorageVolume>(input, mode),
        "android.view.WindowManager$LayoutParams" => {
            round_trip_as::<WindowManager_LayoutParams>(input, mode)
        }
        "android.view.InsetsState" => round_trip_as::<InsetsState>(input, mode),
        "android.view.InsetsSourceControl$Array" => {
            round_trip_as::<InsetsSourceControl_Array>(input, mode)
        }
        "android.view.SurfaceControl" => round_trip_as::<SurfaceControl>(input, mode),
        "android.window.ClientWindowFrames" => round_trip_as::<ClientWindowFrames>(input, mode),
        "android.util.MergedConfiguration" => round_trip_as::<MergedConfiguration>(input, mode),
        "android.window.SizeConfigurationBuckets" => {
            round_trip_as::<SizeConfigurationBuckets>(input, mode)
        }
        "android.view.WindowRelayoutResult" => round_trip_as::<WindowRelayoutResult>(input, mode),
        "android.view.InputDevice" => round_trip_as::<InputDevice>(input, mode),
        "android.view.PointerIcon" => round_trip_as::<PointerIcon>(input, mode),
        "android.view.inputmethod.EditorInfo" => round_trip_as::<EditorInfo>(input, mode),
        "java.util.Map" => round_trip_map(input, mode),
        _ => Err(format!("no codec for {class}")),
    }
}

// ---- JNI ---------------------------------------------------------------------------------------

/// `JNIEnv*`: a pointer to the function table pointer.
pub(crate) type JniEnv = *mut *const *const c_void;
pub(crate) type JObject = *mut c_void;

/// JNINativeInterface slots (jni.h order, counted from the struct's first member `reserved0`).
const NEW_STRING_UTF: usize = 167;
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;
const GET_ARRAY_LENGTH: usize = 171;
/// `GetObjectArrayElement` (jni.h: NewObjectArray is 172, this 173).
pub(crate) const GET_OBJECT_ARRAY_ELEMENT: usize = 173;
const NEW_BYTE_ARRAY: usize = 176;
const GET_BYTE_ARRAY_REGION: usize = 200;
const SET_BYTE_ARRAY_REGION: usize = 208;

/// Slot `i` of the calling thread's function table.
///
/// # Safety
/// `env` is the JNIEnv ART passed to a native method of this thread.
pub(crate) unsafe fn slot(env: JniEnv, i: usize) -> *const c_void {
    // SAFETY: the table has every JNI 1.6 slot (NEW_STRING_UTF .. SET_BYTE_ARRAY_REGION are below its end)
    unsafe { *(*env).add(i) }
}

/// A Java String as Rust; None for null.
///
/// # Safety
/// `env` as for `slot`; `s` a local reference to a java.lang.String or null.
pub(crate) unsafe fn string(env: JniEnv, s: JObject) -> Option<String> {
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
pub(crate) unsafe fn bytes(env: JniEnv, a: JObject) -> Option<Vec<u8>> {
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
pub(crate) unsafe fn byte_array(env: JniEnv, b: &[u8]) -> JObject {
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
pub(crate) unsafe fn java_string(env: JniEnv, s: &str) -> JObject {
    let c = CString::new(s.replace('\0', " ")).unwrap_or_default();
    // SAFETY: slot 167 is NewStringUTF of this signature; `c` is NUL-terminated
    unsafe {
        let new: unsafe extern "C" fn(JniEnv, *const c_char) -> JObject =
            std::mem::transmute(slot(env, NEW_STRING_UTF));
        new(env, c.as_ptr())
    }
}

pub(crate) fn set_error(e: String) {
    *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = e;
}

/// The last error `set_error` recorded (every truth driver reads it through its own `lastError`).
pub(crate) fn last_error() -> String {
    LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// `static native byte[] roundTrip(String className, byte[] marshalled, int mode)`: null on failure (`lastError`).
///
/// # Safety
/// `ParcelTruth.setKeyCharacterMap(byte[])`: the map every InputDevice codec read compares against. 0 when set, else -1 with the reason in `lastError`.
///
/// # Safety
/// Called by ART as that native method: `env` is the JNIEnv of this thread's call, `map` a local reference valid for it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_setKeyCharacterMap(
    env: JniEnv,
    _class: JObject,
    map: JObject,
) -> i32 {
    // SAFETY: ART's argument to this native method
    let Some(map) = (unsafe { bytes(env, map) }) else {
        set_error("null argument".to_owned());
        return -1;
    };
    match atl_parcelables::codecs::android::view::InputDevice::set_key_character_map(&map) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e.to_owned());
            -1
        }
    }
}

/// Called by ART as that native method.
///
/// # Safety
/// `env` is the JNIEnv of this thread's native call; the object arguments are local references valid for it.
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

/// Runs `f` on a view of the Java Parcel `a` (AParcel_fromJavaParcel): the Parcel's own data and object table are read and written, not a copy. `Err` if `a` is not a Parcel.
///
/// # Safety
/// `env` is the JNIEnv ART passed to this thread's native method; `a` is a local reference to an android.os.Parcel (or null) that stays valid for the call, and nothing else uses the Parcel meanwhile.
unsafe fn with_java_parcel<T>(
    env: JniEnv,
    a: JObject,
    f: impl FnOnce(&mut BorrowedParcel<'_>) -> Result<T, String>,
) -> Result<T, String> {
    // SAFETY: the caller's guarantees; the view is non-owning and deleted below, before the reference ends
    let pa = unsafe { AParcel_fromJavaParcel(env, a) };
    // SAFETY: the view is null or a valid AParcel; the caller owns the Parcel exclusively for the call
    let result = match unsafe { BorrowedParcel::from_raw(pa) } {
        Some(mut view) => f(&mut view),
        None => Err("not an android.os.Parcel".to_owned()),
    };
    // SAFETY: made above and not used after this (deleting null is a no-op)
    unsafe { AParcel_delete(pa) };
    result
}

/// `static native int roundTripParcel(String className, Parcel in, Parcel out)`: decodes the value at `in`'s position with the codec of `className`, every byte of `in` consumed, and encodes it into `out`;
/// 0 = done, -1 = failed (`lastError`). Unlike `roundTrip`, the Parcels are Java's own, so binder and file descriptor objects reach the codec and come back out.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_roundTripParcel(
    env: JniEnv,
    _class: JObject,
    class: JObject,
    input: JObject,
    output: JObject,
) -> i32 {
    // SAFETY: ART's arguments to this native method
    let class = unsafe { string(env, class) };
    let Some(class) = class else {
        set_error("null class".to_owned());
        return -1;
    };
    // SAFETY: as above; `input` and `output` are two different Parcels
    let result = unsafe {
        with_java_parcel(env, input, |src| {
            with_java_parcel(env, output, |dst| copy_class(&class, src, dst))
        })
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            set_error(e);
            -1
        }
    }
}

/// `static native int rustHasFileDescriptors(String className, Parcel in)`: the codec's `has_file_descriptors` of the value decoded at `in`'s position: 1 or 0, -1 = failed (`lastError`).
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_rustHasFileDescriptors(
    env: JniEnv,
    _class: JObject,
    class: JObject,
    input: JObject,
) -> i32 {
    // SAFETY: ART's arguments to this native method
    let class = unsafe { string(env, class) };
    let Some(class) = class else {
        set_error("null class".to_owned());
        return -1;
    };
    // SAFETY: as above
    let result = unsafe { with_java_parcel(env, input, |src| has_file_descriptors(&class, src)) };
    match result {
        Ok(has) => i32::from(has),
        Err(e) => {
            set_error(e);
            -1
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
    // SAFETY: the caller's guarantee
    unsafe { start_reg(env) }
}

/// What every truth driver's `startReg` does.
///
/// # Safety
/// `env` is the JNIEnv ART passed to a native method of this thread.
pub(crate) unsafe fn start_reg(env: JniEnv) -> i32 {
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

/// `static native String refused()`: atl-parcelables' refusal counts (atl-diag `refused-parcel`), `name=count` per line.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_ParcelTruth_refused(env: JniEnv, _class: JObject) -> JObject {
    let s: String = atl_diag::counts()
        .iter()
        .filter(|(outcome, _, _)| outcome == atl_diag::REFUSED_PARCEL)
        .map(|(_, k, v)| format!("{k}={v}\n"))
        .collect();
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &s) }
}
