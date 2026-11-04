// SPDX-License-Identifier: MIT
//! The app's UI agent (atl_services::ui_agent, M3 slice 5): a window's layer in this process's compositor, answered as the SurfaceControl the window service returns from relayout.
//! The layer is made as createSurface makes one (`Composer::create_layer`, a root layer named after the window): the client is the one createConnection gives, the handle is the layer's.
//! The SurfaceControl is libgui's parcel form (codecs/android/view/SurfaceControl.rs); the display's size, RGBA_8888, no transform hint.

use std::sync::Arc;

use atl_parcelables::android::view::SurfaceControl;
use atl_parcelables::codecs::android::view::SurfaceControl::NativeSurfaceControl;
use atl_services::ui_agent::{BnUiAgent, IUiAgent};
use binder::{BinderFeatures, Interface, SpIBinder, StatusCode};

use crate::host::Composer;
use crate::service;

/// PixelFormat.RGBA_8888 (`HAL_PIXEL_FORMAT_RGBA_8888` = 1), the format of every window layer.
const RGBA_8888: u32 = 1;

/// The agent of this process: its windows are layers of its compositor.
struct UiAgent {
    composer: Arc<Composer>,
}

impl Interface for UiAgent {}

impl IUiAgent for UiAgent {
    fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
        let (handle, layer_id) = self.composer.create_layer(layer_name, None)?;
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
}

/// A new UI agent binder of `composer`.
pub fn new_binder(composer: Arc<Composer>) -> SpIBinder {
    BnUiAgent::new_binder(UiAgent { composer }, BinderFeatures::default()).as_binder()
}
