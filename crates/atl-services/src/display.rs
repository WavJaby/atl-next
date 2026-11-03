// SPDX-License-Identifier: MIT
//! The `display` service (Context.DISPLAY_SERVICE): registered so DisplayManagerGlobal.getInstance() is not null; ActivityThread's ClientTransactionListenerController requires it before any activity transaction runs.
//! Every transaction is counted and answered UNKNOWN_TRANSACTION (an empty reply: Java's proxy reads no exception and a null / zero result), so no display exists yet. An activity launch makes these calls (measured by `xtask test app`):
//! `getDisplayInfo` (code 1) 7 times, `getPreferredWideGamutColorSpaceId` (41) and `getOverlaySupport` (55) once each; the codes are IDisplayManager.aidl's declaration order from FIRST_CALL_TRANSACTION.
//! Declared here, like settings.rs's IContentProvider, and not as an AIDL stub: the converter cannot compile IDisplayManager's import closure yet (the stable HAL interface android.hardware.graphics.common, whose frozen `aidl_api/<n>/` copies its scan of an include directory takes as sources, and android.view.Surface).
//! It becomes the stub's implementation, answering from the platform layer's display, when the converter compiles that closure.

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, declare_binder_interface};

use crate::counters;

/// IDisplayManager's descriptor (IDisplayManager.aidl: package android.hardware.display).
pub const DESCRIPTOR: &str = "android.hardware.display.IDisplayManager";
/// The interface name counters use.
const INTERFACE: &str = "IDisplayManager";

pub trait IDisplayManager: Interface {}

declare_binder_interface! {
    IDisplayManager["android.hardware.display.IDisplayManager"] {
        native: BnDisplayManager(on_transact),
        proxy: BpDisplayManager,
    }
}

/// The method names of the codes an activity launch makes, for the counters; any other code is counted by number.
fn method(code: TransactionCode) -> String {
    match code {
        1 => "getDisplayInfo".to_owned(),
        41 => "getPreferredWideGamutColorSpaceId".to_owned(),
        55 => "getOverlaySupport".to_owned(),
        _ => format!("code {code}"),
    }
}

fn on_transact(
    _: &dyn IDisplayManager,
    code: TransactionCode,
    _data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    counters::count(INTERFACE, &method(code));
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl IDisplayManager for BpDisplayManager {}
impl IDisplayManager for Binder<BnDisplayManager> {}

struct DisplayManager;
impl Interface for DisplayManager {}
impl IDisplayManager for DisplayManager {}

/// A new display service binder.
pub fn new_binder() -> SpIBinder {
    BnDisplayManager::new_binder(DisplayManager, BinderFeatures::default()).as_binder()
}
