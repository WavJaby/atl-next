// SPDX-License-Identifier: MIT
//! The compositor registry service (compositor_registry.rs is the protocol). REGISTER takes the suffix, the two service binders and the app's UI agent (ui_agent.rs). The names are checked
//! against the caller's uid, the services go to servicemanager, and the agent goes to the table the window service reads: constructor injection into [`Registry`], so the table is not part of the interface.
//! The services run as the system uid, the only one servicemanager lets add services (patches/aosp/frameworks/native 0002); apps never get addService.

use std::io::Write;
use std::sync::Arc;

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{
    BinderFeatures, Interface, SpIBinder, StatusCode, ThreadState, declare_binder_interface,
};

use crate::compositor_registry::{DESCRIPTOR, REGISTER, SET_BOUNDS, check, service_names};
use crate::counters;
use crate::tasks::{TaskSize, Tasks};
use crate::ui_agent::UiAgents;

/// The interface name counters use.
const INTERFACE: &str = "ICompositorRegistry";

pub trait ICompositorRegistry: Interface {
    /// REGISTER once its arguments are read: `suffix` must be under `caller_uid` (compositor_registry::check). On success the two services and the agent are kept under it.
    fn register(
        &self,
        caller_uid: u32,
        suffix: &str,
        surface_flinger: SpIBinder,
        surface_flinger_aidl: SpIBinder,
        ui_agent: SpIBinder,
    ) -> Result<(), StatusCode>;

    /// SET_BOUNDS once its arguments are read: the caller's Task is `width` x `height` physical pixels (tasks.rs). A non-positive size is refused; a size change notifies the Task's windows.
    fn set_bounds(&self, caller_uid: u32, width: i32, height: i32) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    ICompositorRegistry[DESCRIPTOR] {
        native: BnCompositorRegistry(on_transact),
        proxy: BpCompositorRegistry,
    }
}

/// REGISTER: the suffix, the two service binders, the UI agent; the calling uid is taken from the transaction.
fn on_transact(
    service: &dyn ICompositorRegistry,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    _reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    if code == SET_BOUNDS {
        let width: i32 = data.read()?;
        let height: i32 = data.read()?;
        return service.set_bounds(ThreadState::get_calling_uid(), width, height);
    }
    if code != REGISTER {
        counters::count(INTERFACE, &format!("code {code}"));
        return Err(StatusCode::UNKNOWN_TRANSACTION);
    }
    let suffix: String = data.read()?;
    let surface_flinger: SpIBinder = data.read()?;
    let surface_flinger_aidl: SpIBinder = data.read()?;
    let ui_agent: SpIBinder = data.read()?;
    service.register(
        ThreadState::get_calling_uid(),
        &suffix,
        surface_flinger,
        surface_flinger_aidl,
        ui_agent,
    )
}

impl ICompositorRegistry for BpCompositorRegistry {
    fn set_bounds(&self, _: u32, _: i32, _: i32) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }

    fn register(
        &self,
        _: u32,
        _: &str,
        _: SpIBinder,
        _: SpIBinder,
        _: SpIBinder,
    ) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }
}

impl ICompositorRegistry for Binder<BnCompositorRegistry> {
    fn set_bounds(&self, caller_uid: u32, width: i32, height: i32) -> Result<(), StatusCode> {
        self.0.set_bounds(caller_uid, width, height)
    }

    fn register(
        &self,
        caller_uid: u32,
        suffix: &str,
        surface_flinger: SpIBinder,
        surface_flinger_aidl: SpIBinder,
        ui_agent: SpIBinder,
    ) -> Result<(), StatusCode> {
        self.0.register(
            caller_uid,
            suffix,
            surface_flinger,
            surface_flinger_aidl,
            ui_agent,
        )
    }
}

struct Registry {
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks>,
}

impl Interface for Registry {}

impl ICompositorRegistry for Registry {
    fn set_bounds(&self, caller_uid: u32, width: i32, height: i32) -> Result<(), StatusCode> {
        if width <= 0 || height <= 0 {
            let _ = writeln!(
                std::io::stderr(),
                "services: bounds refused uid {caller_uid}: {width}x{height}"
            );
            return Err(StatusCode::BAD_VALUE);
        }
        if self.tasks.set_size(caller_uid, TaskSize { width, height }) {
            let _ = writeln!(
                std::io::stderr(),
                "services: task of uid {caller_uid} bounds {width}x{height}"
            );
            let windows = crate::window::notify_resized(&self.tasks, caller_uid);
            let _ = writeln!(
                std::io::stderr(),
                "services: resized {windows} windows of uid {caller_uid}"
            );
        }
        Ok(())
    }

    fn register(
        &self,
        caller_uid: u32,
        suffix: &str,
        surface_flinger: SpIBinder,
        surface_flinger_aidl: SpIBinder,
        ui_agent: SpIBinder,
    ) -> Result<(), StatusCode> {
        if let Err(why) = check(caller_uid, suffix) {
            let _ = writeln!(
                std::io::stderr(),
                "services: registry refused uid {caller_uid}: {why}"
            );
            return Err(StatusCode::PERMISSION_DENIED);
        }
        let (name, name_aidl) = service_names(suffix);
        binder::add_service(&name, surface_flinger)?;
        binder::add_service(&name_aidl, surface_flinger_aidl)?;
        self.agents.insert(caller_uid, ui_agent);
        let _ = writeln!(
            std::io::stderr(),
            "services: registered {name}, {name_aidl}"
        );
        Ok(())
    }
}

/// A new registry binder. `agents` is the table the window service reads (system_services.rs gives both the same one).
pub fn new_binder(agents: Arc<UiAgents>, tasks: Arc<Tasks>) -> SpIBinder {
    BnCompositorRegistry::new_binder(Registry { agents, tasks }, BinderFeatures::default())
        .as_binder()
}
