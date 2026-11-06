// SPDX-License-Identifier: MIT
//! The app's UI agent (atl_services::ui_agent, M3 slice 5): a window's layer in this process's compositor, answered as the SurfaceControl the window service returns from relayout.
//! The layer is made as createSurface makes one (`Composer::create_layer`, a root layer named after the window): the client is the one createConnection gives, the handle is the layer's.
//! The SurfaceControl is libgui's parcel form (codecs/android/view/SurfaceControl.rs); the display's size, RGBA_8888, no transform hint.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, mpsc::Sender};

use atl_compositor::model::ColorLayer;
use atl_parcelables::android::view::{SurfaceControl, SurfaceControl_Transaction};
use atl_parcelables::codecs::android::view::SurfaceControl::NativeSurfaceControl;
use atl_services::ui_agent::{BnUiAgent, IUiAgent};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode};

use crate::host::Composer;
use crate::input::Message;
use crate::service;
use crate::task_sender::TaskSender;

/// PixelFormat.RGBA_8888 (`HAL_PIXEL_FORMAT_RGBA_8888` = 1), the format of every window layer.
const RGBA_8888: u32 = 1;

/// The agent of this process: its windows are layers of its compositor, and their input channels go to the input thread. `sender` takes the window's Task (TASK); None in headless
/// mode, which has no toplevel to size.
struct UiAgent {
    composer: Arc<Composer>,
    input: Sender<Message>,
    sender: Option<Arc<TaskSender>>,
    /// the in-app input method (atl-ime) IMMS-lite binds (INPUT_METHOD); None in headless mode
    input_method: Option<SpIBinder>,
    /// dim layers (DIM_LAYER) by the dimming window's layer id: the handle that keeps the layer alive, and its id
    dims: Mutex<HashMap<i32, (SpIBinder, i32)>>,
}

impl Interface for UiAgent {}

impl IUiAgent for UiAgent {
    fn task(&self, task: i32) -> Result<(), StatusCode> {
        if let Some(sender) = &self.sender {
            sender.set_task(task);
        }
        Ok(())
    }

    fn window_position(&self, layer_id: i32, x: i32, y: i32) -> Result<(), StatusCode> {
        self.composer
            .set_layer_position(layer_id, x as f32, y as f32)
    }

    fn post_draw_transaction(&self, t: &SurfaceControl_Transaction) -> Result<(), StatusCode> {
        self.composer.apply_post_draw(t.clone())
    }

    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
        let (handle, layer_id) = self.composer.create_layer(layer_name, None, false)?;
        let display = self.composer.display();
        Ok(SurfaceControl {
            name: Some(layer_name.to_owned()),
            width: display.width,
            height: display.height,
            native: Some(NativeSurfaceControl {
                client: Some(service::new_client(Arc::clone(&self.composer)).as_binder()),
                handle: Some(handle),
                layer_id,
                name: Some(layer_name.to_owned()),
                transform_hint: 0,
                width: display.width as u32,
                height: display.height as u32,
                format: RGBA_8888,
            }),
        })
    }

    fn input_server(&self, token: SpIBinder, fd: OwnedFd, flags: i32) -> Result<(), StatusCode> {
        self.input
            .send(Message::Window { token, fd, flags })
            .map_err(|_| StatusCode::DEAD_OBJECT)
    }

    fn remove_input(&self, token: SpIBinder) -> Result<(), StatusCode> {
        self.input
            .send(Message::WindowRemoved(token))
            .map_err(|_| StatusCode::DEAD_OBJECT)
    }

    fn input_frame(
        &self,
        token: SpIBinder,
        [left, top, right, bottom]: [i32; 4],
    ) -> Result<(), StatusCode> {
        if std::env::var(crate::start::BREAK_ENV).as_deref() == Ok(crate::start::BREAK_INPUT_FRAMES)
        {
            return Ok(());
        }
        let frame = crate::input::Frame {
            left,
            top,
            right,
            bottom,
        };
        self.input
            .send(Message::WindowFrame { token, frame })
            .map_err(|_| StatusCode::DEAD_OBJECT)
    }

    /// To the UI thread's window (ui_window); headless mode has none and ignores it.
    fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode> {
        Ok(self.input_method.clone())
    }

    fn pointer_icon(&self, kind: i32) -> Result<(), StatusCode> {
        crate::ui_window::set_pointer_icon(kind);
        Ok(())
    }

    /// WMS Dimmer: one colour layer per dimming window (Dimmer.java:169-173 makeDimLayer), a root at the Task origin like every window layer here, cropped to the Task
    /// (DimmerAnimationHelper.java:286-287), relative z -1 to the window (:257), black at alpha `amount` (:293 setAlpha). Dropping the handle destroys the layer.
    fn dim_layer(
        &self,
        layer_id: i32,
        amount: f32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode> {
        let mut dims = self
            .dims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if amount <= 0.0 {
            dims.remove(&layer_id);
            return Ok(());
        }
        let dim_id = match dims.get(&layer_id) {
            Some((_, id)) => *id,
            None => {
                let (handle, id) =
                    self.composer
                        .create_layer(&format!("Dim Layer for {layer_id}"), None, true)?;
                dims.insert(layer_id, (handle, id));
                id
            }
        };
        self.composer.set_color_layer(
            dim_id,
            ColorLayer {
                relative_to: layer_id,
                z: -1,
                color: [0.0; 3],
                alpha: amount,
                size: (width as f32, height as f32),
            },
        )
    }
}

/// A new UI agent binder of `composer`; `input` takes the windows' input channels, `sender` the window's Task, `input_method` is the app's IInputMethod.
pub fn new_binder(
    composer: Arc<Composer>,
    input: Sender<Message>,
    sender: Option<Arc<TaskSender>>,
    input_method: Option<SpIBinder>,
) -> SpIBinder {
    BnUiAgent::new_binder(
        UiAgent {
            composer,
            input,
            sender,
            input_method,
            dims: Mutex::default(),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
