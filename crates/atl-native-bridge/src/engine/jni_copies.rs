// SPDX-License-Identifier: MIT
//! JNI buffers ART hands a 32-bit guest (jni.h `Get<Type>ArrayElements`, `GetStringUTFChars`, `GetStringChars`, `GetStringCritical`, with their `Release*`): ART's copy is host heap (`new uint64_t[]` / `new jchar[]` / `new char[]`, jni_internal.cc), above 4 GiB, which the guest's word cannot hold. The guest gets a window copy of it (`Active::reserve`, below 4 GiB) and `*isCopy = JNI_TRUE`; the window stands for ART's copy until the matching release, which copies the window back into ART's copy (array modes 0 and `JNI_COMMIT`), hands ART its own pointer with the guest's mode, and gives the window back (every mode but `JNI_COMMIT`). ART's copy-back and free rules stay ART's: it only ever sees its own pointer.
//! A pointer that already fits the guest word passes both ways unchanged: ART's heap objects lie below 4 GiB (32-bit compressed references), so a direct pointer to array or string data (`isCopy` false; ART's choice for a non-movable object, `GetPrimitiveArrayCritical`, an uncompressed `GetStringCritical`) needs no copy. Hence no special for `GetPrimitiveArrayCritical` / `GetDirectBufferAddress`: the object stub's result narrowing (dispatch.rs `ilp32_narrow`) passes such a pointer and refuses, counted and named, one that does not fit.
//! The guest's buffer table is guest-writable memory and its pointers are its own choice, so every pointer handed out (window or direct) is recorded with the reference it came from; a release is trusted only when it names an outstanding (pointer, object) pair. Anything else -- a pointer never handed out, a second release, another array's pointer -- is refused, named and counted (`native-bridge.ilp32-argument`), and ART's release does not run: it would `delete[]` or copy through a pointer it never made.
//! Registered for a 32-bit guest only (jni.rs `guest_env`): a 64-bit guest's word holds every host pointer, so it takes ART's buffers through the object stubs as before.
//! Cost: one window (page-granular map) per outstanding copy, plus a copy each way per Get / Release.

use std::collections::HashMap;
use std::sync::Mutex;

use super::objects::{env, host_of, host_slot};

/// jni.h release modes.
const JNI_COMMIT: i32 = 1;
const JNI_ABORT: i32 = 2;
/// jni.h `JNI_TRUE`.
const JNI_TRUE: u8 = 1;

/// One pointer handed to the guest and not yet released: ART's pointer, the reference it was taken from (`obj`, the guest's argument: the release must name the same object), and, for a window copy, the bytes the window holds (`len`, fixed at Get: an array cannot be resized, and a release naming another array is refused before any copy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    host: usize,
    obj: usize,
    len: usize,
    window: bool,
}

/// Outstanding pointers by guest address; a direct pointer (no window) can be outstanding several times (each Get of a non-movable array answers the same address, each needs its Release), a window once.
static HELD: Mutex<Option<HashMap<usize, Vec<Held>>>> = Mutex::new(None);

fn held() -> std::sync::MutexGuard<'static, Option<HashMap<usize, Vec<Held>>>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The guest's pointer for ART's buffer `host` of `len` bytes taken from `obj`: `host` itself when it fits a 32-bit word, else a new window holding a copy; recorded until its release either way. 0 (ART's failure answer) passes unrecorded; 0 is also the answer, counted, when no window memory is left (the JNI out-of-memory answer of a `Get*`).
fn to_guest(what: &str, obj: usize, host: usize, len: usize) -> usize {
    if host == 0 {
        return 0;
    }
    let (at, window) = if host <= u32::MAX as usize {
        (host, false)
    } else {
        let Ok(at) =
            super::active().reserve(len.max(1), (libc::PROT_READ | libc::PROT_WRITE) as u32)
        else {
            super::dispatch::report_refused(
                super::dispatch::REFUSED_RESULT,
                &format!("JNI {what}: no window for {len} bytes"),
            );
            return 0;
        };
        // SAFETY: `host` is ART's buffer of at least `len` bytes (the caller's length from the same array or string), `at` a fresh window of at least `len`.
        unsafe { std::ptr::copy_nonoverlapping(host as *const u8, at as *mut u8, len) };
        (at, true)
    };
    held()
        .get_or_insert_with(HashMap::new)
        .entry(at)
        .or_default()
        .push(Held { host, obj, len, window });
    at
}

/// ART's pointer for guest pointer `guest`, released from `obj` with mode `mode`; `same(a, b)` answers JNI `IsSameObject` for two references that differ as values (a local and a global reference to one array). A window's copy is first copied back into ART's buffer unless the mode is `JNI_ABORT` (`copy_back`: arrays; a string's chars are const); the record (and a window) goes unless the mode is `JNI_COMMIT`.
/// None, refused and counted with the JNI function `what`, when no outstanding Get handed `guest` out for `obj`: a pointer never handed out, a second release, or another array's -- ART's release then does not run (it would free or copy through a pointer it never made) and no window is written.
fn to_host(
    what: &str,
    guest: usize,
    obj: usize,
    mode: i32,
    copy_back: bool,
    same: impl Fn(usize, usize) -> bool,
) -> Option<usize> {
    let mut g = held();
    let found = g.as_mut().and_then(|map| {
        let list = map.get_mut(&guest)?;
        let i = list
            .iter()
            .position(|h| h.obj == obj)
            .or_else(|| list.iter().position(|h| same(h.obj, obj)))?;
        let h = list[i];
        if mode != JNI_COMMIT {
            list.swap_remove(i);
            if list.is_empty() {
                map.remove(&guest);
            }
        }
        Some(h)
    });
    drop(g);
    let Some(h) = found else {
        super::dispatch::report_refused(
            super::dispatch::REFUSED_ARG,
            &format!("JNI {what}: {guest:#x} was not handed out for this object (never, already released, or another object's)"),
        );
        return None;
    };
    if h.window {
        // break `p5_no_copy_back`: the guest's writes never reach ART's buffer
        if copy_back && mode != JNI_ABORT && !cfg!(atl_nb_p5_no_copy_back) {
            // SAFETY: the window holds `h.len` bytes (recorded at Get, still mapped: its record was live); ART's buffer is still live (its release has not run).
            unsafe { std::ptr::copy_nonoverlapping(guest as *const u8, h.host as *mut u8, h.len) };
        }
        if mode != JNI_COMMIT {
            super::active().release(guest, h.len.max(1));
        }
    }
    Some(h.host)
}

/// JNI `IsSameObject(a, b)` through the host env.
///
/// # Safety
/// `host` is a live host JNIEnv; `a`, `b` references valid in it.
unsafe fn is_same(host: usize, a: usize, b: usize) -> bool {
    type Same = extern "C" fn(usize, usize, usize) -> u8;
    // SAFETY: caller contract.
    unsafe { host_fn::<Same>(host, env::IsSameObject)(host, a, b) != 0 }
}

/// Writes `JNI_TRUE` to the guest's `isCopy` (NULL = not asked) when the guest got a window copy.
///
/// # Safety
/// `is_copy` is NULL or the guest's writable `jboolean`.
unsafe fn copied(is_copy: usize, host: usize, guest: usize) {
    if is_copy != 0 && guest != host && guest != 0 {
        // SAFETY: caller contract.
        unsafe { *(is_copy as *mut u8) = JNI_TRUE };
    }
}

type Get = extern "C" fn(usize, usize, usize) -> usize;
type Len = extern "C" fn(usize, usize) -> i32;

/// The host env's function `slot` of type `T`.
///
/// # Safety
/// `host` is a live host JNIEnv and `T` the jni.h type of `slot`.
unsafe fn host_fn<T: Copy>(host: usize, slot: usize) -> T {
    // SAFETY: caller contract; a function pointer is one word.
    unsafe { std::mem::transmute_copy(&host_slot(host, slot)) }
}

/// `<T>* Get<T>ArrayElements(JNIEnv*, <T>Array, jboolean* isCopy)`, element size `SIZE` bytes (jni.h: jboolean/jbyte 1, jchar/jshort 2, jint/jfloat 4, jlong/jdouble 8).
pub(super) extern "C" fn get_elements<const SLOT: usize, const SIZE: usize>(
    guest_env: usize,
    array: usize,
    is_copy: usize,
) -> usize {
    // SAFETY: the guest's arguments to this jni.h function; `host` is the env's host JNIEnv.
    unsafe {
        let host = host_of(guest_env);
        let p = host_fn::<Get>(host, SLOT)(host, array, is_copy);
        let n = if p > u32::MAX as usize {
            host_fn::<Len>(host, env::GetArrayLength)(host, array).max(0) as usize * SIZE
        } else {
            0
        };
        let g = to_guest("Get<Type>ArrayElements", array, p, n);
        copied(is_copy, p, g);
        g
    }
}

/// `void Release<T>ArrayElements(JNIEnv*, <T>Array, <T>* elems, jint mode)`.
pub(super) extern "C" fn release_elements<const SLOT: usize>(
    guest_env: usize,
    array: usize,
    elems: usize,
    mode: i32,
) {
    type Rel = extern "C" fn(usize, usize, usize, i32);
    // SAFETY: as `get_elements`.
    unsafe {
        let host = host_of(guest_env);
        let same = |a, b| is_same(host, a, b);
        if let Some(p) = to_host("Release<Type>ArrayElements", elems, array, mode, true, same) {
            host_fn::<Rel>(host, SLOT)(host, array, p, mode);
        }
    }
}

/// A string's chars ([`get_string`]): `GetStringUTFChars` (modified UTF-8, NUL-terminated: its length + 1), `GetStringChars` / `GetStringCritical` (`GetStringLength` UTF-16 units, no terminator).
pub(super) const UTF: bool = true;
pub(super) const UTF16: bool = false;

/// `const char* GetStringUTFChars(JNIEnv*, jstring, jboolean*)`, `const jchar* GetStringChars(...)`, `const jchar* GetStringCritical(...)` (`SLOT`; `UTF8` per [`UTF`]).
pub(super) extern "C" fn get_string<const SLOT: usize, const UTF8: bool>(
    guest_env: usize,
    string: usize,
    is_copy: usize,
) -> usize {
    // SAFETY: as `get_elements`; a UTF answer is NUL-terminated (jni.h).
    unsafe {
        let host = host_of(guest_env);
        let p = host_fn::<Get>(host, SLOT)(host, string, is_copy);
        let n = if p <= u32::MAX as usize {
            0
        } else if UTF8 {
            libc::strlen(p as *const libc::c_char) + 1
        } else {
            host_fn::<Len>(host, env::GetStringLength)(host, string).max(0) as usize * 2
        };
        let g = to_guest("Get<String>Chars", string, p, n);
        copied(is_copy, p, g);
        g
    }
}

/// `void ReleaseStringUTFChars(JNIEnv*, jstring, const char*)` and its `jchar` siblings (`SLOT`): the window given back, ART's pointer released.
pub(super) extern "C" fn release_string<const SLOT: usize>(
    guest_env: usize,
    string: usize,
    chars: usize,
) {
    type Rel = extern "C" fn(usize, usize, usize);
    // SAFETY: as `get_elements`.
    unsafe {
        let host = host_of(guest_env);
        let same = |a, b| is_same(host, a, b);
        if let Some(p) = to_host("ReleaseString<Type>Chars", chars, string, 0, false, same) {
            host_fn::<Rel>(host, SLOT)(host, string, p);
        }
    }
}

/// The env specials of a 32-bit guest: every buffer pair above.
pub(super) fn specials() -> Vec<(usize, usize)> {
    macro_rules! arrays {
        ($($get:ident $rel:ident $size:literal),*) => {
            vec![$(
                (env::$get, get_elements::<{ env::$get }, $size> as *const () as usize),
                (env::$rel, release_elements::<{ env::$rel }> as *const () as usize),
            )*]
        };
    }
    let mut v = arrays!(
        GetBooleanArrayElements ReleaseBooleanArrayElements 1,
        GetByteArrayElements ReleaseByteArrayElements 1,
        GetCharArrayElements ReleaseCharArrayElements 2,
        GetShortArrayElements ReleaseShortArrayElements 2,
        GetIntArrayElements ReleaseIntArrayElements 4,
        GetLongArrayElements ReleaseLongArrayElements 8,
        GetFloatArrayElements ReleaseFloatArrayElements 4,
        GetDoubleArrayElements ReleaseDoubleArrayElements 8
    );
    v.extend([
        (
            env::GetStringUTFChars,
            get_string::<{ env::GetStringUTFChars }, UTF> as *const () as usize,
        ),
        (
            env::ReleaseStringUTFChars,
            release_string::<{ env::ReleaseStringUTFChars }> as *const () as usize,
        ),
        (
            env::GetStringChars,
            get_string::<{ env::GetStringChars }, UTF16> as *const () as usize,
        ),
        (
            env::ReleaseStringChars,
            release_string::<{ env::ReleaseStringChars }> as *const () as usize,
        ),
        (
            env::GetStringCritical,
            get_string::<{ env::GetStringCritical }, UTF16> as *const () as usize,
        ),
        (
            env::ReleaseStringCritical,
            release_string::<{ env::ReleaseStringCritical }> as *const () as usize,
        ),
    ]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::dispatch::{ILP32_REFUSED, REFUSED_LOCK};
    use std::sync::atomic::Ordering;

    /// Reference values in these tests: distinct objects unless `same` says otherwise.
    const A: usize = 0x11;
    const B: usize = 0x22;
    fn distinct(_: usize, _: usize) -> bool {
        false
    }
    fn rel(guest: usize, obj: usize, mode: i32) -> Option<usize> {
        to_host("t", guest, obj, mode, true, distinct)
    }
    fn refused() -> u64 {
        ILP32_REFUSED.load(Ordering::Relaxed) as u64
    }

    /// A pointer that fits passes unchanged both ways, once per Get (a direct pointer handed out twice needs two releases; a third is refused); ART's 0 is not recorded.
    #[test]
    fn a_fitting_pointer_is_not_copied() {
        let _serial = REFUSED_LOCK.lock();
        let before = refused();
        assert_eq!(to_guest("t", A, 0x1000, 16), 0x1000);
        assert_eq!(to_guest("t", A, 0x1000, 16), 0x1000);
        assert_eq!(rel(0x1000, A, 0), Some(0x1000));
        assert_eq!(rel(0x1000, A, 0), Some(0x1000));
        assert_eq!(to_guest("t", A, 0, 0), 0);
        assert_eq!(refused(), before);
        assert_eq!(rel(0x1000, A, 0), None);
        assert_eq!(refused(), before + 1);
    }

    /// The review's questions (landed-changes-review-7 item 3) for a release: a pointer never handed out, a second release, and another array's release are each refused (None = ART's release skipped), counted once each (3), and none writes the window or ART's buffer; the right object then releases normally, and a different reference value IsSameObject calls the same object is accepted.
    #[test]
    fn unknown_double_and_foreign_releases_are_refused() {
        let _serial = REFUSED_LOCK.lock();
        let mut art: Vec<u8> = vec![1, 2, 3, 4];
        let host = art.as_mut_ptr() as usize;
        assert!(host > u32::MAX as usize, "test host heap below 4 GiB");
        let before = refused();
        let w = to_guest("t", A, host, 4);
        // SAFETY: the window holds 4 bytes.
        unsafe { *(w as *mut u8) = 9 };
        assert_eq!(rel(0x2000, A, 0), None, "never handed out");
        assert_eq!(rel(w, B, 0), None, "another array's release");
        assert_eq!(art, [1, 2, 3, 4], "no copy-back through a refused release");
        assert_eq!(refused(), before + 2);
        // a second reference to the same array (IsSameObject true) releases it
        assert_eq!(to_host("t", w, B, 0, true, |a, b| a == A && b == B), Some(host));
        assert_eq!(art, [9, 2, 3, 4]);
        assert_eq!(rel(w, A, 0), None, "second release");
        assert_eq!(refused(), before + 3);
    }

    /// JNI spec, Release<Type>ArrayElements: mode 0 copies back and frees, JNI_COMMIT copies back and keeps the buffer, JNI_ABORT frees without copying back. Here the window plays the guest's buffer and `art` ART's copy: COMMIT -> art sees 11 and the window stays; a write after it then ABORT -> art keeps 11 at [1]'s old value 2, the window is gone.
    #[test]
    fn release_modes_follow_the_jni_spec() {
        let mut art: Vec<i32> = vec![1, 2, 3];
        let host = art.as_mut_ptr() as usize;
        // a host heap address is above 4 GiB on every 64-bit host the tests run on (x86_64 user space starts at 0x5555_5555_4000 for PIE heaps)
        assert!(host > u32::MAX as usize, "test host heap below 4 GiB");
        let _serial = REFUSED_LOCK.lock();
        let w = to_guest("t", A, host, 12);
        assert_ne!(w, host);
        // SAFETY: the window holds 3 i32.
        let win = unsafe { std::slice::from_raw_parts_mut(w as *mut i32, 3) };
        assert_eq!(win, &[1, 2, 3]);
        win[0] = 11;
        assert_eq!(rel(w, A, JNI_COMMIT), Some(host));
        assert_eq!(art, [11, 2, 3]);
        win[1] = 22;
        assert_eq!(rel(w, A, JNI_ABORT), Some(host));
        assert_eq!(art, [11, 2, 3]);
        // the window was given back: a second release is refused
        assert_eq!(rel(w, A, 0), None);
    }

    /// Mode 0 copies back and gives the window back; a string release (copy_back false) never writes ART's buffer.
    #[test]
    fn mode_zero_copies_back_and_strings_never_do() {
        let mut art = *b"abc\0";
        let host = art.as_mut_ptr() as usize;
        let _serial = REFUSED_LOCK.lock();
        let w = to_guest("t", A, host, 4);
        // SAFETY: the window holds 4 bytes.
        unsafe { *(w as *mut u8) = b'x' };
        assert_eq!(to_host("t", w, A, 0, false, distinct), Some(host));
        assert_eq!(&art, b"abc\0");
        let w = to_guest("t", A, host, 4);
        // SAFETY: as above.
        unsafe { *(w as *mut u8) = b'x' };
        assert_eq!(rel(w, A, 0), Some(host));
        assert_eq!(&art, b"xbc\0");
        assert!(held().as_ref().is_none_or(|m| !m.contains_key(&w)));
    }

    /// One special per Get and Release: 8 array pairs + 3 string pairs.
    #[test]
    fn every_buffer_pair_is_special() {
        let s = specials();
        assert_eq!(s.len(), 22);
        assert!(s.iter().any(|&(slot, _)| slot == env::ReleaseStringCritical));
    }
}
