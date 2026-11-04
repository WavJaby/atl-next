// SPDX-License-Identifier: MIT
//! The compositor registry service ([`crate::compositor_registry`] is the protocol): an app process hands over its in-process SurfaceFlinger objects, which are registered with servicemanager under names carrying the caller's uid.
//! The services run as the system uid, the only one servicemanager lets add services (patches/aosp/frameworks/native 0002); apps never get addService.

use std::io::Write;

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{
    BinderFeatures, Interface, SpIBinder, StatusCode, ThreadState, declare_binder_interface,
};

use crate::compositor_registry::{DESCRIPTOR, REGISTER, check, service_names};
use crate::counters;

/// The interface name counters use.
const INTERFACE: &str = "ICompositorRegistry";

pub trait ICompositorRegistry: Interface {}

declare_binder_interface! {
    ICompositorRegistry[DESCRIPTOR] {
        native: BnCompositorRegistry(on_transact),
        proxy: BpCompositorRegistry,
    }
}

/// REGISTER: the suffix, the two service binders; refused (PERMISSION_DENIED) unless the suffix is under the caller's uid.
fn on_transact(
    _: &dyn ICompositorRegistry,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    if code != REGISTER {
        counters::count(INTERFACE, &format!("code {code}"));
        return Err(StatusCode::UNKNOWN_TRANSACTION);
    }
    let suffix: String = data.read()?;
    let surface_flinger: SpIBinder = data.read()?;
    let surface_flinger_aidl: SpIBinder = data.read()?;
    let uid = ThreadState::get_calling_uid();
    if let Err(why) = check(uid, &suffix) {
        let _ = writeln!(
            std::io::stderr(),
            "services: registry refused uid {uid}: {why}"
        );
        return Err(StatusCode::PERMISSION_DENIED);
    }
    let (name, name_aidl) = service_names(&suffix);
    binder::add_service(&name, surface_flinger)?;
    binder::add_service(&name_aidl, surface_flinger_aidl)?;
    let _ = writeln!(
        std::io::stderr(),
        "services: registered {name}, {name_aidl}"
    );
    Ok(())
}

impl ICompositorRegistry for BpCompositorRegistry {}
impl ICompositorRegistry for Binder<BnCompositorRegistry> {}

struct Registry;
impl Interface for Registry {}
impl ICompositorRegistry for Registry {}

/// A new registry binder.
pub fn new_binder() -> SpIBinder {
    BnCompositorRegistry::new_binder(Registry, BinderFeatures::default()).as_binder()
}
