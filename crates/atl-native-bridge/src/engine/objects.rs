// SPDX-License-Identifier: MIT
//! Proxy object tables (opensl-es plan 2.2 item 1): a guest object whose word 0 is a guest function table and word 1 the host object it stands for (`GuestObj`). The guest `JNIEnv` / `JavaVM` (jni.rs) and each OpenSL ES interface (sles.rs) are rows of one registry, generated from their headers (build.rs `object_tables`).
//! Every table slot is an object stub (arch `object_stub`) that crosses to the host object's own slot with only the object argument rewritten (identity `atl_nb_object_up`: `mov 8(%rdi)`), or a special's bound host function. One stub pool serves every table, so slot `k` of any table is stub `k`.
//! Under QEMU the trap names only the slot; the table is found from the guest object's word 0 ([`table_of`]), which also validates it: an address no registered table backs is refused, never dereferenced as a host object.

use std::sync::OnceLock;

use super::active;

mod slots {
    include!(concat!(env!("OUT_DIR"), "/object_tables.rs"));
}
pub use slots::{SL_IIDS, TABLES};
pub(super) use slots::{
    env, sl_android_configuration, sl_android_simple_buffer_queue, sl_buffer_queue, sl_engine,
    sl_object, sl_output_mix, sl_play, sl_prefetch_status, sl_seek, sl_volume, vm,
};

/// Per slot, the lane descriptor (build.rs `object_lanes_rs`): [`lane_of`] reads it.
mod lanes {
    include!(concat!(env!("OUT_DIR"), "/object_lanes.rs"));
}
/// Public: the classes of a QEMU binding's [`Sig`] (`Active::bind_result`).
pub use lanes::{Class, LANES, Lane};

/// A host function's lanes as a QEMU binding crosses them (dispatch.rs `call_host`): the result class, and the parameter classes from its prototype.
/// `params` None: no prototype, only for a `bind` caller (a libc row): the guest's lanes cross as they stand, right for a non-variadic callee with integer arguments. A variadic callee's double is misplaced on LP64D (read from an integer register), a float unboxed there. JNI calls always carry the prototype of their `...A` sibling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sig {
    pub ret: Class,
    pub params: Option<&'static [Class]>,
}

impl Sig {
    /// An integer result, no prototype: the whole 64-bit host register (a 32-bit guest's libc rows need their prototypes first, phase4-32bit P4b).
    pub const INT: Sig = Sig::ret(Class::Int {
        bits: 64,
        signed: true,
    });

    /// Result class `ret`, no prototype.
    pub const fn ret(ret: Class) -> Sig {
        Sig { ret, params: None }
    }
}

/// Index of a table in [`TABLES`].
pub type TableId = usize;
/// The JNI tables' registry indices (build.rs `JNI_TABLES` come first).
pub const ENV: TableId = 0;
pub const VM: TableId = 1;
const _: () = assert!(TABLES.len() == lanes::LANES.len());

/// Object stubs (bindings.rs `OBJECT_STUBS`): every table fits the one pool (unit test `every_table_fits_the_stub_pool`).
pub const OBJECT_STUBS: usize = super::bindings::OBJECT_STUBS;

/// A guest proxy object: what guest code's `(*obj)->Fn(obj, ...)` dereferences. Word 1's offset is fixed by `atl_nb_object_up` (`mov 8(%rdi)`).
/// Lives in guest-addressable memory ([`Placed`]), so a 32-bit guest holds it in its word; that guest reads only word 0's low half (little-endian), its table's address, which fits by construction (`Active::reserve`). The host reads both words at these offsets for every width.
#[repr(C)]
pub(super) struct GuestObj {
    /// the guest address of the table's function array ([`functions`])
    pub(super) functions: usize,
    pub(super) host: usize,
}
const _: () = assert!(std::mem::offset_of!(GuestObj, host) == 8);

/// A [`GuestObj`] in its own guest-addressable block (`Active::reserve`), given back on drop: the guest's `JNIEnv` per thread (record.rs), its `JavaVM`, each SL proxy (sles.rs).
/// Non-zero, so an `Option<Placed>` is one word with NULL for None (record.rs `HostThread`, which identity asm passes by pointer, keeps an FFI-safe layout).
#[repr(transparent)]
pub(super) struct Placed(std::num::NonZeroUsize);

impl Placed {
    /// A proxy of function array `functions` standing for `host`.
    pub(super) fn new(functions: usize, host: usize) -> Placed {
        let at = active()
            .reserve(
                std::mem::size_of::<GuestObj>(),
                (libc::PROT_READ | libc::PROT_WRITE) as u32,
            )
            .ok()
            .and_then(std::num::NonZeroUsize::new)
            .unwrap_or_else(|| super::unsupported("guest proxy object: no guest memory"));
        // SAFETY: `reserve` answered fresh writable bytes, page aligned, at least a `GuestObj`'s.
        unsafe { (at.get() as *mut GuestObj).write(GuestObj { functions, host }) };
        Placed(at)
    }

    /// The guest address of the object.
    pub(super) fn addr(&self) -> usize {
        self.0.get()
    }

    /// Points the object at host object `host` (ART may hand a thread a new env).
    pub(super) fn set_host(&mut self, host: usize) {
        // SAFETY: our block, a `GuestObj` since `new`.
        unsafe { (*(self.addr() as *mut GuestObj)).host = host };
    }

    /// Keeps the object for the process's life; its address.
    pub(super) fn leak(self) -> usize {
        let at = self.addr();
        std::mem::forget(self);
        at
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        active().release(self.addr(), std::mem::size_of::<GuestObj>());
    }
}

/// Each table's guest function array (its guest address), built once ([`functions`]).
static FUNCTIONS: [OnceLock<usize>; TABLES.len()] = [const { OnceLock::new() }; TABLES.len()];

/// The guest function array of table `t`, as its guest address: guest words (thread.rs `write_word`) in guest-addressable memory (`Active::reserve`), so a 32-bit guest's `(*env)->Fn` reads 4-byte slots. Slot i = object stub i (a slot with no function refuses by name when called, dispatch.rs `refuse`), or the special's host function, bound with the slot's own lanes as its prototype (a 32-bit guest's arguments are placed by class). Built on the first call; the owner of a table passes the same specials every time. `host`: a live host object of the table, read only by the `jni_direct` break.
pub(super) fn functions(t: TableId, specials: &[(usize, usize)], host: usize) -> usize {
    *FUNCTIONS[t].get_or_init(|| {
        let word = active().guest_pointer_bits() as usize / 8;
        let len = TABLES[t].1.len();
        let at = active()
            .reserve(len * word, (libc::PROT_READ | libc::PROT_WRITE) as u32)
            .unwrap_or_else(|e| super::unsupported(&format!("object table {}: {e}", TABLES[t].0)));
        for slot in 0..len {
            let f = match specials.iter().find(|&&(s, _)| s == slot) {
                Some(&(_, f)) => {
                    let sig = lanes::LANES[t][slot].map_or(Sig::INT, |l| Sig {
                        ret: l.ret,
                        params: Some(l.params),
                    });
                    active().bind_result(f, sig).unwrap_or_else(|| {
                        super::unsupported("object specials: host thunks used up")
                    })
                }
                None => direct_break(t, slot, host).unwrap_or_else(|| active().object_stub(slot)),
            };
            // SAFETY: slot `slot` of the `len` guest words reserved above.
            unsafe { super::thread::write_word((at + slot * word) as *mut u8, f) };
        }
        at
    })
}

/// Deliberate break of gate 4.4 (`--features break-jni_direct`): the env's `NewStringUTF` slot is the host function itself, no crossing -> the guest calls ART on its own thread pointer with its own env.
#[cfg(atl_nb_jni_direct)]
fn direct_break(t: TableId, slot: usize, host: usize) -> Option<usize> {
    // SAFETY: `host` is a live host JNIEnv; slot within its table.
    (t == ENV && slot == env::NewStringUTF && host != 0).then(|| unsafe { host_slot(host, slot) })
}
#[cfg(not(atl_nb_jni_direct))]
fn direct_break(_t: TableId, _slot: usize, _host: usize) -> Option<usize> {
    None
}

/// Slot `slot` of host object `obj`'s own function table.
///
/// # Safety
/// `obj` is a live host object whose word 0 is its function table (`JNIEnv*`, `JavaVM*`, an SL interface), `slot` within that table.
pub(super) unsafe fn host_slot(obj: usize, slot: usize) -> usize {
    // SAFETY: caller contract.
    unsafe { *(*(obj as *const *const usize)).add(slot) }
}

/// The host object behind guest proxy `guest` (its word 1).
///
/// # Safety
/// `guest` is a live guest proxy made with a [`functions`] array.
pub(super) unsafe fn host_of(guest: usize) -> usize {
    // SAFETY: caller contract.
    unsafe { (*(guest as *const GuestObj)).host }
}

/// The registered table behind guest object `guest`, by its word 0; `None` when no table built so far has that array.
///
/// # Safety
/// `guest` is readable for one word (a guest object pointer the guest passed in x0).
pub(super) unsafe fn table_of(guest: usize) -> Option<TableId> {
    if guest == 0 {
        return None;
    }
    // SAFETY: caller contract.
    let f = unsafe { *(guest as *const usize) };
    FUNCTIONS.iter().position(|a| a.get() == Some(&f))
}

/// Why a guest object call is refused (dispatch.rs `refuse`, the named abort of maintainer decision D-4).
pub(super) enum Refused {
    /// x0 is no registered proxy object.
    NotProxy { id: usize, guest: usize },
    /// The id is past the table of its object (`table` with `len` entries).
    Forged {
        id: usize,
        table: &'static str,
        len: usize,
    },
    /// A field with no function behind it (jni.h `reserved*`).
    Reserved { id: usize, table: &'static str },
    /// A `...V` variant with no shorty source: ART gives `getMethodShorty` at `initialize` (exports.rs); a caller of the engine that skipped it has none.
    NoShorty { name: &'static str },
    /// A `...V` variant whose method's shorty holds a character outside jni's `ZBSCIJFDL`.
    Shorty { name: &'static str, c: u8 },
    /// A `...` call whose variadic arguments run past the lanes' stack window (the guest's r0-r3 + stack on AAPCS32, x0..x7 + stack on AAPCS64): the words the method's shorty needs were not carried across.
    VariadicWindow { name: &'static str },
}

/// The lane descriptor of guest object call `id` made on guest object `guest`, and its table. Refused: no registered object, an id past the table, a slot with no function.
///
/// # Safety
/// As [`table_of`].
pub(super) unsafe fn lane_of(guest: usize, id: usize) -> Result<(TableId, &'static Lane), Refused> {
    // SAFETY: caller contract.
    let t = unsafe { table_of(guest) }.ok_or(Refused::NotProxy { id, guest })?;
    let (name, _) = TABLES[t];
    match lanes::LANES[t].get(id) {
        None => Err(Refused::Forged {
            id,
            table: name,
            len: lanes::LANES[t].len(),
        }),
        Some(None) => Err(Refused::Reserved { id, table: name }),
        Some(Some(lane)) => Ok((t, lane)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One stub pool serves every table: the largest table (JNINativeInterface, 229+ fields) bounds it.
    #[test]
    fn every_table_fits_the_stub_pool() {
        for (name, slots) in TABLES {
            assert!(slots.len() <= OBJECT_STUBS, "{name}: {} slots", slots.len());
        }
        assert_eq!(
            (TABLES[ENV].0, TABLES[VM].0),
            ("JNINativeInterface", "JNIInvokeInterface")
        );
    }

    /// OpenSLES.h `struct SLEngineItf_`: 15 methods, CreateLEDDevice first, CreateAudioPlayer third (BC's vtable offset +0x10); `SLObjectItf_`: Realize first, GetInterface at +0x18, Destroy at +0x30 (opensl-es plan section 3).
    #[test]
    fn sl_tables_follow_the_header() {
        let t = |n: &str| TABLES.iter().find(|(r, _)| *r == n).expect(n).1;
        let engine = t("SLEngineItf_");
        assert_eq!(
            (engine.len(), engine[0], engine[2]),
            (15, "CreateLEDDevice", "CreateAudioPlayer")
        );
        let object = t("SLObjectItf_");
        assert_eq!(
            (object[0], object[3], object[6]),
            ("Realize", "GetInterface", "Destroy")
        );
        assert!(
            SL_IIDS
                .iter()
                .any(|(iid, i)| *iid == "SL_IID_PLAY" && TABLES[*i].0 == "SLPlayItf_")
        );
    }
}
