// SPDX-License-Identifier: MIT
//! The app's UI agent (plan 8c, M3 slice 5): the binder an app process gives the compositor registry in REGISTER (compositor_registry.rs), and the call the window service makes on it.
//! `IUiAgent` (descriptor `atl.IUiAgent`) has eleven transactions. MINIMIZE (oneway, from onBackPressed's moveTaskToBack): nothing in; the agent minimizes its window. DIM_LAYER: a dimming window's layer id, dim amount (f32), dim width and height (i32) in, nothing out; the agent keeps a black colour layer
//! just below that layer (WMS Dimmer), amount <= 0 drops it. POST_DRAW_TRANSACTION: a window's post-draw Transaction (finishDrawing), its bytes and objects, in; the composer applies it. INPUT_METHOD: nothing in, the app's in-app input method (an IInputMethod binder, atl-ime) out, or null; IMMS-lite binds it where AOSP binds an IME service (ime_bind.rs). POINTER_ICON (oneway, from the input service's setPointerIcon): the icon type (i32, PointerIconType) in; the agent sets its window's cursor. WINDOW_SURFACE: the layer's name (String16) in, the window's SurfaceControl (codecs/android/view/SurfaceControl.rs, Part form) out.
//! WINDOW_POSITION: a window's layer id (i32) and its frame's origin in its Task (x, y: i32) in, nothing out; the agent sets the layer's position, as WMS does on the window's SurfaceControl (WindowState.updateSurfacePosition -> Transaction.setPosition).
//! INPUT_SERVER: a window's input channel token (strong binder, the token both channel ends carry: AOSP keys a dispatcher connection by it) and server end (an fd, ParcelFileDescriptor) and the window's LayoutParams flags (i32) in, nothing out.
//! REMOVE_INPUT: a removed window's input channel token in, nothing out: the agent drops that channel (WindowState.disposeInputChannel -> InputDispatcher::removeInputChannel). TASK: the window's Task number (i32, tasks.rs) in, nothing out; the agent names it in its SET_BOUNDS.
//! The app serves it (atl-composer `ui_agent`: the layer in its in-process compositor, the input server end to its input thread). This module holds the protocol on both sides; the table the registry fills per uid is atl-services `ui_agents`.
//! The window service calls it through the proxy; `Binder<BnUiAgent>` delegates to the native object like any AIDL interface.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use atl_parcelables::android::view::{SurfaceControl, SurfaceControl_Transaction};
use atl_parcelables::codecs::android::view::SurfaceControl_Transaction::TransactionObject;
use atl_parcelables::parts::Part;
use binder::binder_impl::{Binder, BorrowedParcel, IBinderInternal, TransactionCode};
use binder::{Interface, ParcelFileDescriptor, SpIBinder, StatusCode, declare_binder_interface};

/// The interface descriptor.
pub const DESCRIPTOR: &str = "atl.IUiAgent";
/// A window's SurfaceControl: the layer name in, the SurfaceControl out.
pub const WINDOW_SURFACE: TransactionCode = 1;
/// A window's input channel token and server end in; the agent publishes the window's input on it. Nothing out.
pub const INPUT_SERVER: TransactionCode = 2;
/// A window's Task number in (TaskId::raw); nothing out. Sent when the window is added: the agent holds the number for its SET_BOUNDS.
pub const TASK: TransactionCode = 3;
/// A removed window's input channel token in; the agent drops that channel. Nothing out.
pub const REMOVE_INPUT: TransactionCode = 4;
/// A pointer icon type in (oneway: the app's own setPointerIcon call is waiting on the input service meanwhile).
pub const POINTER_ICON: TransactionCode = 5;
/// A window's layer id and frame origin in; the agent places the layer there. Nothing out.
pub const WINDOW_POSITION: TransactionCode = 6;
/// Nothing in; the app's IInputMethod (null when it has none) out.
pub const INPUT_METHOD: TransactionCode = 7;
/// A post-draw Transaction in (its bytes, then each object: offset, kind, the binder or fd); the agent's composer applies it. Nothing out.
pub const POST_DRAW_TRANSACTION: TransactionCode = 8;
/// A dimming window's layer id, the dim amount (f32) and the dim bounds' width and height (i32, from the Task's origin) in; nothing out. Amount <= 0 drops the window's dim layer.
pub const DIM_LAYER: TransactionCode = 9;
/// A window's input channel token and its frame in its Task (left, top, right, bottom: i32) in; nothing out. The agent's dispatcher hit-tests and translates that window's pointer events with it.
pub const INPUT_FRAME: TransactionCode = 10;
/// Nothing in (oneway): the app's Task went to the back (moveTaskToBack), so the agent minimizes its window.
pub const MINIMIZE: TransactionCode = 11;
/// The interface name counters use.
const INTERFACE: &str = "IUiAgent";

pub trait IUiAgent: Interface {
    /// A window's SurfaceControl: a new layer named `layer_name` in the app's compositor (the layer's handle and client come with it).
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode>;
    /// A window's input channel, by its token, and its server end: the app publishes the window's input on it from now on. The agent owns the fd. `flags` = the window's LayoutParams flags: the agent's dispatcher skips a NOT_TOUCHABLE window for touches and a NOT_FOCUSABLE one for focus.
    fn input_server(&self, token: SpIBinder, fd: OwnedFd, flags: i32) -> Result<(), StatusCode>;
    /// The window of input channel `token` is removed: the agent drops the channel, and input goes to the windows left.
    fn remove_input(&self, token: SpIBinder) -> Result<(), StatusCode>;
    /// The frame (left, top, right, bottom, in its Task) of the window of input channel `token` (WindowInfo.frame and transform).
    fn input_frame(&self, token: SpIBinder, frame: [i32; 4]) -> Result<(), StatusCode>;
    /// The Task the app's window belongs to, by its number (tasks.rs TaskId::raw): the app's bounds go to it.
    fn task(&self, task: i32) -> Result<(), StatusCode>;
    /// The pointer icon the app's view hierarchy resolved (PointerIconType value): the app's window shows it as its cursor.
    fn pointer_icon(&self, kind: i32) -> Result<(), StatusCode>;
    /// The app's Task went to the back (ActivityClientController.moveActivityTaskToBack): its window is minimized and the app keeps running.
    fn minimize(&self) -> Result<(), StatusCode>;
    /// The window of layer `layer_id` has its frame at (`x`, `y`) in its Task: its layer is placed there.
    fn window_position(&self, layer_id: i32, x: i32, y: i32) -> Result<(), StatusCode>;
    /// The app's in-app input method (IInputMethod), None when it serves none.
    fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode>;
    /// A window's post-draw Transaction (IWindowSession.finishDrawing), as libgui parcelled it: the app's composer applies it, as WMS applies it to SurfaceFlinger.
    fn post_draw_transaction(&self, t: &SurfaceControl_Transaction) -> Result<(), StatusCode>;
    /// FLAG_DIM_BEHIND (WMS Dimmer): a black colour layer just below the window of layer `layer_id`, alpha `amount`, covering `width` x `height` from the Task's origin; `amount` <= 0 drops it.
    fn dim_layer(
        &self,
        layer_id: i32,
        amount: f32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode>;
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
            let token: SpIBinder = data.read()?;
            let fd: ParcelFileDescriptor = data.read()?;
            let flags: i32 = data.read()?;
            service.input_server(token, fd.into(), flags)
        }
        REMOVE_INPUT => {
            let token: SpIBinder = data.read()?;
            service.remove_input(token)
        }
        INPUT_FRAME => {
            let token: SpIBinder = data.read()?;
            let frame = [data.read()?, data.read()?, data.read()?, data.read()?];
            service.input_frame(token, frame)
        }
        TASK => {
            let task: i32 = data.read()?;
            service.task(task)
        }
        POINTER_ICON => {
            let kind: i32 = data.read()?;
            service.pointer_icon(kind)
        }
        MINIMIZE => service.minimize(),
        WINDOW_POSITION => {
            let layer_id: i32 = data.read()?;
            let x: i32 = data.read()?;
            let y: i32 = data.read()?;
            service.window_position(layer_id, x, y)
        }
        INPUT_METHOD => {
            let method = service.input_method()?;
            reply.write(&method)
        }
        POST_DRAW_TRANSACTION => {
            let t = read_transaction(data)?;
            service.post_draw_transaction(&t)
        }
        DIM_LAYER => {
            let layer_id: i32 = data.read()?;
            let amount: f32 = data.read()?;
            let width: i32 = data.read()?;
            let height: i32 = data.read()?;
            service.dim_layer(layer_id, amount, width, height)
        }
        _ => {
            atl_diag::unimplemented(INTERFACE, &format!("code {code}"));
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

    fn input_server(&self, token: SpIBinder, fd: OwnedFd, flags: i32) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&token)?;
        data.write(&ParcelFileDescriptor::new(fd))?;
        data.write(&flags)?;
        binder.submit_transact(INPUT_SERVER, data, 0)?;
        Ok(())
    }

    fn remove_input(&self, token: SpIBinder) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&token)?;
        binder.submit_transact(REMOVE_INPUT, data, 0)?;
        Ok(())
    }

    fn input_frame(&self, token: SpIBinder, frame: [i32; 4]) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&token)?;
        for side in frame {
            data.write(&side)?;
        }
        binder.submit_transact(INPUT_FRAME, data, 0)?;
        Ok(())
    }

    fn task(&self, task: i32) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&task)?;
        binder.submit_transact(TASK, data, 0)?;
        Ok(())
    }

    fn pointer_icon(&self, kind: i32) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&kind)?;
        binder.submit_transact(POINTER_ICON, data, binder::binder_impl::FLAG_ONEWAY)?;
        Ok(())
    }

    fn minimize(&self) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let data = binder.prepare_transact()?;
        binder.submit_transact(MINIMIZE, data, binder::binder_impl::FLAG_ONEWAY)?;
        Ok(())
    }

    fn window_position(&self, layer_id: i32, x: i32, y: i32) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&layer_id)?;
        data.write(&x)?;
        data.write(&y)?;
        binder.submit_transact(WINDOW_POSITION, data, 0)?;
        Ok(())
    }

    fn dim_layer(
        &self,
        layer_id: i32,
        amount: f32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        data.write(&layer_id)?;
        data.write(&amount)?;
        data.write(&width)?;
        data.write(&height)?;
        binder.submit_transact(DIM_LAYER, data, 0)?;
        Ok(())
    }

    fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode> {
        let binder = self.as_binder();
        let data = binder.prepare_transact()?;
        let reply = binder.submit_transact(INPUT_METHOD, data, 0)?;
        reply.read()
    }

    fn post_draw_transaction(&self, t: &SurfaceControl_Transaction) -> Result<(), StatusCode> {
        let binder = self.as_binder();
        let mut data = binder.prepare_transact()?;
        write_transaction(&mut data.borrowed(), t)?;
        binder.submit_transact(POST_DRAW_TRANSACTION, data, 0)?;
        Ok(())
    }
}

/// Object kinds in POST_DRAW_TRANSACTION.
const OBJECT_BINDER: i32 = 0;
const OBJECT_FD: i32 = 1;

/// POST_DRAW_TRANSACTION's payload: the bytes, the object count, then per object its offset in the bytes, its kind and the binder (nullable) or fd (a duplicate: the Transaction keeps its own).
fn write_transaction(
    p: &mut BorrowedParcel<'_>,
    t: &SurfaceControl_Transaction,
) -> Result<(), StatusCode> {
    p.write(&t.data)?;
    p.write(&i32::try_from(t.objects.len()).map_err(|_| StatusCode::BAD_VALUE)?)?;
    for (offset, object) in &t.objects {
        p.write(&i32::try_from(*offset).map_err(|_| StatusCode::BAD_VALUE)?)?;
        match object {
            TransactionObject::Binder(b) => {
                p.write(&OBJECT_BINDER)?;
                p.write(b)?;
            }
            TransactionObject::Fd(fd) => {
                p.write(&OBJECT_FD)?;
                let dup = fd.try_clone().map_err(|_| StatusCode::BAD_VALUE)?;
                p.write(&ParcelFileDescriptor::new(dup))?;
            }
        }
    }
    Ok(())
}

/// The other end of [`write_transaction`].
fn read_transaction(p: &BorrowedParcel<'_>) -> Result<SurfaceControl_Transaction, StatusCode> {
    let data: Vec<u8> = p.read()?;
    let n = usize::try_from(p.read::<i32>()?).map_err(|_| StatusCode::BAD_VALUE)?;
    let mut objects = Vec::new();
    for _ in 0..n {
        let offset = usize::try_from(p.read::<i32>()?).map_err(|_| StatusCode::BAD_VALUE)?;
        let object = match p.read::<i32>()? {
            OBJECT_BINDER => TransactionObject::Binder(p.read()?),
            OBJECT_FD => {
                let fd: ParcelFileDescriptor = p.read()?;
                TransactionObject::Fd(Arc::new(fd.into()))
            }
            _ => return Err(StatusCode::BAD_VALUE),
        };
        objects.push((offset, object));
    }
    Ok(SurfaceControl_Transaction {
        present: true,
        data,
        objects,
    })
}

impl IUiAgent for Binder<BnUiAgent> {
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
        self.0.window_surface(layer_name)
    }

    fn input_server(&self, token: SpIBinder, fd: OwnedFd, flags: i32) -> Result<(), StatusCode> {
        self.0.input_server(token, fd, flags)
    }

    fn remove_input(&self, token: SpIBinder) -> Result<(), StatusCode> {
        self.0.remove_input(token)
    }

    fn input_frame(&self, token: SpIBinder, frame: [i32; 4]) -> Result<(), StatusCode> {
        self.0.input_frame(token, frame)
    }

    fn task(&self, task: i32) -> Result<(), StatusCode> {
        self.0.task(task)
    }

    fn pointer_icon(&self, kind: i32) -> Result<(), StatusCode> {
        self.0.pointer_icon(kind)
    }

    fn minimize(&self) -> Result<(), StatusCode> {
        self.0.minimize()
    }

    fn window_position(&self, layer_id: i32, x: i32, y: i32) -> Result<(), StatusCode> {
        self.0.window_position(layer_id, x, y)
    }

    fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode> {
        self.0.input_method()
    }

    fn post_draw_transaction(&self, t: &SurfaceControl_Transaction) -> Result<(), StatusCode> {
        self.0.post_draw_transaction(t)
    }

    fn dim_layer(
        &self,
        layer_id: i32,
        amount: f32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode> {
        self.0.dim_layer(layer_id, amount, width, height)
    }
}
