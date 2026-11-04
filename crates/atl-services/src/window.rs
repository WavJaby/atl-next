// SPDX-License-Identifier: MIT
//! The `window` service (Context.WINDOW_SERVICE): the converter's IWindowManager stub, and the sessions it opens (WMS-lite, M3 slice 5). The three answers an activity reads before
//! any window exists: hasNavigationBar: false. WindowManagerService.hasNavigationBar (WindowManagerService.java:6669-6677) asks the display policy, which reads
//! config_showNavigationBar (frameworks/base/core/res/res/values/config.xml:2743, false); the desktop has no software navigation bar.
//! getCurrentAnimatorScale: 1.0. WindowManagerService.getCurrentAnimatorScale (:3687-3691) returns mAnimatorDurationScaleSetting, whose default is 1.0f (:1052), or 0 with animations disabled.
//! openSession: a session for the caller's callback (WindowManagerService.openSession:6004-6005); the session's window methods are below.
//! Session, the window path: addToDisplayAsUser (WindowManagerService.addWindow: ADD_OKAY, the client end of the window's input channel, the insets, the frame, scale 1),
//! relayout (WindowManagerService.relayoutWindow: the client frames, the merged configuration, the SurfaceControl the window's UI agent gives, the insets), remove (the window goes:
//! its input channel's server end closes with it). The frames are the whole display: one fullscreen window, no insets (the desktop shows no system bars).
//! Every other method keeps the stub's default body: counted (counters.rs) and answered UNKNOWN_TRANSACTION. finishDrawing stays one of them: its post-draw SurfaceControl.Transaction is
//! not modelled (the composer, M3 slice 4), so the codec refuses it before the method runs.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::view::IWindowManager::{
    BnWindowManager, EnforcePermission, IWindowManager,
};
use atl_aidl::aidl::android::view::IWindowSession::{BnWindowSession, IWindowSession};
use atl_parcelables::android::content::res::Configuration;
use atl_parcelables::android::graphics::Rect;
use atl_parcelables::android::util::MergedConfiguration;
use atl_parcelables::android::view::{
    InputChannel, InsetsSourceControl_Array, InsetsState, SurfaceControl,
    WindowManager_LayoutParams, WindowRelayoutResult,
};
use atl_parcelables::codecs::android::view::InputChannel::InputChannelCore;
use atl_parcelables::android::window::ClientWindowFrames;
use binder::{
    BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, StatusCode, Strong, ThreadState,
};

use crate::display_model::DisplayModel;
use crate::{activity, counters, permission};

/// The interface names counters use.
const WINDOW_MANAGER: &str = "IWindowManager";
const WINDOW_SESSION: &str = "IWindowSession";

/// WindowManagerGlobal.ADD_OKAY (WindowManagerGlobal.java:126).
const ADD_OKAY: i32 = 0;
/// WindowManagerGlobal.RELAYOUT_RES_FIRST_TIME (:86): the window's first relayout. RELAYOUT_RES_SURFACE_CHANGED (:91) is set only when a surface came with the relayout.
const RELAYOUT_RES_FIRST_TIME: i32 = 1;
const RELAYOUT_RES_SURFACE_CHANGED: i32 = 1 << 1;
/// WindowManager.LayoutParams.INPUT_FEATURE_NO_INPUT_CHANNEL (WindowManager.java:4505): such a window gets no input channel.
const INPUT_FEATURE_NO_INPUT_CHANNEL: i32 = 1 << 0;
/// InputTransport.cpp:95 SOCKET_BUFFER_SIZE: the send and receive buffers of both ends of a channel pair (InputChannel::openInputChannelPair, :370-375).
const SOCKET_BUFFER_SIZE: usize = 32 * 1024;
/// CompatibilityInfo's scale of an app that needs no size compatibility: the value addToDisplay reports (outSizeCompatScale).
const COMPAT_SCALE_NONE: f32 = 1.0;

/// IUiAgent (plan 8c), as WMS uses it: at relayout, the app process that owns a window's surface gives the window's SurfaceControl. Only the window path's use is shaped here:
/// the transport (a binder the app process registers) and the in-app side come with the composer (M3 slice 4). Until then [`NoUiAgent`] answers, counted.
pub trait UiAgent: Send + Sync {
    /// The SurfaceControl of the window with client binder `window`; None: the window has no surface (yet).
    fn window_surface(&self, window: &SpIBinder) -> Option<SurfaceControl>;
}

/// No agent is registered: no window has a surface.
struct NoUiAgent;

impl UiAgent for NoUiAgent {
    fn window_surface(&self, _window: &SpIBinder) -> Option<SurfaceControl> {
        counters::count(
            WINDOW_SESSION,
            "relayout: no window surface (no UI agent until the composer, M3 slice 4)",
        );
        None
    }
}

/// One window the session added: its client binder (what `remove` and `relayout` find it by) and the server end of its input channel, which AOSP's InputDispatcher
/// publishes on; it closes when the window goes.
struct Window {
    client: SpIBinder,
    input_server: Option<OwnedFd>,
    laid_out: bool,
}

struct WindowManager {
    model: DisplayModel,
}

/// The session openSession gives: the windows it added, and the display they lay out on.
struct Session {
    model: DisplayModel,
    agent: Box<dyn UiAgent>,
    windows: Mutex<Vec<Window>>,
}

impl Interface for WindowManager {}

impl Interface for Session {}

impl IWindowManager for WindowManager {
    /// Fail-closed as the other services' hooks (display.rs, activity.rs): the permissions the stub names are held by root / system only (permission::holds).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let perms: &[&str] = match &permission {
            EnforcePermission::Single(p) => std::slice::from_ref(p),
            EnforcePermission::AllOf(p) | EnforcePermission::AnyOf(p) => p,
        };
        if permission::holds(ThreadState::get_calling_uid(), perms) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(WINDOW_MANAGER, method)
    }

    /// WindowManagerService.hasNavigationBar:6669-6677, for the one display: false (see the module doc).
    fn r#hasNavigationBar(&self, _display_id: i32) -> binder::Result<bool> {
        Ok(false)
    }

    /// WindowManagerService.getCurrentAnimatorScale:3687-3691 with the default setting 1.0f (:1052) and animations enabled.
    fn r#getCurrentAnimatorScale(&self) -> binder::Result<f32> {
        Ok(1.0)
    }

    /// WindowManagerService.openSession:6004-6005: a session for the caller's callback; the callback is not kept (no window event reaches the app yet).
    fn r#openSession(
        &self,
        _callback: Option<
            &Strong<dyn atl_aidl::mangled::_7_android_4_view_22_IWindowSessionCallback>,
        >,
    ) -> binder::Result<Option<Strong<dyn IWindowSession>>> {
        Ok(Some(BnWindowSession::new_binder(
            Session {
                model: self.model,
                agent: Box::new(NoUiAgent),
                windows: Mutex::new(Vec::new()),
            },
            BinderFeatures::default(),
        )))
    }
}

impl Session {
    /// The display as a window's frame: the whole display, from the origin.
    fn display_rect(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.model.width,
            bottom: self.model.height,
        }
    }

    /// The insets a window of the display sees: none (no system bars, no cutout), the display frame.
    fn insets_state(&self) -> InsetsState {
        InsetsState {
            display_frame: self.display_rect(),
            rounded_corner_frame: self.display_rect(),
            seq: 0,
            sources: Vec::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Window>> {
        // a poisoned list still holds the windows
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// AOSP InputChannel.openInputChannelPair (InputTransport.cpp:355-378): a SOCK_SEQPACKET pair, both ends with the buffer sizes above. The first fd is the server end, the second
/// the client end (the app's). Close-on-exec, so no child of atld inherits either end.
fn open_input_pair() -> Result<(OwnedFd, OwnedFd), StatusCode> {
    let (server, client) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|_| StatusCode::UNKNOWN_ERROR)?;
    for fd in [&server, &client] {
        rustix::net::sockopt::set_socket_send_buffer_size(fd, SOCKET_BUFFER_SIZE)
            .map_err(|_| StatusCode::UNKNOWN_ERROR)?;
        rustix::net::sockopt::set_socket_recv_buffer_size(fd, SOCKET_BUFFER_SIZE)
            .map_err(|_| StatusCode::UNKNOWN_ERROR)?;
    }
    Ok((server, client))
}

impl IWindowSession for Session {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(WINDOW_SESSION, method)
    }

    /// WindowManagerService.addWindow for a window of the one display: the window is recorded by its client binder; a window that takes input gets an input channel (the
    /// client end returned, the server end kept with the window). Refused: a call without the window or its attributes (counted, BAD_VALUE).
    fn r#addToDisplayAsUser(
        &self,
        window: Option<&Strong<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>>,
        attrs: Option<&WindowManager_LayoutParams>,
        _view_visibility: i32,
        _layer_stack_id: i32,
        _user_id: i32,
        _requested_visible_types: i32,
        out_input_channel: &mut Option<InputChannel>,
        insets_state: &mut Option<InsetsState>,
        active_controls: &mut Option<InsetsSourceControl_Array>,
        attached_frame: &mut Option<Rect>,
        size_compat_scale: &mut Option<Vec<f32>>,
    ) -> binder::Result<i32> {
        let (Some(window), Some(attrs)) = (window, attrs) else {
            counters::refused(WINDOW_SESSION, "addToDisplayAsUser: no window or LayoutParams");
            return Err(StatusCode::BAD_VALUE.into());
        };
        let (channel, input_server) = if attrs.input_features & INPUT_FEATURE_NO_INPUT_CHANNEL == 0
        {
            let (server, client) = open_input_pair()?;
            let channel = InputChannel {
                core: Some(InputChannelCore {
                    name: attrs.title.clone(),
                    fd: Arc::new(client),
                    token: None,
                }),
            };
            (channel, Some(server))
        } else {
            (InputChannel::default(), None)
        };
        self.lock().push(Window {
            client: window.as_binder(),
            input_server,
            laid_out: false,
        });
        *out_input_channel = Some(channel);
        *insets_state = Some(self.insets_state());
        *active_controls = None;
        *attached_frame = Some(Rect::default());
        *size_compat_scale = Some(vec![COMPAT_SCALE_NONE]);
        Ok(ADD_OKAY)
    }

    /// WindowManagerService.relayoutWindow for a window added by this session: the client frames and the display's configuration, the surface the UI agent gives (none
    /// until the composer), the insets. The first relayout reports RELAYOUT_RES_FIRST_TIME. Refused: a window this session did not add (counted, BAD_VALUE).
    fn r#relayout(
        &self,
        window: Option<&Strong<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>>,
        _attrs: Option<&WindowManager_LayoutParams>,
        _requested_width: i32,
        _requested_height: i32,
        _view_visibility: i32,
        _flags: i32,
        seq: i32,
        _last_sync_seq_id: i32,
        out_relayout_result: &mut Option<WindowRelayoutResult>,
    ) -> binder::Result<i32> {
        let Some(window) = window else {
            counters::refused(WINDOW_SESSION, "relayout: no window");
            return Err(StatusCode::BAD_VALUE.into());
        };
        let client = window.as_binder();
        let (first, surface) = {
            let mut windows = self.lock();
            let Some(record) = windows.iter_mut().find(|w| w.client == client) else {
                counters::refused(WINDOW_SESSION, "relayout: window not added by this session");
                return Err(StatusCode::BAD_VALUE.into());
            };
            let first = !record.laid_out;
            record.laid_out = true;
            (first, self.agent.window_surface(&record.client))
        };
        let configuration: Configuration = activity::global_configuration(&self.model);
        let surface_changed = surface.is_some();
        *out_relayout_result = Some(WindowRelayoutResult {
            frames: ClientWindowFrames {
                frame: self.display_rect(),
                display_frame: self.display_rect(),
                parent_frame: self.display_rect(),
                attached_frame: None,
                is_parent_frame_clipped_by_display_cutout: false,
                compat_scale: COMPAT_SCALE_NONE,
                seq,
            },
            merged_configuration: MergedConfiguration {
                global: configuration.clone(),
                override_config: Configuration::default(),
                merged: configuration,
            },
            surface_control: surface.unwrap_or_default(),
            insets_state: self.insets_state(),
            // no controls: the null array (InsetsSourceControl.Array with mControls null, its sequence 0)
            active_controls: InsetsSourceControl_Array {
                controls_count: -1,
                seq: 0,
            },
            sync_seq_id: 0,
            activity_window_info: None,
        });
        let mut result = 0;
        if first {
            result |= RELAYOUT_RES_FIRST_TIME;
        }
        if surface_changed {
            result |= RELAYOUT_RES_SURFACE_CHANGED;
        }
        Ok(result)
    }

    /// WindowManagerService.removeWindow: the window leaves the session and its input channel's server end closes. A window the session does not hold is nothing to remove
    /// (AOSP does nothing for an unknown window either).
    fn r#remove(&self, client_token: Option<&SpIBinder>) -> binder::Result<()> {
        let Some(token) = client_token else {
            counters::refused(WINDOW_SESSION, "remove: no client token");
            return Err(StatusCode::BAD_VALUE.into());
        };
        self.lock().retain(|w| w.client != *token);
        Ok(())
    }
}

/// A new window service binder: the one display's model, which every session lays out on.
pub fn new_binder(model: DisplayModel) -> SpIBinder {
    BnWindowManager::new_binder(WindowManager { model }, BinderFeatures::default()).as_binder()
}
