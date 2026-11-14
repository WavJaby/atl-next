// SPDX-License-Identifier: MIT
//! `android.ui.ISurfaceComposer` ("SurfaceFlinger"): hand-written C++ in libgui (ISurfaceComposer.cpp), not AIDL, so the interface is declared here with libgui's descriptor and transaction code.
//! `setTransactionState` is the one call BLASTBufferQueue and SurfaceControl.Transaction make through it: the interface token, then a `TransactionState`, decoded by the core's codec.
//! `getSchedulingPolicy` (the other code in BnSurfaceComposer::onTransact) is counted like any call this service does not serve.

use std::sync::Arc;

use atl_libgui_parcel::transaction::{SET_TRANSACTION_STATE, SURFACE_COMPOSER_DESCRIPTOR};
use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode, declare_binder_interface};

use crate::host::Composer;

/// The interface name counters use.
const INTERFACE: &str = "ISurfaceComposer(ui)";

pub trait ISurfaceFlinger: Interface {
    /// BnSurfaceComposer::onTransact(SET_TRANSACTION_STATE) after the interface check: the state is queued for the next frame tick.
    fn set_transaction_state(&self, data: &BorrowedParcel<'_>) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    ISurfaceFlinger[SURFACE_COMPOSER_DESCRIPTOR] {
        native: BnSurfaceFlinger(on_transact),
        proxy: BpSurfaceFlinger,
    }
}

fn on_transact(
    service: &dyn ISurfaceFlinger,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    if code != SET_TRANSACTION_STATE {
        atl_diag::unknown_code(INTERFACE, code);
        return Err(StatusCode::UNKNOWN_TRANSACTION);
    }
    // its BAD_VALUE passes the reply hook once, with the cause host.rs set, as a generated on_transact's
    service.set_transaction_state(data).inspect_err(|e| {
        atl_diag::binder_reply(
            INTERFACE,
            Some("setTransactionState"),
            code,
            atl_diag::EX_TRANSACTION_FAILED,
            *e as i32,
        );
    })
}

impl ISurfaceFlinger for BpSurfaceFlinger {
    fn set_transaction_state(&self, _: &BorrowedParcel<'_>) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }
}

impl ISurfaceFlinger for Binder<BnSurfaceFlinger> {
    fn set_transaction_state(&self, data: &BorrowedParcel<'_>) -> Result<(), StatusCode> {
        self.0.set_transaction_state(data)
    }
}

struct SurfaceFlinger {
    composer: Arc<Composer>,
}

impl Interface for SurfaceFlinger {}

impl ISurfaceFlinger for SurfaceFlinger {
    fn set_transaction_state(&self, data: &BorrowedParcel<'_>) -> Result<(), StatusCode> {
        self.composer.set_transaction_state(data)
    }
}

/// The "SurfaceFlinger" service object of `composer`.
pub fn new_binder(composer: Arc<Composer>) -> SpIBinder {
    BnSurfaceFlinger::new_binder(SurfaceFlinger { composer }, BinderFeatures::default()).as_binder()
}
