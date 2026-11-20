// SPDX-License-Identifier: MIT
//! ILP32 handles (phase4-32bit P4b s4): a host object a 32-bit guest only names back (an `opaque` row position: `EGLDisplay`, `AAsset*`, `ALooper*`) crosses as a 32-bit handle, never as its address. One rule for every such value, whatever the address: a host pointer below 4 GiB is not passed raw "because it fits", so a guest cannot forge a host address -- only a handle this table issued resolves.
//! - One table for every type (a type tag per slot for the live counts, not checked on use: the NDK passes one object under several typedef names, `EGLImage` / `EGLImageKHR` / `GLeglImageOES`).
//! - One handle per live host address: the same object answers the same handle (`eglGetDisplay`, `AChoreographer_getInstance`), so the guest's comparisons hold.
//! - Freed by the calls that end the object (build.rs `HANDLE_RELEASES`, `LaneMap::Handle(_, true)`); reference-counted releases keep it (the address keeps its handle while it lives; reused, it answers the same handle again).
//! - A handle the table does not hold (never issued, freed, forged) resolves to nothing: the caller refuses the call and counts it (`native-bridge.ilp32-handle`), never a host crash. AOSP's NDK leaves a bad pointer undefined.
//! - Text results (`glGetString`, `AAsset_getName`) are copied once per distinct content into guest-addressable memory ([`intern`]): a stable address per string, as the NDK promises for static strings; a per-call buffer (`AAssetDir_getNextFileName`) gets the copy of its content, which stays valid longer than the host's.
//! - Thread safety: one mutex; a crossing holds it only to look up or insert.

use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Mutex;

/// Bits of a handle holding the slot index + 1 (0 is NULL); above them the slot's generation.
const INDEX_BITS: u32 = 20;
/// Generations cycle through 1..=GENERATIONS: a freed slot's next handle differs from every handle it issued in the last `GENERATIONS - 1` reuses; handles stay below 2^31 (a positive `int` if the guest casts one).
const GENERATIONS: u32 = (1 << (31 - INDEX_BITS)) - 1;
/// Slots at most: the index field's range.
const SLOTS: usize = (1 << INDEX_BITS) - 1;

struct Slot {
    /// the host object, 0 when free
    host: usize,
    ty: &'static str,
    generation: u32,
    /// a window copy of the object's read-only data (address, length), given back when the handle is freed ([`attach`])
    window: Option<(usize, usize)>,
}

#[derive(Default)]
struct Table {
    slots: Vec<Slot>,
    free: Vec<usize>,
    by_host: HashMap<usize, u32>,
}

impl Table {
    fn handle(&self, index: usize) -> u32 {
        (self.slots[index].generation << INDEX_BITS) | (index as u32 + 1)
    }

    /// The live slot index `h` names, if this table issued it and has not freed it.
    fn index(&self, h: u32) -> Option<usize> {
        let index = (h & ((1 << INDEX_BITS) - 1)).checked_sub(1)? as usize;
        let s = self.slots.get(index)?;
        (s.host != 0 && s.generation == h >> INDEX_BITS).then_some(index)
    }
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Table) -> R) -> R {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    f(t.get_or_insert_with(Table::default))
}

/// The handle of host object `host` of type `ty`: 0 for NULL; the live handle of this address if it has one; else a new one. `None` when every slot is in use (counted by the caller like any refusal).
pub(super) fn to_guest(ty: &'static str, host: usize) -> Option<u32> {
    if host == 0 {
        return Some(0);
    }
    // break `ilp32_raw_handles`: the address itself, refused (None, counted) at or above 4 GiB
    if cfg!(atl_nb_ilp32_raw_handles) {
        return u32::try_from(host).ok();
    }
    with(|t| t.issue(ty, host))
}

/// The handle of host object `host` for one callback's duration (`Down::Handle`): the live handle when the object has one (`false`: not the caller's to free -- the guest holds it, e.g. the `AAudioStream*` it opened), else a new one (`true`: the caller frees it with [`release`] when the callback returns, the object's life being the callback's -- choreographer.h `AChoreographerFrameCallbackData`, "only valid during the callback"). NULL -> `(0, false)`. `None` when every slot is in use.
pub(super) fn scoped(ty: &'static str, host: usize) -> Option<(u32, bool)> {
    if host == 0 || cfg!(atl_nb_ilp32_raw_handles) {
        return to_guest(ty, host).map(|h| (h, false));
    }
    with(|t| match t.by_host.get(&host) {
        Some(&h) => Some((h, false)),
        None => t.issue(ty, host).map(|h| (h, true)),
    })
}

impl Table {
    /// [`to_guest`] under the lock: the live handle of `host`, else a new one.
    fn issue(&mut self, ty: &'static str, host: usize) -> Option<u32> {
        let t = self;
        if let Some(&h) = t.by_host.get(&host) {
            return Some(h);
        }
        let index = match t.free.pop() {
            Some(i) => i,
            None if t.slots.len() < SLOTS => {
                t.slots.push(Slot {
                    host: 0,
                    ty,
                    generation: 0,
                    window: None,
                });
                t.slots.len() - 1
            }
            None => return None,
        };
        let s = &mut t.slots[index];
        s.host = host;
        s.ty = ty;
        s.generation = s.generation % GENERATIONS + 1;
        let h = t.handle(index);
        t.by_host.insert(host, h);
        Some(h)
    }
}

/// The host object handle `h` names: `Some(0)` for 0 (NULL passes as NULL); `None` for a handle this table does not hold.
pub(super) fn to_host(h: usize) -> Option<usize> {
    if h == 0 || cfg!(atl_nb_ilp32_raw_handles) {
        return Some(h);
    }
    let h = u32::try_from(h).ok()?;
    with(|t| t.index(h).map(|i| t.slots[i].host))
}

/// Unlinks handle `h` from its host address before the call that ends the object: the slot still resolves for that call ([`to_host`]), but the address, which the host may hand out again the moment it frees the object, gets a new handle from [`to_guest`] -- never the dying one. [`release`] frees the slot after the call.
pub(super) fn retire(h: usize) {
    let Ok(h) = u32::try_from(h) else { return };
    with(|t| {
        if let Some(i) = t.index(h) {
            let host = t.slots[i].host;
            if t.by_host.get(&host) == Some(&h) {
                t.by_host.remove(&host);
            }
        }
    });
}

/// Frees handle `h` after the call that ended its object; an unknown handle is ignored (the call was refused before). The address mapping goes only if it is still `h`'s ([`retire`]: a reused address may hold another handle by now).
pub(super) fn release(h: usize) {
    let Ok(h) = u32::try_from(h) else { return };
    let window = with(|t| {
        let i = t.index(h)?;
        let host = std::mem::take(&mut t.slots[i].host);
        if t.by_host.get(&host) == Some(&h) {
            t.by_host.remove(&host);
        }
        t.free.push(i);
        t.slots[i].window.take()
    });
    if let Some((at, len)) = window {
        super::active().release(at, len);
    }
}

/// The window copy attached to handle `h` ([`attach`]), if any.
pub(super) fn window(h: usize) -> Option<usize> {
    let h = u32::try_from(h).ok()?;
    with(|t| t.index(h).and_then(|i| t.slots[i].window).map(|(at, _)| at))
}

/// Attaches window copy `(at, len)` to live handle `h`: it lives until the handle is freed (the call ending the object, `HANDLE_RELEASES`). `false` (the window is the caller's to give back) when `h` is not live or already has one.
pub(super) fn attach(h: usize, at: usize, len: usize) -> bool {
    let Ok(h) = u32::try_from(h) else {
        return false;
    };
    with(|t| match t.index(h) {
        Some(i) if t.slots[i].window.is_none() => {
            t.slots[i].window = Some((at, len));
            true
        }
        _ => false,
    })
}

/// Live handles per type, the leak counter: a type whose count only grows over a run leaks host objects (or its release is not in build.rs `HANDLE_RELEASES`).
pub fn live() -> Vec<(&'static str, usize)> {
    with(|t| {
        let mut n: HashMap<&'static str, usize> = HashMap::new();
        for s in t.slots.iter().filter(|s| s.host != 0) {
            *n.entry(s.ty).or_default() += 1;
        }
        let mut v: Vec<_> = n.into_iter().collect();
        v.sort();
        v
    })
}

/// Arena chunk for interned text: one window allocation serves many strings.
const ARENA: usize = 64 << 10;

#[derive(Default)]
struct Strings {
    by_text: HashMap<Vec<u8>, usize>,
    /// (next free byte, end) of the current chunk
    chunk: (usize, usize),
}

static STRINGS: Mutex<Option<Strings>> = Mutex::new(None);

/// The guest-addressable copy of NUL-terminated host text `host` (NULL stays NULL): one copy per distinct content, from window memory ([`super::Active::reserve`], below 4 GiB for a 32-bit guest), never freed. `None` when the window allocator refuses.
///
/// # Safety
/// `host` is NULL or a readable NUL-terminated string.
pub(super) unsafe fn intern(host: usize) -> Option<usize> {
    if host == 0 {
        return Some(0);
    }
    // SAFETY: caller contract.
    let text = unsafe { CStr::from_ptr(host as *const libc::c_char) }.to_bytes_with_nul();
    let mut s = STRINGS.lock().unwrap_or_else(|e| e.into_inner());
    let s = s.get_or_insert_with(Strings::default);
    if let Some(&at) = s.by_text.get(text) {
        return Some(at);
    }
    if s.chunk.1 - s.chunk.0 < text.len() {
        let len = text.len().next_multiple_of(ARENA);
        let at = super::active()
            .reserve(len, (libc::PROT_READ | libc::PROT_WRITE) as u32)
            .ok()?;
        s.chunk = (at, at + len);
    }
    let at = s.chunk.0;
    // SAFETY: `at..at + len` lies in the current chunk, fresh window memory nothing else writes.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), at as *mut u8, text.len()) };
    s.chunk.0 += text.len();
    s.by_text.insert(text.to_vec(), at);
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle round trip, by the rules above: NULL <-> 0; one handle per live address; below 2^31; a freed handle and a raw address (a forged handle) resolve to nothing; a reused slot answers a new handle.
    #[test]
    fn handles_round_trip_and_refuse_what_they_did_not_issue() {
        // host addresses as a 64-bit host's heap holds them; one below 4 GiB, which still crosses as a handle
        let (a, b, low) = (
            0x7f00_0000_1000usize,
            0x7f00_0000_2000usize,
            0x1000_0000usize,
        );
        assert_eq!(to_guest("T", 0), Some(0));
        assert_eq!(to_host(0), Some(0));
        let ha = to_guest("T", a).unwrap();
        assert_eq!(to_guest("T", a), Some(ha), "one handle per live address");
        let hb = to_guest("U", b).unwrap();
        let hl = to_guest("U", low).unwrap();
        assert_ne!(hl as usize, low, "an address below 4 GiB is a handle too");
        assert!(ha < 1 << 31 && hb < 1 << 31);
        assert_eq!(
            (to_host(ha as usize), to_host(hb as usize)),
            (Some(a), Some(b))
        );
        assert_eq!(to_host(low), None, "a raw address is not a handle");
        assert_eq!(to_host(a), None, "a raw host address is not a handle");
        release(ha as usize);
        assert_eq!(to_host(ha as usize), None, "a freed handle");
        let again = to_guest("T", a).unwrap();
        assert_ne!(again, ha, "the reused slot carries a new generation");
        assert_eq!(to_host(again as usize), Some(a));
        assert!(live().iter().any(|&(t, n)| t == "U" && n >= 2));
        for h in [again, hb, hl] {
            release(h as usize);
        }
    }

    /// A callback argument's handle (`Down::Handle`): an object the guest already names keeps its handle and is not the callback's to free (AAudio.h error callback's `AAudioStream*`); one it does not is issued fresh and freed after (choreographer.h: frame callback data valid only during the callback), so a use after the callback is refused.
    #[test]
    fn a_callback_argument_borrows_a_live_handle_or_gets_a_scoped_one() {
        let (stream, data) = (0x7f00_0000_5000usize, 0x7f00_0000_6000usize);
        let held = to_guest("AAudioStream", stream).unwrap();
        assert_eq!(scoped("AAudioStream", stream), Some((held, false)));
        let (h, fresh) = scoped("AChoreographerFrameCallbackData", data).unwrap();
        assert!(fresh && h != 0);
        assert_eq!(to_host(h as usize), Some(data));
        release(h as usize);
        assert_eq!(to_host(h as usize), None, "used after its callback");
        assert_eq!(scoped("T", 0), Some((0, false)));
        release(held as usize);
    }

    /// The call that ends an object (`AAsset_close`) and another thread's call answering the same address (the allocator reused it) race: the second gets a new handle, the closing call still resolves its own, and freeing the old one leaves the new one live.
    #[test]
    fn an_address_reused_while_its_object_is_ending_gets_a_new_handle() {
        let addr = 0x7f00_0000_7000usize;
        let old = to_guest("AAsset", addr).unwrap();
        retire(old as usize);
        assert_eq!(
            to_host(old as usize),
            Some(addr),
            "the ending call still resolves"
        );
        let new = to_guest("AAsset", addr).unwrap();
        assert_ne!(new, old, "a reused address never gets the dying handle");
        release(old as usize);
        assert_eq!(to_host(old as usize), None);
        assert_eq!(
            to_host(new as usize),
            Some(addr),
            "the new object's handle survives"
        );
        assert_eq!(to_guest("AAsset", addr), Some(new));
        release(new as usize);
    }
}
