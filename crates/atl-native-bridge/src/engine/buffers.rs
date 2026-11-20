// SPDX-License-Identifier: MIT
//! ILP32 read-only host buffers (build.rs `ILP32_PROXY_SHIMS`): a pointer result into data a host object owns, which a 32-bit guest cannot address above 4 GiB. Answered with a window copy attached to the object's handle (handles.rs `attach`): made on the first call, the same address on later ones, given back when the call ending the object frees the handle (`AAsset_close`, `ADisplayLutsEntry_destroy`: build.rs `HANDLE_RELEASES`), which matches the headers' lifetime ("valid for the lifetime of the entry"). The data never changes in that lifetime, so one copy stays exact.

use super::{bindings, handles};

/// Host export `name` of libandroid.
fn host(name: &str) -> usize {
    bindings::system_symbol("libandroid.so", name)
        .unwrap_or_else(|| super::unsupported(&format!("{name}: no host libandroid")))
}

/// Guest handle `h`'s window copy: the attached one, else `len` bytes from `data` (both of the host object) copied into a new window and attached. 0 (NULL, the headers' failure answer) for an unknown handle (counted), a NULL host buffer or no window memory.
fn copy_for(
    h: usize,
    symbol: &str,
    data: impl Fn(usize) -> usize,
    len: impl Fn(usize) -> Option<usize>,
) -> usize {
    let Some(obj) = handles::to_host(h).filter(|&o| o != 0) else {
        super::dispatch::ILP32_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        super::dispatch::report_refused(
            super::dispatch::REFUSED_HANDLE,
            &format!("{symbol} parameter 0 = {h:#x}"),
        );
        return 0;
    };
    if let Some(at) = handles::window(h) {
        return at;
    }
    let (src, n) = (data(obj), len(obj).unwrap_or(0));
    if src == 0 {
        return 0;
    }
    let Ok(at) =
        super::active().reserve(n.max(1), libc::PROT_READ as u32 | libc::PROT_WRITE as u32)
    else {
        super::dispatch::report_refused(
            super::dispatch::REFUSED_RESULT,
            &format!("{symbol}: no window for {n} bytes"),
        );
        return 0;
    };
    // SAFETY: `src` is the host object's buffer of `n` bytes (the header's length), `at` a fresh window of at least `n`.
    unsafe { std::ptr::copy_nonoverlapping(src as *const u8, at as *mut u8, n) };
    if !handles::attach(h, at, n.max(1)) {
        // another thread attached first: answer its copy
        super::active().release(at, n.max(1));
        return handles::window(h).unwrap_or(0);
    }
    at
}

/// `const void* AAsset_getBuffer(AAsset*)`, the arm guest's: the asset's whole contents (`AAsset_getLength64` bytes).
pub(super) extern "C" fn asset_get_buffer(asset: usize) -> usize {
    type Data = unsafe extern "C" fn(usize) -> usize;
    type Len = unsafe extern "C" fn(usize) -> i64;
    copy_for(
        asset,
        "AAsset_getBuffer",
        // SAFETY: libandroid's `AAsset_getBuffer` on a live asset.
        |a| unsafe { std::mem::transmute::<usize, Data>(host("AAsset_getBuffer"))(a) },
        |a| {
            // SAFETY: libandroid's `AAsset_getLength64` on a live asset.
            let n = unsafe { std::mem::transmute::<usize, Len>(host("AAsset_getLength64"))(a) };
            usize::try_from(n).ok()
        },
    )
}

/// `const float* ADisplayLutsEntry_getBuffer(const ADisplayLutsEntry*)`, the arm guest's: the entry's floats, [`lut_floats`] of them.
pub(super) extern "C" fn lut_get_buffer(entry: usize) -> usize {
    type Data = unsafe extern "C" fn(usize) -> usize;
    type Int = unsafe extern "C" fn(usize) -> i32;
    copy_for(
        entry,
        "ADisplayLutsEntry_getBuffer",
        // SAFETY: libandroid's `ADisplayLutsEntry_getBuffer` on a live entry.
        |e| unsafe { std::mem::transmute::<usize, Data>(host("ADisplayLutsEntry_getBuffer"))(e) },
        |e| {
            // SAFETY: libandroid's int accessors on a live entry.
            let (dim, size) = unsafe {
                (
                    std::mem::transmute::<usize, Int>(host("ADisplayLutsEntry_getDimension"))(e),
                    std::mem::transmute::<usize, Int>(host("ADisplayLutsEntry_getSize"))(e),
                )
            };
            lut_floats(dim, size).map(|n| n * std::mem::size_of::<f32>())
        },
    )
}

/// The floats of a LUT entry of `dimension` and per-dimension `size`: display_luts.cpp `ADisplayLutsEntry_createEntry` derives size = length (one dimension) or cbrt(length / 3) (three: three channels of size^3), so length = size or 3 * size^3.
fn lut_floats(dimension: i32, size: i32) -> Option<usize> {
    let s = usize::try_from(size).ok()?;
    match dimension {
        // ADISPLAYLUTS_ONE_DIMENSION
        1 => Some(s),
        // ADISPLAYLUTS_THREE_DIMENSION
        3 => s.checked_mul(s)?.checked_mul(s)?.checked_mul(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// display_luts.cpp: a 3D entry of 17 per dimension holds 3 * 17^3 = 14739 floats; 1D holds `size`.
    #[test]
    fn lut_lengths_invert_create_entry() {
        assert_eq!(lut_floats(3, 17), Some(14739));
        assert_eq!(lut_floats(1, 256), Some(256));
        assert_eq!(lut_floats(2, 4), None);
    }

    /// The copy is made once per handle, holds the host bytes, and survives until the handle is freed.
    #[test]
    fn a_buffer_copy_lives_with_its_handle() {
        let data = *b"asset contents";
        let obj = 0x7f00_0000_5000usize;
        let h = handles::to_guest("AAsset", obj).unwrap() as usize;
        let src = data.as_ptr() as usize;
        let at = copy_for(h, "t", |_| src, |_| Some(data.len()));
        assert_ne!(at, 0);
        // SAFETY: the window holds data.len() bytes.
        let copy = unsafe { std::slice::from_raw_parts(at as *const u8, data.len()) };
        assert_eq!(copy, &data);
        assert_eq!(
            copy_for(h, "t", |_| unreachable!(), |_| unreachable!()),
            at,
            "one copy per handle"
        );
        handles::release(h);
        assert_eq!(handles::window(h), None);
    }
}
