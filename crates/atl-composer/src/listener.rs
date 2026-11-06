// SPDX-License-Identifier: MIT
//! The calls back into libgui's `TransactionCompletedListener` (hand-written C++, `android.gui.ITransactionComposerListener`: raw one-way transactions, no AIDL). The interface exists here only to make a
//! proxy: libbinder_rs's `prepare_transact` needs a class associated with the binder, which `into_interface` does (and checks the descriptor against the listener's own).

use atl_compositor::callbacks::LISTENER_DESCRIPTOR;
use atl_libgui_parcel::parcel::WireParcel;
use binder::binder_impl::{Binder, BorrowedParcel, FLAG_ONEWAY, IBinderInternal, TransactionCode};
use binder::{Interface, SpIBinder, StatusCode, Strong, declare_binder_interface};

use crate::bridge::write_body;

pub trait ITransactionCompletedListener: Interface {}

declare_binder_interface! {
    ITransactionCompletedListener[LISTENER_DESCRIPTOR] {
        native: BnTransactionCompletedListener(on_transact),
        proxy: BpTransactionCompletedListener,
    }
}

/// The listener lives in libgui; this side never serves it.
fn on_transact(
    _: &dyn ITransactionCompletedListener,
    _: TransactionCode,
    _: &BorrowedParcel<'_>,
    _: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl ITransactionCompletedListener for BpTransactionCompletedListener {}
impl ITransactionCompletedListener for Binder<BnTransactionCompletedListener> {}

/// Sends `wire` (a call the core encoded: interface token first) to `listener` as one-way transaction `code`; binders in it resolve through `resolve`.
pub fn call(
    listener: SpIBinder,
    code: TransactionCode,
    wire: &WireParcel,
    resolve: &dyn Fn(u64) -> Option<SpIBinder>,
) -> Result<(), StatusCode> {
    let proxy: Strong<dyn ITransactionCompletedListener> = listener.into_interface()?;
    let binder = proxy.as_binder();
    // prepare_transact writes the token; the core's parcel carries its own, so skip it
    let mut reader = wire.reader();
    reader
        .enforce_interface(LISTENER_DESCRIPTOR)
        .map_err(|_| StatusCode::BAD_VALUE)?;
    let mut parcel = binder.prepare_transact()?;
    write_body(&mut parcel, wire, reader.position(), resolve)?;
    binder.submit_transact(code, parcel, FLAG_ONEWAY)?;
    Ok(())
}
