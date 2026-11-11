// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/core/java/android/content/ContentProviderNative.java, frameworks/base/core/java/android/content/IContentProvider.java
//! The settings provider stub (M2; AOSP SettingsProvider is plan phase 5): an IContentProvider whose `call` returns a null Bundle; every other transaction is counted unimplemented.
//! IContentProvider is a hand-written Java interface (ContentProviderNative.onTransact), not AIDL, so this binder is declared here; its descriptor and CALL_TRANSACTION code are AOSP's.

use binder::binder_impl::{Binder, BorrowedParcel, FIRST_CALL_TRANSACTION, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, declare_binder_interface};

use atl_services_types::counters;

/// IContentProvider.descriptor ("android.content.IContentProvider", IContentProvider.java).
pub const DESCRIPTOR: &str = "android.content.IContentProvider";
/// IContentProvider.CALL_TRANSACTION = IBinder.FIRST_CALL_TRANSACTION + 20 (IContentProvider.java:217).
pub const CALL_TRANSACTION: TransactionCode = FIRST_CALL_TRANSACTION + 20;
/// The interface name counters use.
const INTERFACE: &str = "IContentProvider";
/// Parcel.writeBundle(null) writes length -1 (os/Parcel.java writeBundle).
const NULL_BUNDLE: i32 = -1;
/// Parcel.writeNoException: status 0.
const NO_EXCEPTION: i32 = 0;

pub trait IContentProvider: Interface {}

declare_binder_interface! {
    IContentProvider["android.content.IContentProvider"] {
        native: BnContentProvider(on_transact),
        proxy: BpContentProvider,
    }
}

/// ContentProviderNative.onTransact:296-313 for CALL_TRANSACTION: after the interface check (libbinder_ndk's, as data.enforceInterface) it reads AttributionSource, authority, method,
/// arg and extras, calls `call`, writes no exception and the result Bundle. The stub's `call` returns null whatever the arguments, so they are not read.
fn on_transact(
    _: &dyn IContentProvider,
    code: TransactionCode,
    _data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    if code == CALL_TRANSACTION {
        reply.write(&NO_EXCEPTION)?;
        return reply.write(&NULL_BUNDLE);
    }
    counters::count_unimplemented(INTERFACE, &format!("code {code}"));
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl IContentProvider for BpContentProvider {}
impl IContentProvider for Binder<BnContentProvider> {}

struct Settings;
impl Interface for Settings {}
impl IContentProvider for Settings {}

/// A new settings provider binder.
pub fn new_binder() -> SpIBinder {
    BnContentProvider::new_binder(Settings, BinderFeatures::default()).as_binder()
}
