// SPDX-License-Identifier: MIT
//! The app's UI agent (plan 8c, M3 slice 5): the binder an app process gives the compositor registry in REGISTER (compositor_registry.rs), and the call the window service makes on it.
//! `IUiAgent` (descriptor `atl.IUiAgent`) has two transactions. WINDOW_SURFACE: the layer's name (String16) in, the window's SurfaceControl (codecs/android/view/SurfaceControl.rs, Part form) out.
//! INPUT_SERVER: a window's input channel server end (an fd, ParcelFileDescriptor) in, nothing out.
//! The app serves it (atl-composer `ui_agent`: the layer in its in-process compositor, the input server end to its input thread). This module holds the protocol on both sides and the table the registry fills per uid.
//! The window service calls it through the proxy; the server object is only ever a remote one here, so `Binder<BnUiAgent>` delegates to the native object like any AIDL interface.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use atl_parcelables::android::view::SurfaceControl;
use atl_parcelables::parts::Part;
use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, ParcelFileDescriptor, SpIBinder, StatusCode, declare_binder_interface};

use crate::counters;

/// The interface descriptor.
pub const DESCRIPTOR: &str = "atl.IUiAgent";
/// A window's SurfaceControl: the layer name in, the SurfaceControl out.
pub const WINDOW_SURFACE: TransactionCode = 1;
/// A window's input channel server end in; the agent publishes the window's input on it. Nothing out.
pub const INPUT_SERVER: TransactionCode = 2;
/// The interface name counters use.
const INTERFACE: &str = "IUiAgent";

pub trait IUiAgent: Interface {
    /// A window's SurfaceControl: a new layer named `layer_name` in the app's compositor (the layer's handle and client come with it).
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode>;
    /// A window's input channel server end: the app publishes the window's input on it from now on. The agent owns the fd.
    fn input_server(&self, fd: OwnedFd) -> Result<(), StatusCode>;
}

declare_binder_interface! {
    IUiAgent[DESCRIPTOR] {
        native: BnUiAgent(on_transact),
        proxy: BpUiAgent,
    }
}

fn on_transact(
    service: &dyn IUiAgent,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    match code {
        WINDOW_SURFACE => {
            let layer_name: String = data.read()?;
            let surface = service.window_surface(&layer_name)?;
            Part::write(&surface, reply)
        }
        INPUT_SERVER => {
            let fd: ParcelFileDescriptor = data.read()?;
            service.input_server(fd.into())
        }
        _ => {
            counters::count(INTERFACE, &format!("code {code}"));
            Err(StatusCode::UNKNOWN_TRANSACTION)
        }
    }
}

impl IUiAgent for BpUiAgent {
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(layer_name)?;
        let reply = binder.submit_transact(WINDOW_SURFACE, data, 0)?;
        <SurfaceControl as Part>::read(reply.borrowed_ref())
    }

    fn input_server(&self, fd: OwnedFd) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&ParcelFileDescriptor::new(fd))?;
        binder.submit_transact(INPUT_SERVER, data, 0)?;
        Ok(())
    }
}

impl IUiAgent for Binder<BnUiAgent> {
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
        self.0.window_surface(layer_name)
    }

    fn input_server(&self, fd: OwnedFd) -> Result<(), StatusCode> {
        self.0.input_server(fd)
    }
}

/// The UI agents the registry holds, by uid. One app per uid: a process that registers again replaces its uid's entry (a restarted app).
#[derive(Default)]
pub struct UiAgents {
    agents: Mutex<HashMap<u32, SpIBinder>>,
}

impl UiAgents {
    pub fn insert(&self, uid: u32, agent: SpIBinder) {
        self.lock().insert(uid, agent);
    }

    /// The agent registered for `uid`.
    pub fn get(&self, uid: u32) -> Option<SpIBinder> {
        self.lock().get(&uid).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u32, SpIBinder>> {
        // a poisoned table still holds the agents
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }
}
