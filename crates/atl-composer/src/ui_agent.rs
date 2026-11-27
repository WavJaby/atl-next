// SPDX-License-Identifier: MIT
//! The app's UI agent (atl_services_wire::ui_agent, M3 slice 5): a window's layer in this process's compositor, answered as the SurfaceControl the window service returns from relayout.
//! The layer is made as createSurface makes one (`Composer::create_layer`, a root layer named after the window): the client is the one createConnection gives, the handle is the layer's.
//! The SurfaceControl is libgui's parcel form (codecs/android/view/SurfaceControl.rs); the display's size, RGBA_8888, no transform hint.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use atl_compositor::model::ColorLayer;
use atl_parcelables::android::view::{SurfaceControl, SurfaceControl_Transaction};
use atl_parcelables::codecs::android::view::SurfaceControl::NativeSurfaceControl;
use atl_services_types::display_model::DisplayModel;
use atl_services_wire::ui_agent::{BnUiAgent, IUiAgent};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode};

use crate::host::{Composer, VmHost};
use crate::input::{InputSender, Message};
use crate::service;
use crate::task_reports::TaskReports;

/// `model` becomes the composer's display; a change is logged (`atl-composer: ui: display <d>`), whichever route brought it (the services' push or a report's reply, task_reports.rs).
pub(crate) fn adopt(composer: &Composer, model: DisplayModel) {
    if composer.set_display(model) {
        // `model`, not `display`: inside tracing's macros `display` names tracing::field::display
        tracing::info!("atl-composer: ui: display {}", model.to_arg());
    }
}

/// PixelFormat.RGBA_8888 (`HAL_PIXEL_FORMAT_RGBA_8888` = 1), the format of every window layer.
const RGBA_8888: u32 = 1;

/// The agent of this process: its windows are layers of its compositor, and their input channels go to the input thread. `reports` takes the window's Task (TASK); None in headless
/// mode, which has no toplevel to size.
struct UiAgent {
    composer: Arc<Composer>,
    input: InputSender<Message>,
    reports: Option<Arc<TaskReports>>,
    /// the in-app input method (atl-ime) IMMS-lite binds (INPUT_METHOD); None in headless mode
    input_method: Option<SpIBinder>,
    /// dim layers (DIM_LAYER) by the dimming window's layer id: the handle that keeps the layer alive, and its id
    dims: Mutex<HashMap<i32, (SpIBinder, i32)>>,
    /// the launcher's VM: the task title's label lookup (TASK_TITLE)
    host: Arc<dyn VmHost>,
}

impl Interface for UiAgent {}

impl IUiAgent for UiAgent {
    fn task(&self, task: i32) -> Result<(), StatusCode> {
        if let Some(reports) = &self.reports {
            reports.set_task(task);
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
        // shown at creation: the window service shows and hides a window through its own state, not this layer's flags
        let (handle, layer_id) = self.composer.create_layer(layer_name, None, false, 0)?;
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
        // break `input-frames` (Cargo.toml): every window's frame is an endless one at the Task's origin, so pointer events stay in Task coordinates (the frame still comes: it is what lets the window take focus)
        let frame = if cfg!(feature = "break-input-frames") {
            crate::input::Frame {
                left: 0,
                top: 0,
                right: i32::MAX,
                bottom: i32::MAX,
            }
        } else {
            crate::input::Frame {
                left,
                top,
                right,
                bottom,
            }
        };
        self.input
            .send(Message::WindowFrame { token, frame })
            .map_err(|_| StatusCode::DEAD_OBJECT)
    }

    /// To the display thread's window (ui_window); headless mode has none and ignores it.
    fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode> {
        Ok(self.input_method.clone())
    }

    fn pointer_icon(&self, kind: i32) -> Result<(), StatusCode> {
        crate::ui_window::set_pointer_icon(kind);
        Ok(())
    }

    fn minimize(&self) -> Result<(), StatusCode> {
        crate::ui_window::minimize();
        Ok(())
    }

    /// A non-positive size is the caller's bug: refused, nothing asked of the window.
    fn resize(&self, width: i32, height: i32) -> Result<(), StatusCode> {
        match (u32::try_from(width), u32::try_from(height)) {
            (Ok(width @ 1..), Ok(height @ 1..)) => {
                crate::ui_window::resize(width, height);
                Ok(())
            }
            _ => Err(StatusCode::BAD_VALUE),
        }
    }

    /// The services' display, pushed before any app hears of the change: SurfaceFlinger's mode, the vsync interval and tilt's rotation follow it from now on. An unparsable one is refused.
    fn set_display(&self, display: &str) -> Result<(), StatusCode> {
        let display = DisplayModel::parse_arg(display).map_err(|_| StatusCode::BAD_VALUE)?;
        // break `skip-display-push`: the push is dropped, so a process keeps its display until its own report's reply
        if cfg!(feature = "break-skip-display-push") {
            return Ok(());
        }
        // break `late-display-push`: the push is adopted after the apps heard of the change (AOSP's order reversed)
        if cfg!(feature = "break-late-display-push") {
            let composer = Arc::clone(&self.composer);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                adopt(&composer, display);
            });
            return Ok(());
        }
        adopt(&self.composer, display);
        Ok(())
    }

    /// The task's title as recents shows it: the TaskDescription label when it has text, else the root activity's label loaded by the app's PackageManager (Launcher3 TaskIconCache's fallback,
    /// ActivityInfo.loadLabel). The one place the window's title is decided after it opens. A failed lookup keeps the current title.
    fn task_title(
        &self,
        label: Option<&str>,
        package: &str,
        class: &str,
    ) -> Result<(), StatusCode> {
        let title = match label.map(str::trim).filter(|l| !l.is_empty()) {
            Some(label) => Some(label.to_owned()),
            None => self.host.activity_label(package, class),
        };
        match title {
            Some(title) => crate::ui_window::set_title(title),
            None => tracing::warn!("atl-composer: ui: no label for {package}/{class}"),
        }
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
                let (handle, id) = self.composer.create_layer(
                    &format!("Dim Layer for {layer_id}"),
                    None,
                    true,
                    0,
                )?;
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

/// A new UI agent binder of `composer`; `input` takes the windows' input channels, `reports` the window's Task, `input_method` is the app's IInputMethod, `host` answers the title's label lookup.
pub fn new_binder(
    composer: Arc<Composer>,
    input: InputSender<Message>,
    reports: Option<Arc<TaskReports>>,
    input_method: Option<SpIBinder>,
    host: Arc<dyn VmHost>,
) -> SpIBinder {
    BnUiAgent::new_binder(
        UiAgent {
            composer,
            input,
            reports,
            input_method,
            dims: Mutex::default(),
            host,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
