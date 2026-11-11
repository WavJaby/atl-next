// SPDX-License-Identifier: MIT
//! `android.gui.ISurfaceComposer` ("SurfaceFlingerAIDL"), `ISurfaceComposerClient` and `IDisplayEventConnection` on the converter's stubs: the calls BLASTBufferQueue, Choreographer's DisplayEventReceiver,
//! SurfaceControl and ViewRootImpl make through libgui. Answers follow SurfaceFlinger's (SurfaceFlinger.cpp `getStaticDisplayInfo` / `getDynamicDisplayInfoFromId` / `getDisplayState`) for the one display
//! the display model describes; every other method keeps the stub's default body, counted (atl_services_types::counters) and answered UNKNOWN_TRANSACTION.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::gui::CaptureArgs::CaptureArgs;
use atl_aidl::aidl::android::gui::CompositionPreference::CompositionPreference;
use atl_aidl::aidl::android::gui::CreateSurfaceResult::CreateSurfaceResult;
use atl_aidl::aidl::android::gui::DisplayCaptureArgs::DisplayCaptureArgs;
use atl_aidl::aidl::android::gui::DisplayConnectionType::DisplayConnectionType;
use atl_aidl::aidl::android::gui::DisplayMode::DisplayMode;
use atl_aidl::aidl::android::gui::DisplayStatInfo::DisplayStatInfo;
use atl_aidl::aidl::android::gui::DisplayState::DisplayState;
use atl_aidl::aidl::android::gui::DynamicDisplayInfo::DynamicDisplayInfo;
use atl_aidl::aidl::android::gui::FrameRateCategoryRate::FrameRateCategoryRate;
use atl_aidl::aidl::android::gui::HdrCapabilities::HdrCapabilities;
use atl_aidl::aidl::android::gui::IDisplayEventConnection::{
    BnDisplayEventConnection, IDisplayEventConnection,
};
use atl_aidl::aidl::android::gui::IScreenCaptureListener::IScreenCaptureListener;
use atl_aidl::aidl::android::gui::ISurfaceComposer::{BnSurfaceComposer, ISurfaceComposer};
use atl_aidl::aidl::android::gui::ISurfaceComposerClient::{
    BnSurfaceComposerClient, ISurfaceComposerClient, eFXSurfaceEffect, eFXSurfaceMask, eNoColorFill,
};
use atl_aidl::aidl::android::gui::Rotation::Rotation;
use atl_aidl::aidl::android::gui::Size::Size;
use atl_aidl::aidl::android::gui::StaticDisplayInfo::StaticDisplayInfo;
use atl_compositor::event::ConnectionId;
use atl_libgui_parcel::parcel::WireParcel;
use atl_services_types::{counters, display_model};
use binder::{BinderFeatures, SpIBinder, Status, StatusCode, Strong};
use gui_aidl_types_rs::{BitTube, LayerMetadata, VsyncEventData};

use crate::capture::Args;
use crate::capture_reply;
use crate::clock;
use crate::host::{Composer, DISPLAY_ID};

/// The interface names counters use.
const COMPOSER: &str = "ISurfaceComposer";
const CLIENT: &str = "ISurfaceComposerClient";
const EVENT_CONNECTION: &str = "IDisplayEventConnection";
/// `ui::ColorMode::NATIVE`: the one color mode the display reports.
const COLOR_MODE_NATIVE: i32 = 0;
/// DynamicDisplayInfo.preferredBootDisplayMode: none (SurfaceFlinger reports -1 without boot display mode support).
const NO_PREFERRED_BOOT_MODE: i32 = -1;
/// The display's one mode.
const MODE_ID: i32 = 0;
/// Density's reference dpi (DisplayMetrics.DENSITY_DEFAULT): `StaticDisplayInfo.density` is the scale factor against it.
const DENSITY_DEFAULT_DPI: f32 = 160.0;

fn unimplemented(interface: &'static str, method: &str) -> Status {
    counters::count_unimplemented(interface, method);
    StatusCode::UNKNOWN_TRANSACTION.into()
}

fn size(c: &Composer) -> Size {
    let d = c.display();
    Size {
        width: d.width,
        height: d.height,
    }
}

fn display_size(c: &Composer) -> (i32, i32) {
    let d = c.display();
    (d.width, d.height)
}

/// The arguments the composer checks (capture.rs); `size` is DisplayCaptureArgs' width x height, (0, 0) for captureDisplayById.
fn capture_args(args: &CaptureArgs, size: (i32, i32)) -> Args {
    Args {
        pixel_format: args.pixelFormat,
        frame_scale: (args.frameScaleX, args.frameScaleY),
        cropped: args
            .sourceCrop
            .as_ref()
            .is_some_and(|r| r.left < r.right && r.top < r.bottom),
        secure_layers: args.captureSecureLayers,
        dataspace: args.dataspace,
        grayscale: args.grayscale,
        excludes_layers: args.excludeHandles.as_ref().is_some_and(|h| !h.is_empty()),
        size,
    }
}

/// "SurfaceFlingerAIDL".
struct SurfaceComposer {
    composer: Arc<Composer>,
}

impl binder::Interface for SurfaceComposer {}

impl ISurfaceComposer for SurfaceComposer {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        unimplemented(COMPOSER, method)
    }

    fn r#createConnection(&self) -> binder::Result<Option<Strong<dyn ISurfaceComposerClient>>> {
        Ok(Some(new_client(Arc::clone(&self.composer))))
    }

    fn r#createDisplayEventConnection(
        &self,
        _vsync_source: atl_aidl::mangled::_7_android_3_gui_16_ISurfaceComposer_11_VsyncSource,
        _event_registration: atl_aidl::mangled::_7_android_3_gui_16_ISurfaceComposer_17_EventRegistration,
        _layer_handle: Option<&SpIBinder>,
    ) -> binder::Result<Option<Strong<dyn IDisplayEventConnection>>> {
        let (id, receive) = self
            .composer
            .create_display_event_connection()
            .map_err(|_| Status::from(StatusCode::NO_MEMORY))?;
        Ok(Some(BnDisplayEventConnection::new_binder(
            EventConnection {
                composer: Arc::clone(&self.composer),
                id,
                receive: Mutex::new(Some(receive)),
            },
            BinderFeatures::default(),
        )))
    }

    /// SurfaceFlinger::captureDisplay: the display's picture, answered on the listener (capture.rs). A token that is not this display is refused there.
    fn r#captureDisplay(
        &self,
        args: Option<&DisplayCaptureArgs>,
        listener: Option<&Strong<dyn IScreenCaptureListener>>,
    ) -> binder::Result<()> {
        let (Some(args), Some(listener)) = (args, listener) else {
            return Ok(());
        };
        let request = match &args.captureArgs {
            Some(c) if args.displayToken.as_ref() == Some(&self.composer.display_token()) => {
                Ok(capture_args(c, (args.width, args.height)))
            }
            Some(_) => Err("displayToken is not this display"),
            None => Err("no captureArgs"),
        };
        capture_reply::serve(listener, request, display_size(&self.composer));
        Ok(())
    }

    /// SurfaceFlinger::captureDisplayById: as captureDisplay, for the one display this process has.
    fn r#captureDisplayById(
        &self,
        display_id: i64,
        args: Option<&CaptureArgs>,
        listener: Option<&Strong<dyn IScreenCaptureListener>>,
    ) -> binder::Result<()> {
        let (Some(args), Some(listener)) = (args, listener) else {
            return Ok(());
        };
        let request = if display_id == DISPLAY_ID as i64 {
            Ok(capture_args(args, (0, 0)))
        } else {
            Err("no such display")
        };
        capture_reply::serve(listener, request, display_size(&self.composer));
        Ok(())
    }

    /// SurfaceFlinger::getMaxAcquiredBufferCount: the pipeline depth.
    fn r#getMaxAcquiredBufferCount(&self) -> binder::Result<i32> {
        Ok(self.composer.max_acquired_buffer_count())
    }

    /// SurfaceComposerAIDL::getCompositionPreference:9246 -> SurfaceFlinger::getCompositionPreference:2032: the default and wide-gamut composition dataspace and format, from the
    /// surface_flinger sysprops with defaults (SurfaceFlinger.cpp:458-467) V0_SRGB / RGBA_8888, and V0_SRGB for wide gamut when `has_wide_color_display` is false. The desktop composes
    /// in sRGB 8-bit (display_model COMPOSITION_*), the defaults' values; hwui's DeviceInfo::updateDisplayInfo aborts without an answer (ADisplay_acquirePhysicalDisplays).
    fn r#getCompositionPreference(&self) -> binder::Result<Option<CompositionPreference>> {
        Ok(Some(CompositionPreference {
            defaultDataspace: display_model::COMPOSITION_DATASPACE,
            defaultPixelFormat: display_model::COMPOSITION_PIXEL_FORMAT,
            wideColorGamutDataspace: display_model::COMPOSITION_DATASPACE,
            wideColorGamutPixelFormat: display_model::COMPOSITION_PIXEL_FORMAT,
        }))
    }

    fn r#getPhysicalDisplayIds(&self) -> binder::Result<Option<Vec<i64>>> {
        Ok(Some(vec![DISPLAY_ID as i64]))
    }

    fn r#getPhysicalDisplayToken(&self, display_id: i64) -> binder::Result<Option<SpIBinder>> {
        Ok((display_id == DISPLAY_ID as i64).then(|| self.composer.display_token()))
    }

    /// The layer stack 0, the display's rotation and shown size (SurfaceFlinger.cpp getDisplayState: `display->getOrientation()`, the layer stack space).
    fn r#getDisplayState(
        &self,
        _display: Option<&SpIBinder>,
    ) -> binder::Result<Option<DisplayState>> {
        Ok(Some(DisplayState {
            layerStack: 0,
            orientation: Rotation(self.composer.display().rotation),
            layerStackSpaceRect: Some(size(&self.composer)),
        }))
    }

    fn r#getDisplayStats(
        &self,
        _display: Option<&SpIBinder>,
    ) -> binder::Result<Option<DisplayStatInfo>> {
        Ok(Some(DisplayStatInfo {
            vsyncTime: 0,
            vsyncPeriod: clock::interval_ns(self.composer.display().refresh_hz),
        }))
    }

    /// An internal, insecure panel in its natural orientation; density against 160 dpi.
    fn r#getStaticDisplayInfo(
        &self,
        _display_id: i64,
    ) -> binder::Result<Option<StaticDisplayInfo>> {
        Ok(Some(StaticDisplayInfo {
            connectionType: DisplayConnectionType::Internal,
            port: 0,
            density: self.composer.display().dpi as f32 / DENSITY_DEFAULT_DPI,
            secure: false,
            deviceProductInfo: None,
            installOrientation: Rotation::Rotation0,
        }))
    }

    fn r#getDynamicDisplayInfoFromId(
        &self,
        _display_id: i64,
    ) -> binder::Result<Option<DynamicDisplayInfo>> {
        Ok(Some(dynamic_info(&self.composer)))
    }

    fn r#getDynamicDisplayInfoFromToken(
        &self,
        _display: Option<&SpIBinder>,
    ) -> binder::Result<Option<DynamicDisplayInfo>> {
        Ok(Some(dynamic_info(&self.composer)))
    }

    fn r#isWideColorDisplay(&self, _token: Option<&SpIBinder>) -> binder::Result<bool> {
        Ok(false)
    }
}

/// One display mode: the model's size and refresh rate, the model's density on both axes.
fn dynamic_info(c: &Composer) -> DynamicDisplayInfo {
    let d = c.display();
    let hz = d.refresh_hz;
    let (width, height) = d.natural_size();
    DynamicDisplayInfo {
        // the mode is the panel's (SurfaceFlinger.cpp getDynamicDisplayInfoInternal: the HWC mode's size), not the rotated view's
        supportedDisplayModes: Some(vec![Some(DisplayMode {
            id: MODE_ID,
            resolution: Some(Size { width, height }),
            xDpi: d.dpi as f32,
            yDpi: d.dpi as f32,
            supportedHdrTypes: Some(Vec::new()),
            peakRefreshRate: hz,
            vsyncRate: hz,
            appVsyncOffset: 0,
            sfVsyncOffset: 0,
            presentationDeadline: clock::interval_ns(hz),
            group: 0,
        })]),
        activeDisplayModeId: MODE_ID,
        renderFrameRate: hz,
        supportedColorModes: Some(vec![COLOR_MODE_NATIVE]),
        activeColorMode: COLOR_MODE_NATIVE,
        hdrCapabilities: Some(HdrCapabilities {
            supportedHdrTypes: Some(Vec::new()),
            ..Default::default()
        }),
        autoLowLatencyModeSupported: false,
        gameContentTypeSupported: false,
        preferredBootDisplayMode: NO_PREFERRED_BOOT_MODE,
        hasArrSupport: false,
        frameRateCategoryRate: Some(FrameRateCategoryRate {
            normal: hz,
            high: hz,
        }),
        supportedRefreshRates: Some(vec![hz]),
    }
}

/// The ISurfaceComposerClient createConnection hands out.
struct Client {
    composer: Arc<Composer>,
}

impl binder::Interface for Client {}

impl ISurfaceComposerClient for Client {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        unimplemented(CLIENT, method)
    }

    /// SurfaceFlinger::createLayer: a layer named `name` with a new handle. Of `flags`, the layer type decides whether it fills with colour (an effect layer without `eNoColorFill`; createLayer adds that flag to every other type);
    /// hidden, opaque ... arrive again as layer state in the transactions, and `metadata` has no reader yet.
    fn r#createSurface(
        &self,
        name: Option<&str>,
        flags: i32,
        parent: Option<&SpIBinder>,
        _metadata: Option<&LayerMetadata>,
    ) -> binder::Result<Option<CreateSurfaceResult>> {
        let name = name.unwrap_or_default();
        let mut parent = parent.cloned();
        let fills_color = flags & eFXSurfaceMask == eFXSurfaceEffect && flags & eNoColorFill == 0;
        let (handle, id) = self
            .composer
            .create_layer(name, parent.as_mut(), fills_color)
            .map_err(Status::from)?;
        Ok(Some(CreateSurfaceResult {
            handle: Some(handle),
            layerId: id,
            layerName: Some(name.to_owned()),
            transformHint: 0,
        }))
    }
}

/// One vsync connection: the client's ends of its channel (receive, send) wait here until the client steals them.
struct EventConnection {
    composer: Arc<Composer>,
    id: ConnectionId,
    receive: Mutex<Option<(OwnedFd, OwnedFd)>>,
}

impl binder::Interface for EventConnection {}

impl Drop for EventConnection {
    fn drop(&mut self) {
        self.composer.remove_connection(self.id);
    }
}

impl IDisplayEventConnection for EventConnection {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        unimplemented(EVENT_CONNECTION, method)
    }

    /// The channel's client end, once (EventThreadConnection::stealReceiveChannel moves it out).
    fn r#stealReceiveChannel(&self, out: &mut Option<BitTube>) -> binder::Result<()> {
        let fd = self
            .receive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| Status::from(StatusCode::INVALID_OPERATION))?;
        *out = Some(BitTube(Some(fd)));
        Ok(())
    }

    fn r#setVsyncRate(&self, count: i32) -> binder::Result<()> {
        self.composer.set_vsync_rate(self.id, count);
        Ok(())
    }

    fn r#requestNextVsync(&self) -> binder::Result<()> {
        self.composer.request_next_vsync(self.id);
        Ok(())
    }

    /// The timelines in wire form (`ParcelableVsyncEventData::writeToParcel`).
    fn r#getLatestVsyncEventData(&self) -> binder::Result<Option<VsyncEventData>> {
        let mut wire = WireParcel::new();
        self.composer
            .latest_vsync_event_data()
            .write_parcelable_body(&mut wire);
        Ok(Some(VsyncEventData(wire.data().to_vec())))
    }
}

/// A client connection of `composer`: what createConnection gives, and the client a window's SurfaceControl carries (ui_agent.rs).
pub fn new_client(composer: Arc<Composer>) -> Strong<dyn ISurfaceComposerClient> {
    BnSurfaceComposerClient::new_binder(Client { composer }, BinderFeatures::default())
}

/// The "SurfaceFlingerAIDL" service object of `composer`.
pub fn new_binder(composer: Arc<Composer>) -> SpIBinder {
    BnSurfaceComposer::new_binder(SurfaceComposer { composer }, BinderFeatures::default())
        .as_binder()
}
