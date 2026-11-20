// SPDX-License-Identifier: MIT
//! The lane-mapped class: host sides of the NDK functions with a lane the host cannot take as the guest passes it (build.rs `ENV_SHIMS` and `CALLBACKS`, `Entry::Mapped`). Each row maps those lanes, then calls the same function of the host library of its soname (bindings.rs `system_symbol`).
//! - `JNIEnv*`: the guest passes its own env (jni.rs `GuestObj`); mapped to the host env (jni.rs `host_env`).
//! - a guest function pointer (GS2.2 callback-param, tr-first blocker 5): mapped to a down shell ([`super::Active::down`], plain kind, the callback's shorty), so the host library calls a host address that enters the guest. The host may call it during the call or later, from any thread: the shell attaches a thread on its first down call (qemu `call_down`, identity `attach_entry`). Shells are deduplicated by (function, shorty) and never freed, so a callback re-posted every frame takes one. The callback's own arguments are values or the guest's user data (build.rs `CALLBACKS` review), passed unchanged.
//!
//! - ILP32 (arm guest): a record or out word through a host temporary (convert.rs), a host object as a handle (handles.rs), text results interned in guest-addressable memory.
//!
//! The other arguments are host values already: object references are the host's, and on an LP64 guest an AAssetManager, ANativeWindow or pixel pointer names host memory the guest reads in the same address space. Every row's lanes fit six int and eight fp registers (build.rs refuses the rest), so one forwarder shape serves all: the fp lanes keep their bits untouched (a float in the low half of its register), an fp result returns in the fp register.

use super::bindings::{self, Elem, LaneMap, Len, MappedRow, RetMap, Slots};
use super::{convert, handles, jni, unsupported};

/// The forwarded host function: six int lanes (SysV's integer registers) and eight fp lanes (its vector registers); a shorter prototype ignores the rest. One int result (a void one's is never read).
type HostFn = unsafe extern "C" fn(
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> usize;
/// [`HostFn`] with an fp result.
type HostFnFp = unsafe extern "C" fn(
    usize,
    usize,
    usize,
    usize,
    usize,
    usize,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
    f64,
) -> f64;

/// `Entry::Mapped` row `(soname, symbol, maps, ret)`'s call: each mapped lane replaced, then `symbol` of the host library `soname` with the six int and eight fp lanes, its result mapped by `ret`. `fp_ret`: the result is an fp lane, answered as its bits. Aborts when the host library lacks the symbol (bindings.rs `target` binds the row only when it has it) or no down shell is left.
pub(super) fn call_mapped(
    (soname, symbol, maps, ret): MappedRow,
    mut lanes: [usize; 6],
    fp: [f64; 8],
    fp_ret: bool,
) -> usize {
    let f = bindings::system_symbol(soname, symbol).unwrap_or_else(|| {
        unsupported(&format!(
            "{symbol}: lane-mapped row with no {soname} in SYSTEM_LIB"
        ))
    });
    // ILP32 handles first: an unknown one refuses the whole call before any temporary or host side effect
    let mut released: Option<usize> = None;
    // word arrays: (lane, the guest array, its element and direction, the host array); the host arrays live until the call returns
    let mut slots: Vec<(usize, usize, Elem, Slots, Vec<usize>)> = Vec::new();
    for &(i, map) in maps {
        match map {
            LaneMap::Handle(_, release) => {
                let Some(host) = handles::to_host(lanes[i]) else {
                    return refuse_handle(symbol, i, lanes[i]);
                };
                if release {
                    released = Some(lanes[i]);
                }
                lanes[i] = host;
            }
            LaneMap::Words(elem, dir, len) if lanes[i] != 0 => {
                // SAFETY: lane `i` is the guest's non-NULL pointer to a word array of this length rule (build.rs ILP32_WORD_ARRAYS: the header's count parameter or terminator), readable for In/InOut.
                match unsafe { words_in(elem, dir, lanes[i], len, &lanes) } {
                    Ok(host) => slots.push((i, lanes[i], elem, dir, host)),
                    Err(Refusal::Handle(h)) => return refuse_handle(symbol, i, h),
                    Err(why) => {
                        super::dispatch::report_refused(
                            super::dispatch::REFUSED_ARG,
                            &format!("{symbol} parameter {i}: {why:?} (bound {MAX_WORDS} words)"),
                        );
                        return 0;
                    }
                }
            }
            _ => {}
        }
    }
    for (i, _, _, _, host) in &mut slots {
        lanes[*i] = host.as_mut_ptr() as usize;
    }
    // ILP32 temporaries: (lane, the guest pointer, its map, the host copy), written back after the call
    let mut temps: Vec<(usize, usize, LaneMap, convert::Temp)> = Vec::new();
    for &(i, map) in maps {
        match map {
            LaneMap::Handle(..) | LaneMap::Words(..) => {}
            LaneMap::Record(c, _) if lanes[i] != 0 => {
                // SAFETY: lane `i` is the guest's non-NULL pointer to this record in its own layout (build.rs from the ILP32 classifier's kind of this parameter).
                let mut t = unsafe { convert::record_in(c, lanes[i]) };
                let host = t.ptr() as usize;
                temps.push((i, lanes[i], map, t));
                lanes[i] = host;
            }
            LaneMap::OutWord(w) if lanes[i] != 0 => {
                // SAFETY: lane `i` is the guest's non-NULL pointer to one word of this kind (as above).
                let mut t = unsafe { convert::word_in(w, lanes[i]) };
                let host = t.ptr() as usize;
                temps.push((i, lanes[i], map, t));
                lanes[i] = host;
            }
            LaneMap::Record(..) | LaneMap::OutWord(_) => {}
            // SAFETY: lane `i` is the guest's own `JNIEnv*` (made by jni.rs `guest_env`), as the NDK function receives it (build.rs proved the position from its prototype).
            LaneMap::Env => lanes[i] = unsafe { jni::host_env(lanes[i]) },
            // break `cb_down_skip` (app gate, rows looperNow / looperLater): the guest's own address reaches the host, which calls guest code natively (QEMU: AArch64 bytes run as x86-64)
            #[cfg(not(atl_nb_cb_down_skip))]
            LaneMap::Down(shorty) if lanes[i] != 0 => {
                lanes[i] = super::active()
                    .down(lanes[i], super::Down::Plain, Some(shorty))
                    .unwrap_or_else(|e| unsupported(&format!("{symbol}: callback lane {i}: {e}")));
            }
            LaneMap::DownHandle(shorty, ty) if lanes[i] != 0 => {
                lanes[i] = super::active()
                    .down(lanes[i], super::Down::Handle(ty), Some(shorty))
                    .unwrap_or_else(|e| unsupported(&format!("{symbol}: callback lane {i}: {e}")));
            }
            LaneMap::Down(_) | LaneMap::DownHandle(..) => {}
        }
    }
    // the ending call's address may be reused by the host before `release` below: unlink it now so `to_guest` cannot hand out the dying handle
    if let Some(h) = released {
        handles::retire(h);
    }
    let [a0, a1, a2, a3, a4, a5] = lanes;
    let [f0, f1, f2, f3, f4, f5, f6, f7] = fp;
    let value = if fp_ret {
        // SAFETY: the host library's export of this name, whose prototype (the NDK header's) takes at most six int and eight fp lanes and answers an fp lane here (build.rs); extra lanes sit in registers it never reads.
        let f: HostFnFp = unsafe { std::mem::transmute(f) };
        // SAFETY: as below.
        (unsafe { f(a0, a1, a2, a3, a4, a5, f0, f1, f2, f3, f4, f5, f6, f7) }).to_bits() as usize
    } else {
        // SAFETY: as above, an int lane or void result.
        let f: HostFn = unsafe { std::mem::transmute(f) };
        // SAFETY: as above; every other lane is a host value in this address space, every mapped one the host's own.
        unsafe { f(a0, a1, a2, a3, a4, a5, f0, f1, f2, f3, f4, f5, f6, f7) }
    };
    for (i, guest, map, mut t) in temps {
        // a refused host pointer is written NULL and counted (convert.rs); reported here with the row and lane
        let ok = match map {
            // SAFETY: `guest` is the guest's pointer read above, writable: a non-const pointee.
            LaneMap::Record(c, true) => unsafe { convert::to_guest(c, t.ptr(), guest as *mut u8) },
            // SAFETY: as above.
            LaneMap::OutWord(w) => unsafe { convert::word_out(w, &mut t, guest) },
            _ => true,
        };
        if !ok {
            super::dispatch::report_refused(
                super::dispatch::REFUSED_OUT,
                &format!("{symbol} parameter {i}"),
            );
        }
    }
    for (i, guest, elem, dir, host) in slots {
        // SAFETY: `guest` is the guest's pointer to these slots read above, writable for Out/InOut (a non-const `T**`).
        if !unsafe { words_out(elem, dir, guest, &host) } {
            super::dispatch::report_refused(
                super::dispatch::REFUSED_HANDLE,
                &format!("{symbol} parameter {i}: handle table full"),
            );
        }
    }
    if let Some(h) = released {
        handles::release(h);
    }
    match ret {
        RetMap::Plain => value,
        RetMap::Handle(ty) => handles::to_guest(ty, value).map_or_else(
            || {
                super::dispatch::report_refused(
                    super::dispatch::REFUSED_HANDLE,
                    &format!("{symbol} result: handle table full"),
                );
                0
            },
            |h| h as usize,
        ),
        // SAFETY: the host function's `const char*` result (the classifier's `String` kind): NULL or NUL-terminated text.
        RetMap::String => unsafe { handles::intern(value) }.unwrap_or_else(|| {
            super::dispatch::report_refused(
                super::dispatch::REFUSED_RESULT,
                &format!("{symbol} result: no window for its text"),
            );
            0
        }),
    }
}

/// Host bytes one call's word array may take: a per-call memory budget, not a spec limit (no GL ES / EGL / NDK header bounds these counts). Real counts are small: EGL configs in tens, shader strings and attribute pairs in ones to hundreds.
const MAX_ARRAY_BYTES: usize = 8 << 20;
/// Most words one array converts (= [`MAX_ARRAY_BYTES`] / host word): a longer count, or an attribute list with no terminator within it, is refused and counted, never allocated or read past.
const MAX_WORDS: usize = MAX_ARRAY_BYTES / size_of::<usize>();

/// Why [`words_in`] refused an array.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// a slot read in holds this value, a handle the table does not hold
    Handle(usize),
    /// the count (elements) is past [`MAX_WORDS`]
    Bound(usize),
    /// no terminator key within [`MAX_WORDS`]
    Unterminated,
}

/// The element count of a word array: `Param` reads its count lane as the guest's 32-bit word, unsigned (a negative `int`/`GLsizei` count reads past the bound and is refused, so a host reading `size_t` never sees more than the array holds); `Terminated` scans key slots (even indices) of the guest list for the terminator, values unread.
///
/// # Safety
/// For `Terminated`, `guest` points to a readable list of 4-byte words in key/value pairs.
unsafe fn word_count(len: Len, guest: usize, lanes: &[usize; 6]) -> Result<usize, Refusal> {
    let n = match len {
        Len::One => 1,
        Len::Param(l, factor) => (lanes[l] as u32 as usize).saturating_mul(factor),
        Len::Terminated(end) => {
            return (0..MAX_WORDS)
                .step_by(2)
                // SAFETY: key slot `k`, reached only past the pairs before it, none of them the terminator (the caller's list).
                .find(|&k| unsafe { (guest as *const u32).add(k).read_unaligned() } == end)
                .map(|k| k + 1)
                .ok_or(Refusal::Unterminated);
        }
    };
    if n > MAX_WORDS {
        return Err(Refusal::Bound(n));
    }
    Ok(n)
}

/// The host array for the guest word array at `guest` (its length by `len`, count lanes in `lanes`): Out zeroed (the guest's slots are uninitialised), In/InOut each guest word converted by `elem`. Lives as long as the caller keeps it: the call.
///
/// # Safety
/// `guest` points to the array `len` describes, readable for In/InOut (and for `Terminated`, whatever the direction).
unsafe fn words_in(
    elem: Elem,
    dir: Slots,
    guest: usize,
    len: Len,
    lanes: &[usize; 6],
) -> Result<Vec<usize>, Refusal> {
    // SAFETY: the caller's array.
    let n = unsafe { word_count(len, guest, lanes) }?;
    if dir == Slots::Out {
        return Ok(vec![0; n]);
    }
    (0..n)
        .map(|k| {
            // SAFETY: slot `k` of the caller's `n` readable slots (unaligned: the guest's array may be packed in a record).
            let w = unsafe { (guest as *const u32).add(k).read_unaligned() };
            match elem {
                Elem::Handle(_) => handles::to_host(w as usize).ok_or(Refusal::Handle(w as usize)),
                // AAPCS32: a 4-byte data pointer; guest base 0, so the address as an unsigned number is the host's
                Elem::Ptr => Ok(w as usize),
                // egl.h `typedef intptr_t EGLAttrib`: a signed 4-byte word on ILP32, 8 on LP64
                Elem::Attrib => Ok(w as i32 as isize as usize),
            }
        })
        .collect()
}

/// Writes the host array's objects back to the guest's slots as handles (a `Handle` array, Out or InOut; In writes nothing, and `Ptr`/`Attrib` arrays are In only: build.rs refuses another direction). A one-slot lane writes its slot always (NULL included: the host's failure answer); an array only the slots the host filled (non-NULL), leaving the rest as the guest had them. `false` when a handle could not be issued (that slot written NULL).
///
/// # Safety
/// For Out/InOut, `guest` points to `host.len()` writable 4-byte slots.
unsafe fn words_out(elem: Elem, dir: Slots, guest: usize, host: &[usize]) -> bool {
    let Elem::Handle(ty) = elem else {
        return true;
    };
    if dir == Slots::In {
        return true;
    }
    let mut ok = true;
    for (k, &v) in host.iter().enumerate() {
        if v == 0 && host.len() > 1 {
            continue;
        }
        let h = handles::to_guest(ty, v).unwrap_or_else(|| {
            ok = false;
            0
        });
        // SAFETY: slot `k` of the caller's writable slots.
        unsafe { (guest as *mut u32).add(k).write_unaligned(h) };
    }
    ok
}

/// A guest handle the table does not hold (never issued, freed, a raw address): the host function is not called, the answer is 0 (NULL / false / `EGL_FALSE`, the NDK's usual error value), counted and reported with the row and lane.
fn refuse_handle(symbol: &str, i: usize, value: usize) -> usize {
    super::dispatch::report_refused(
        super::dispatch::REFUSED_HANDLE,
        &format!("{symbol} parameter {i} = {value:#x}"),
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: Elem = Elem::Handle("S");
    const NO_LANES: [usize; 6] = [0; 6];

    /// `n` slots at `p` by a count lane (lane 0 = `n`).
    unsafe fn slots_in(elem: Elem, dir: Slots, p: usize, n: usize) -> Result<Vec<usize>, Refusal> {
        // SAFETY: the caller's `n` slots.
        unsafe { words_in(elem, dir, p, Len::Param(0, 1), &[n, 0, 0, 0, 0, 0]) }
    }

    /// `Ptr` elements: AAPCS32 data pointers are 4 bytes, and guest base 0 makes a guest address the host's, so 0x8000_0000 (a valid guest address below 4 GiB) zero-extends, never to -2^31; NULL stays 0.
    #[test]
    fn ptr_words_zero_extend() {
        let g = [0x0001_0000u32, 0x8000_0000, 0];
        // SAFETY: three readable slots.
        let host = unsafe { slots_in(Elem::Ptr, Slots::In, g.as_ptr() as usize, 3) };
        assert_eq!(host, Ok(vec![0x1_0000, 0x8000_0000, 0]));
    }

    /// `Attrib` elements: egl.h `typedef intptr_t EGLAttrib` (signed, 4 bytes on ILP32); `EGL_WIDTH` 0x3057, value -1, `EGL_NONE` 0x3038 (egl.h): the list ends at the key slot holding `EGL_NONE`, which is copied; -1 sign-extends.
    #[test]
    fn attrib_lists_sign_extend_and_end_at_egl_none() {
        let g = [0x3057u32, 0xFFFF_FFFF, 0x3038, 0x1234];
        // SAFETY: a terminated list.
        let host = unsafe {
            words_in(
                Elem::Attrib,
                Slots::In,
                g.as_ptr() as usize,
                Len::Terminated(0x3038),
                &NO_LANES,
            )
        };
        assert_eq!(host, Ok(vec![0x3057, 0xFFFF_FFFF_FFFF_FFFF, 0x3038]));
        // a value slot holding 0x3038 is not the end: only key slots are inspected
        let v = [0x3057u32, 0x3038, 0x3038];
        // SAFETY: as above.
        let host = unsafe {
            words_in(
                Elem::Attrib,
                Slots::In,
                v.as_ptr() as usize,
                Len::Terminated(0x3038),
                &NO_LANES,
            )
        };
        assert_eq!(host.map(|h| h.len()), Ok(3));
    }

    /// An attribute list with no terminator within the bound (bound + 1 pairs, every key `EGL_WIDTH`) is refused, never read past the bound.
    #[test]
    fn unterminated_attrib_list_is_refused() {
        let g = vec![0x3057u32; 2 * (MAX_WORDS + 1)];
        // SAFETY: bound + 1 readable pairs.
        let host = unsafe {
            words_in(
                Elem::Attrib,
                Slots::In,
                g.as_ptr() as usize,
                Len::Terminated(0x3038),
                &NO_LANES,
            )
        };
        assert_eq!(host, Err(Refusal::Unterminated));
    }

    /// A count times a factor: NdkMediaDataSource.h `AMediaDataSource_newUri(uri, numheaders, key_values)`, `key_values` holding 2 x `numheaders` strings: `numheaders` 2 reads 4 words; a count past the bound refuses before any allocation or read.
    #[test]
    fn scaled_count_and_bound() {
        let g = [1u32, 2, 3, 4, 5];
        let lanes = [0, 2, 0, 0, 0, 0];
        // SAFETY: five readable slots, four read.
        let host = unsafe {
            words_in(
                Elem::Ptr,
                Slots::In,
                g.as_ptr() as usize,
                Len::Param(1, 2),
                &lanes,
            )
        };
        assert_eq!(host, Ok(vec![1, 2, 3, 4]));
        // a negative GLsizei (-1 as the guest's word) reads as 2^32 - 1, past the bound: refused, nothing read
        let lanes = [0xFFFF_FFFF, 0, 0, 0, 0, 0];
        // SAFETY: never dereferenced (refused first).
        let host = unsafe { words_in(Elem::Ptr, Slots::In, 8, Len::Param(0, 1), &lanes) };
        assert_eq!(host, Err(Refusal::Bound(0xFFFF_FFFF)));
    }

    /// Handle slots by the headers' contracts: an out slot gets a handle naming the host object (a raw host address never reaches the guest: break `ilp32_raw_handles` fails here); an array (egl.h eglChooseConfig: `num_config` of `config_size` slots written) writes only the filled slots; an iterator cursor (NdkMediaCodecStore.h: keep the last value) crosses back in; a slot holding an unissued value refuses.
    #[test]
    fn handle_slots_issue_handles_and_refuse_forged_ones() {
        // host objects above 4 GiB, as a 64-bit host heap holds them
        let (a, b) = (0x7f00_0000_3000usize, 0x7f00_0000_4000usize);
        // one Out slot, guest garbage before the call
        let mut one = [0xdead_beefu32];
        // SAFETY: here and below, each array holds as many 4-byte slots as passed, readable and writable.
        let host = unsafe { slots_in(H, Slots::Out, one.as_ptr() as usize, 1) }.unwrap();
        assert_eq!(host, [0]);
        // SAFETY: as above.
        assert!(unsafe { words_out(H, Slots::Out, one.as_mut_ptr() as usize, &[a]) });
        assert_ne!(
            one[0] as usize,
            a & 0xffff_ffff,
            "a handle, not the truncated address"
        );
        assert_eq!(handles::to_host(one[0] as usize), Some(a));
        // InOut: the cursor crosses back in as the host object
        // SAFETY: as above.
        let back = unsafe { slots_in(H, Slots::InOut, one.as_ptr() as usize, 1) }.unwrap();
        assert_eq!(back, [a]);
        // array of 3, host filled 2: the third keeps the guest's value
        let mut arr = [0u32, 0, 7];
        // SAFETY: as above.
        assert!(unsafe { words_out(H, Slots::Out, arr.as_mut_ptr() as usize, &[a, b, 0]) });
        assert_eq!(arr[0], one[0], "one handle per live address");
        assert_eq!(handles::to_host(arr[1] as usize), Some(b));
        assert_eq!(arr[2], 7);
        // In: a forged slot refuses; In writes nothing back
        let forged = [arr[0], 0x1234u32];
        // SAFETY: as above.
        assert!(unsafe { slots_in(H, Slots::In, forged.as_ptr() as usize, 2) }.is_err());
        let good = [arr[0], arr[1]];
        assert_eq!(
            // SAFETY: as above.
            unsafe { slots_in(H, Slots::In, good.as_ptr() as usize, 2) }.unwrap(),
            [a, b]
        );
        for h in [arr[0], arr[1]] {
            handles::release(h as usize);
        }
    }
}
