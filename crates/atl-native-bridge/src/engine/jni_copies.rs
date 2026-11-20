// SPDX-License-Identifier: MIT
//! JNI buffers ART hands a 32-bit guest (jni.h `Get<Type>ArrayElements`, `GetStringUTFChars`, `GetStringChars`, `GetStringCritical`, with their `Release*`): ART's copy is host heap (`new uint64_t[]` / `new jchar[]` / `new char[]`, jni_internal.cc), above 4 GiB, which the guest's word cannot hold. The guest gets a window copy of it (`Active::reserve`, below 4 GiB) and `*isCopy = JNI_TRUE`; the window stands for ART's copy until the matching release, which copies the window back into ART's copy (array modes 0 and `JNI_COMMIT`), hands ART its own pointer with the guest's mode, and gives the window back (every mode but `JNI_COMMIT`). ART's copy-back and free rules stay ART's: it only ever sees its own pointer.
//! A pointer that already fits the guest word passes both ways unchanged: ART's heap objects lie below 4 GiB (32-bit compressed references), so a direct pointer to array or string data (`isCopy` false; ART's choice for a non-movable object, `GetPrimitiveArrayCritical`, an uncompressed `GetStringCritical`) needs no copy. Hence no special for `GetPrimitiveArrayCritical` / `GetDirectBufferAddress`: the object stub's result narrowing (dispatch.rs `ilp32_narrow`) passes such a pointer and refuses, counted and named, one that does not fit.
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

/// One outstanding copy: ART's pointer and the bytes the window holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    host: usize,
    len: usize,
}

/// Outstanding copies by window address.
static HELD: Mutex<Option<HashMap<usize, Held>>> = Mutex::new(None);

fn held() -> std::sync::MutexGuard<'static, Option<HashMap<usize, Held>>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The guest's pointer for ART's buffer `host` of `len` bytes: `host` itself when it fits a 32-bit word (0 included), else a new window holding a copy, recorded until its release; 0 (the JNI error answer, as an out-of-memory `Get*`) when no window memory is left, counted.
fn to_guest(what: &str, host: usize, len: usize) -> usize {
    if host <= u32::MAX as usize {
        return host;
    }
    let Ok(at) = super::active().reserve(len.max(1), (libc::PROT_READ | libc::PROT_WRITE) as u32)
    else {
        super::dispatch::report_refused(
            super::dispatch::REFUSED_RESULT,
            &format!("JNI {what}: no window for {len} bytes"),
        );
        return 0;
    };
    // SAFETY: `host` is ART's buffer of at least `len` bytes (the caller's length from the same array or string), `at` a fresh window of at least `len`.
    unsafe { std::ptr::copy_nonoverlapping(host as *const u8, at as *mut u8, len) };
    held()
        .get_or_insert_with(HashMap::new)
        .insert(at, Held { host, len });
    at
}

/// ART's pointer for guest pointer `guest` at a release of mode `mode`: a window's copy is first copied back into ART's buffer unless the mode is `JNI_ABORT` (`copy_back`: arrays; a string's chars are const), and the window is given back unless the mode is `JNI_COMMIT`. A pointer no window stands for is ART's own and passes unchanged.
fn to_host(guest: usize, mode: i32, copy_back: bool) -> usize {
    let mut g = held();
    let Some(map) = g.as_mut() else { return guest };
    let Some(&h) = map.get(&guest) else {
        return guest;
    };
    // break `p5_no_copy_back`: the guest's writes never reach ART's buffer
    if copy_back && mode != JNI_ABORT && !cfg!(atl_nb_p5_no_copy_back) {
        // SAFETY: the window holds `h.len` bytes; ART's buffer is still live (its release has not run).
        unsafe { std::ptr::copy_nonoverlapping(guest as *const u8, h.host as *mut u8, h.len) };
    }
    if mode != JNI_COMMIT {
        map.remove(&guest);
        drop(g);
        super::active().release(guest, h.len.max(1));
    }
    h.host
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
        let g = to_guest("Get<Type>ArrayElements", p, n);
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
        host_fn::<Rel>(host, SLOT)(host, array, to_host(elems, mode, true), mode);
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
        let g = to_guest("Get<String>Chars", p, n);
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
        host_fn::<Rel>(host, SLOT)(host, string, to_host(chars, 0, false));
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

    /// A host buffer above 4 GiB stands in for ART's copy: a pointer that fits passes unchanged.
    #[test]
    fn a_fitting_pointer_is_not_copied() {
        assert_eq!(to_guest("t", 0x1000, 16), 0x1000);
        assert_eq!(to_host(0x1000, 0, true), 0x1000);
        assert_eq!(to_guest("t", 0, 0), 0);
    }

    /// JNI spec, Release<Type>ArrayElements: mode 0 copies back and frees, JNI_COMMIT copies back and keeps the buffer, JNI_ABORT frees without copying back. Here the window plays the guest's buffer and `art` ART's copy: COMMIT -> art sees 11 and the window stays; a write after it then ABORT -> art keeps 11 at [1]'s old value 2, the window is gone.
    #[test]
    fn release_modes_follow_the_jni_spec() {
        let mut art: Vec<i32> = vec![1, 2, 3];
        let host = art.as_mut_ptr() as usize;
        // a host heap address is above 4 GiB on every 64-bit host the tests run on (x86_64 user space starts at 0x5555_5555_4000 for PIE heaps)
        assert!(host > u32::MAX as usize, "test host heap below 4 GiB");
        let w = to_guest("t", host, 12);
        assert_ne!(w, host);
        // SAFETY: the window holds 3 i32.
        let win = unsafe { std::slice::from_raw_parts_mut(w as *mut i32, 3) };
        assert_eq!(win, &[1, 2, 3]);
        win[0] = 11;
        assert_eq!(to_host(w, JNI_COMMIT, true), host);
        assert_eq!(art, [11, 2, 3]);
        win[1] = 22;
        assert_eq!(to_host(w, JNI_ABORT, true), host);
        assert_eq!(art, [11, 2, 3]);
        // the window was given back: a second release is ART's own pointer
        assert_eq!(to_host(w, 0, true), w);
    }

    /// Mode 0 copies back and gives the window back; a string release (copy_back false) never writes ART's buffer.
    #[test]
    fn mode_zero_copies_back_and_strings_never_do() {
        let mut art = *b"abc\0";
        let host = art.as_mut_ptr() as usize;
        let w = to_guest("t", host, 4);
        // SAFETY: the window holds 4 bytes.
        unsafe { *(w as *mut u8) = b'x' };
        assert_eq!(to_host(w, 0, false), host);
        assert_eq!(&art, b"abc\0");
        let w = to_guest("t", host, 4);
        // SAFETY: as above.
        unsafe { *(w as *mut u8) = b'x' };
        assert_eq!(to_host(w, 0, true), host);
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
