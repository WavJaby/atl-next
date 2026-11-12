// SPDX-License-Identifier: MIT
//! Parcel squashing (os/Parcel.java allowSquashing :2895, maybeWriteSquashed :2962, readSquashed :3010): inside a value that allows it (PackageInfo, ProviderInfoList writeToParcel), a later write of an
//! ApplicationInfo already written to the same parcel is one int, the offset from the byte after that int back to the first copy's body; a first write is int 0, then the body. ApplicationInfo is the only class
//! AOSP squashes (its writeToParcel is the only maybeWriteSquashed caller).
//! Java keeps the tables on the Parcel object; a `BorrowedParcel` has no room for them, so they live here per thread, keyed by the parcel's address, for the outermost squashing value being read or written.
//! - Read: a first copy is remembered at the position after its 0; a back-reference resolves to a clone of the copy at `position - offset`, else is refused (counted): one outside a squashing value,
//!   one naming no remembered copy.
//! - Write: Java squashes the same instance; Rust values have no identity, so a body byte-equal to one already written is squashed. Readers resolve both to equal values. A body that holds a binder or
//!   descriptor object (a meta-data Bundle with one) has no bytes to compare and is written in full.

use std::cell::RefCell;

use binder::binder_impl::Parcel;
use binder::unstable_api::AsNative;

use crate::codecs::android::content::pm::ApplicationInfo::ApplicationInfo;
use crate::parcel::*;

/// The tables of one squashing value on one parcel.
struct Scope {
    parcel: Key,
    /// first copies read: position after their 0 -> value
    read: Vec<(i32, ApplicationInfo)>,
    /// first copies written: position after their 0 -> body bytes
    written: Vec<(i32, Vec<u8>)>,
}

thread_local! {
    static SCOPES: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
}

/// A parcel's identity for the tables: its address.
#[derive(Clone, Copy, PartialEq)]
pub struct Key(usize);

pub fn key(p: &In<'_>) -> Key {
    Key(p.as_native() as usize)
}

/// Pops the scope `allow` pushed, on every path out of it.
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        SCOPES.with(|s| s.borrow_mut().pop());
    }
}

/// `f` with squashing allowed on the parcel `parcel` ([`key`]): Parcel.allowSquashing ... restoreAllowSquashing around a value's write, and the read of the same value. Nested on the same parcel, the outer tables stay in use.
pub fn allow<R>(parcel: Key, f: impl FnOnce() -> Result<R>) -> Result<R> {
    let nested = SCOPES.with(|s| s.borrow().last().is_some_and(|x| x.parcel == parcel));
    let _opened = (!nested).then(|| {
        SCOPES.with(|s| {
            s.borrow_mut().push(Scope {
                parcel,
                read: Vec::new(),
                written: Vec::new(),
            })
        });
        Opened
    });
    f()
}

/// `g` on the open scope of `p`, if any.
fn with_scope<R>(p: &In<'_>, g: impl FnOnce(&mut Scope) -> R) -> Option<R> {
    let parcel = key(p);
    // the tables are per thread; a parcel address reused by a later parcel never meets an open scope of the earlier one, which `allow` closes on return
    SCOPES.with(|s| {
        s.borrow_mut()
            .last_mut()
            .filter(|x| x.parcel == parcel)
            .map(g)
    })
}

/// Parcel.readSquashed(ApplicationInfo::new): the offset int, then a body (`read_body`) or the earlier copy it names.
pub fn read(
    p: &In<'_>,
    read_body: impl FnOnce(&In<'_>) -> Result<ApplicationInfo>,
) -> Result<ApplicationInfo> {
    let offset = r_i32(p)?;
    let pos = p.get_data_position();
    if offset == 0 {
        let v = read_body(p)?;
        with_scope(p, |s| s.read.push((pos, v.clone())));
        return Ok(v);
    }
    let first = pos.checked_sub(offset);
    with_scope(p, |s| {
        s.read
            .iter()
            .find(|(at, _)| Some(*at) == first)
            .map(|(_, v)| v.clone())
    })
    .ok_or_else(|| {
        crate::refuse(
            "android.content.pm.ApplicationInfo (back-reference outside a squashing value)",
        )
    })?
    .ok_or_else(|| {
        crate::refuse("android.content.pm.ApplicationInfo (back-reference to no earlier copy)")
    })
}

/// Parcel.maybeWriteSquashed + the body (`write_body`) on a first write.
pub fn write(p: &mut In<'_>, write_body: impl FnOnce(&mut In<'_>) -> Result<()>) -> Result<()> {
    if with_scope(p, |_| ()).is_none() {
        w_i32(p, 0)?;
        return write_body(p);
    }
    let mut body = Parcel::new();
    write_body(&mut body.borrowed())?;
    let bytes = body.marshal().ok();
    let pos = p.get_data_position();
    let first = bytes.as_ref().and_then(|b| {
        with_scope(p, |s| {
            s.written.iter().find(|(_, w)| w == b).map(|(at, _)| *at)
        })
        .flatten()
    });
    if let Some(first) = first {
        // "the offset from the next byte of this integer" (Parcel.java:2977)
        let offset = pos
            .checked_sub(first)
            .and_then(|d| d.checked_add(4))
            .ok_or(binder::StatusCode::BAD_VALUE)?;
        return w_i32(p, offset);
    }
    w_i32(p, 0)?;
    if let Some(b) = bytes {
        let at = p.get_data_position();
        with_scope(p, |s| s.written.push((at, b)));
    }
    p.append_from(&body, 0, body.get_data_size())
}
