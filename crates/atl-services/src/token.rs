// SPDX-License-Identifier: MIT
//! A bare binder object used as an identity token (an activity's token, as ActivityRecord.Token is a Binder in AOSP): it answers no transaction.

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, declare_binder_interface};

pub trait IToken: Interface {}

declare_binder_interface! {
    IToken["atl.Token"] {
        native: BnToken(on_transact),
        proxy: BpToken,
    }
}

fn on_transact(
    _: &dyn IToken,
    _: TransactionCode,
    _: &BorrowedParcel<'_>,
    _: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl IToken for BpToken {}
impl IToken for Binder<BnToken> {}

struct Token;
impl Interface for Token {}
impl IToken for Token {}

/// A new token.
pub fn new() -> SpIBinder {
    BnToken::new_binder(Token, BinderFeatures::default()).as_binder()
}
