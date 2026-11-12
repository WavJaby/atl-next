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
/// Public: the result class a QEMU binding is bound with (`Active::bind_result`).
pub use lanes::{Class, LANES, Lane};

/// Index of a table in [`TABLES`].
pub type TableId = usize;
/// The JNI tables' registry indices (build.rs `JNI_TABLES` come first).
pub const ENV: TableId = 0;
pub const VM: TableId = 1;
const _: () = assert!(TABLES.len() == lanes::LANES.len());

/// Object stubs (bindings.rs `OBJECT_STUBS`): every table fits the one pool (unit test `every_table_fits_the_stub_pool`).
pub const OBJECT_STUBS: usize = super::bindings::OBJECT_STUBS;

/// A guest proxy object: what guest code's `(*obj)->Fn(obj, ...)` dereferences. Word 1's offset is fixed by `atl_nb_object_up` (`mov 8(%rdi)`).
#[repr(C)]
pub(super) struct GuestObj {
    pub(super) functions: *const usize,
    pub(super) host: usize,
}
const _: () = assert!(std::mem::offset_of!(GuestObj, host) == 8);

/// Each table's guest function array, built once ([`functions`]).
static FUNCTIONS: [OnceLock<Box<[usize]>>; TABLES.len()] =
    [const { OnceLock::new() }; TABLES.len()];

/// The guest function array of table `t`: slot i = object stub i (a slot with no function refuses by name when called, dispatch.rs `refuse`), or the special's host function bound for it. Built on the first call; the owner of a table passes the same specials every time. `host`: a live host object of the table, read only by the `jni_direct` break.
pub(super) fn functions(t: TableId, specials: &[(usize, usize)], host: usize) -> *const usize {
    FUNCTIONS[t]
        .get_or_init(|| {
            (0..TABLES[t].1.len())
                .map(|slot| {
                    if let Some(&(_, f)) = specials.iter().find(|&&(s, _)| s == slot) {
                        return active().bind(f).unwrap_or_else(|| {
                            super::unsupported("object specials: host thunks used up")
                        });
                    }
                    direct_break(t, slot, host).unwrap_or_else(|| active().object_stub(slot))
                })
                .collect()
        })
        .as_ptr()
}

/// Deliberate break of gate 4.4 (`ATL_NB_BREAK=jni_direct`): the env's `NewStringUTF` slot is the host function itself, no crossing -> the guest calls ART on its own thread pointer with its own env.
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
    FUNCTIONS
        .iter()
        .position(|a| a.get().is_some_and(|a| a.as_ptr() as usize == f))
}

/// Why a guest object call is refused (dispatch.rs `refuse`, the named abort of owner decision D-4).
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
