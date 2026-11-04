// SPDX-License-Identifier: MIT
//! `android.gui.ISurfaceComposer` ("SurfaceFlingerAIDL"), `ISurfaceComposerClient` and `IDisplayEventConnection` on the converter's stubs: the calls BLASTBufferQueue, Choreographer's DisplayEventReceiver,
//! SurfaceControl and ViewRootImpl make through libgui. Answers follow SurfaceFlinger's (SurfaceFlinger.cpp `getStaticDisplayInfo` / `getDynamicDisplayInfoFromId` / `getDisplayState`) for the one display
//! the display model describes; every other method keeps the stub's default body, counted (atl_services::counters) and answered UNKNOWN_TRANSACTION.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::gui::CreateSurfaceResult::CreateSurfaceResult;
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
use atl_aidl::aidl::android::gui::ISurfaceComposer::{BnSurfaceComposer, ISurfaceComposer};
use atl_aidl::aidl::android::gui::ISurfaceComposerClient::{
    BnSurfaceComposerClient, ISurfaceComposerClient,
};
use atl_aidl::aidl::android::gui::Rotation::Rotation;
use atl_aidl::aidl::android::gui::Size::Size;
use atl_aidl::aidl::android::gui::StaticDisplayInfo::StaticDisplayInfo;
use atl_compositor::event::ConnectionId;
use atl_compositor::parcel::WireParcel;
use atl_services::counters;
use binder::{BinderFeatures, SpIBinder, Status, StatusCode, Strong};
use gui_aidl_types_rs::{BitTube, LayerMetadata, VsyncEventData};

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
    counters::count(interface, method);
    StatusCode::UNKNOWN_TRANSACTION.into()
}

fn size(c: &Composer) -> Size {
    let d = c.display();
    Size {
        width: d.width,
        height: d.height,
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
        Ok(Some(BnSurfaceComposerClient::new_binder(
            Client {
                composer: Arc::clone(&self.composer),
            },
            BinderFeatures::default(),
        )))
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

    /// SurfaceFlinger::getMaxAcquiredBufferCount: the pipeline depth.
    fn r#getMaxAcquiredBufferCount(&self) -> binder::Result<i32> {
        Ok(self.composer.max_acquired_buffer_count())
    }

    fn r#getPhysicalDisplayIds(&self) -> binder::Result<Option<Vec<i64>>> {
        Ok(Some(vec![DISPLAY_ID as i64]))
    }

    fn r#getPhysicalDisplayToken(&self, display_id: i64) -> binder::Result<Option<SpIBinder>> {
        Ok((display_id == DISPLAY_ID as i64).then(|| self.composer.display_token()))
    }

    /// The layer stack 0, unrotated, the display's size.
    fn r#getDisplayState(
        &self,
        _display: Option<&SpIBinder>,
    ) -> binder::Result<Option<DisplayState>> {
        Ok(Some(DisplayState {
            layerStack: 0,
            orientation: Rotation::Rotation0,
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
    DynamicDisplayInfo {
        supportedDisplayModes: Some(vec![Some(DisplayMode {
            id: MODE_ID,
            resolution: Some(size(c)),
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

    /// SurfaceFlinger::createLayer: a layer named `name` with a new handle; `flags` (layer type, hidden, opaque ...) arrive again as layer state in the transactions, and `metadata` has no reader yet.
    fn r#createSurface(
        &self,
        name: Option<&str>,
        _flags: i32,
        parent: Option<&SpIBinder>,
        _metadata: Option<&LayerMetadata>,
    ) -> binder::Result<Option<CreateSurfaceResult>> {
        let name = name.unwrap_or_default();
        let mut parent = parent.cloned();
        let (handle, id) = self
            .composer
            .create_layer(name, parent.as_mut())
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

/// The "SurfaceFlingerAIDL" service object of `composer`.
pub fn new_binder(composer: Arc<Composer>) -> SpIBinder {
    BnSurfaceComposer::new_binder(SurfaceComposer { composer }, BinderFeatures::default())
        .as_binder()
}
