// SPDX-License-Identifier: MIT
//! Recorder EGL / GLES library for the GS2.2 G1 gates (`egl-bridge`, and the fallback checks of `qemu-gl`): a host stand-in for `libEGL.so` and `libGLESv2.so`, staged as both names (one file). Test-only: a cargo example (cdylib), never staged by `xtask native`; the harness copies it to its bwrap `/system/lib64/`.
//! `eglGetProcAddress` answers three names with a function of this library and NULL for every other: `glGetPathLengthNV` (a GL prototype the bridge describes; QEMU's fallback check, the gate registers no row for it), `eglQueryDevicesEXT` (a prototype with no row in either engine: identity's dedup check) and `glAtlNoDescriptor` (no prototype: the bridge must trap it). The GL functions record their argument words; `rec_take` returns the last call's words.
#![allow(
    clippy::missing_safety_doc,
    non_snake_case,
    reason = "C ABI entries named as the GL and EGL headers name them; called by the bridge and the gates"
)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::{Mutex, PoisonError};

/// glFinish's mark word: the one argument the call records.
pub const FINISH_MARK: u64 = 0x0f1;
/// glGetPathLengthNV's answer: exact in a float, so a gate can check the fp result bit for bit.
pub const PATH_LENGTH: f32 = 12.5;

/// The last recorded call's words.
static LAST: Mutex<Vec<u64>> = Mutex::new(Vec::new());

fn record(words: Vec<u64>) {
    *LAST.lock().unwrap_or_else(PoisonError::into_inner) = words;
}

fn bits(v: f32) -> u64 {
    u64::from(v.to_bits())
}

#[unsafe(no_mangle)]
pub extern "C" fn glVertexAttrib4f(index: u32, x: f32, y: f32, z: f32, w: f32) {
    record(vec![u64::from(index), bits(x), bits(y), bits(z), bits(w)]);
}

#[unsafe(no_mangle)]
pub extern "C" fn glTexImage2D(
    target: u32,
    level: i32,
    internalformat: i32,
    width: i32,
    height: i32,
    border: i32,
    format: u32,
    ty: u32,
    pixels: *const c_void,
) {
    record(vec![
        u64::from(target),
        i64::from(level) as u64,
        i64::from(internalformat) as u64,
        i64::from(width) as u64,
        i64::from(height) as u64,
        i64::from(border) as u64,
        u64::from(format),
        u64::from(ty),
        pixels as usize as u64,
    ]);
}

#[unsafe(no_mangle)]
pub extern "C" fn glFinish() {
    record(vec![FINISH_MARK]);
}

#[unsafe(no_mangle)]
pub extern "C" fn glGetPathLengthNV(path: u32, start: i32, num: i32) -> f32 {
    record(vec![
        u64::from(path),
        i64::from(start) as u64,
        i64::from(num) as u64,
    ]);
    PATH_LENGTH
}

/// `eglQueryDevicesEXT`'s answer: a prototype the table describes and no row holds (EGL_TRUE; the devices are never written). Identity's dedup check (`egl_bridge` check e) asks for it.
pub extern "C" fn eglQueryDevicesEXT(max_devices: i32, devices: *mut c_void, num: *mut i32) -> u32 {
    record(vec![
        i64::from(max_devices) as u64,
        devices as usize as u64,
        num as usize as u64,
    ]);
    EGL_TRUE
}

/// EGL 1.5 egl.h `EGL_TRUE`.
const EGL_TRUE: u32 = 1;

/// The answer for `glAtlNoDescriptor`: a function no prototype describes, so the bridge never calls it.
extern "C" fn rec_no_descriptor() {
    record(vec![0xbad]);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn eglGetProcAddress(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return std::ptr::null();
    }
    // SAFETY: EGL 1.5 §3.10: the caller's NUL-terminated name, alive for the call.
    let name = unsafe { CStr::from_ptr(name) };
    match name.to_bytes() {
        b"glGetPathLengthNV" => glGetPathLengthNV as *const () as *const c_void,
        b"eglQueryDevicesEXT" => eglQueryDevicesEXT as *const () as *const c_void,
        b"glAtlNoDescriptor" => rec_no_descriptor as *const () as *const c_void,
        _ => std::ptr::null(),
    }
}

/// The last recorded call's words: copies up to `cap` of them into `out`; returns how many the call recorded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rec_take(out: *mut u64, cap: usize) -> usize {
    let last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    let n = last.len().min(cap);
    if !out.is_null() {
        // SAFETY: the caller's array of `cap` words, the copy is `n <= cap` of them.
        unsafe { std::ptr::copy_nonoverlapping(last.as_ptr(), out, n) };
    }
    last.len()
}
