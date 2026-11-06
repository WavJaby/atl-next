// SPDX-License-Identifier: MIT
//! The `window` service (Context.WINDOW_SERVICE): the converter's IWindowManager stub, and the sessions it opens (WMS-lite, M3 slice 5). The three answers an activity reads before
//! any window exists: hasNavigationBar: false. WindowManagerService.hasNavigationBar (WindowManagerService.java:6669-6677) asks the display policy, which reads
//! config_showNavigationBar (frameworks/base/core/res/res/values/config.xml:2743, false); the desktop has no software navigation bar.
//! getCurrentAnimatorScale: 1.0. WindowManagerService.getCurrentAnimatorScale (:3687-3691) returns mAnimatorDurationScaleSetting, whose default is 1.0f (:1052), or 0 with animations disabled.
//! openSession: a session for the caller's callback (WindowManagerService.openSession:6004-6005); the session's window methods are below.
//! Session, the window path: addToDisplayAsUser (WindowManagerService.addWindow: ADD_OKAY, the client end of the window's input channel (its server end goes to the app's UI agent), the insets, the frame, scale 1),
//! relayout (WindowManagerService.relayoutWindow: the client frames, the merged configuration, the SurfaceControl the window's UI agent gives, the insets), remove (the window goes:
//! its input channel's server end stays with the app: no close signal yet, a known gap). The frames come from one place, [`client_frames`] (relayout and `resized` alike): display and parent frame are the Task's bounds
//! (freeform: WindowLayout.computeFrames sets the display frame from the window bounds), no insets (the desktop shows no system bars).
//! finishDrawing: the post-draw SurfaceControl.Transaction goes unchanged to the app's composer (Session::finishDrawing; what is not modelled is listed there).
//! Every other method keeps the stub's default body: counted (counters.rs) and answered UNKNOWN_TRANSACTION.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::view::IWindowManager::{
    BnWindowManager, EnforcePermission, IWindowManager,
};
use atl_aidl::aidl::android::view::IWindowSession::{BnWindowSession, IWindowSession};
use atl_libgui_parcel::transaction::Transaction;
use atl_parcelables::android::content::res::Configuration;
use atl_parcelables::android::graphics::Rect;
use atl_parcelables::android::util::MergedConfiguration;
use atl_parcelables::android::view::{
    InputChannel, InsetsSourceControl_Array, InsetsState, SurfaceControl,
    SurfaceControl_Transaction, WindowManager_LayoutParams, WindowRelayoutResult,
};
use atl_parcelables::android::window::ClientWindowFrames;
use atl_parcelables::codecs::android::view::InputChannel::InputChannelCore;
use atl_parcelables::codecs::android::view::SurfaceControl_Transaction as SurfaceControl_Transaction_codec;
use binder::{
    BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, StatusCode, Strong, ThreadState,
};

use crate::permission::Permissions;
use crate::tasks::{Placement, TaskId, Tasks};
use crate::ui_agent::{IUiAgent, UiAgents};
use crate::{activity, counters};

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
/// WindowManager.LayoutParams sub-window type range (WindowManager.java FIRST_SUB_WINDOW / LAST_SUB_WINDOW): a sub-window's token is its parent window's IWindow.
const FIRST_SUB_WINDOW: i32 = 1000;
const LAST_SUB_WINDOW: i32 = 1999;
/// InputTransport.cpp:95 SOCKET_BUFFER_SIZE: the send and receive buffers of both ends of a channel pair (InputChannel::openInputChannelPair, :370-375).
const SOCKET_BUFFER_SIZE: usize = 32 * 1024;
/// CompatibilityInfo's scale of an app that needs no size compatibility: the value addToDisplay reports (outSizeCompatScale).
const COMPAT_SCALE_NONE: f32 = 1.0;
/// WindowManagerGlobal.ADD_BAD_APP_TOKEN (WindowManagerGlobal.java:127): the window's token is no activity's, so the add fails and ViewRootImpl throws BadTokenException.
const ADD_BAD_APP_TOKEN: i32 = -1;
/// WindowManagerGlobal.ADD_BAD_SUBWINDOW_TOKEN: a sub-window whose token is no window's.
const ADD_BAD_SUBWINDOW_TOKEN: i32 = -2;

/// The UI agent of the app that opened a session (ui_agent.rs): the registry holds one per uid, so a session asks the agent of its own uid.
struct RegisteredAgent {
    agents: Arc<UiAgents>,
    uid: u32,
}

impl RegisteredAgent {
    /// The app's UI agent; None, counted under `call`, when the app registered no agent or it is not an IUiAgent.
    fn agent(&self, call: &str) -> Option<Strong<dyn IUiAgent>> {
        let Some(binder) = self.agents.get(self.uid) else {
            counters::answered(
                WINDOW_SESSION,
                &format!("{call}: no UI agent registered for the app's uid"),
            );
            return None;
        };
        match binder.into_interface() {
            Ok(agent) => Some(agent),
            Err(e) => {
                counters::answered(
                    WINDOW_SESSION,
                    &format!("{call}: registered UI agent is not an IUiAgent: {e:?}"),
                );
                None
            }
        }
    }

    /// The agent's layer for a window named `title`; None, counted, when the app registered no agent or the call failed.
    fn window_surface(&self, title: &str) -> Option<SurfaceControl> {
        let agent = self.agent("relayout")?;
        match agent.window_surface(title) {
            Ok(surface) => Some(surface),
            Err(e) => {
                counters::answered(
                    WINDOW_SESSION,
                    &format!("relayout: UI agent refused the window surface: {e:?}"),
                );
                None
            }
        }
    }

    /// Places the window's layer `layer_id` at `origin` in its Task; false, counted, when no agent takes it.
    fn place(&self, layer_id: i32, (x, y): (i32, i32)) -> bool {
        let Some(agent) = self.agent("relayout") else {
            return false;
        };
        match agent.window_position(layer_id, x, y) {
            Ok(()) => true,
            Err(e) => {
                counters::answered(
                    WINDOW_SESSION,
                    &format!("relayout: UI agent refused the window position: {e:?}"),
                );
                false
            }
        }
    }

    /// The dim behind window layer `layer_id` (WMS Dimmer): `amount` over `bounds` from the Task origin, 0 drops it. AOSP: WindowState.applyDims (WindowState.java:4980-4992) asks
    /// the Task's Dimmer to dim below the window at mAttrs.dimAmount; Dimmer.java:169-173 makes the dim a colour layer child of its host (the Task), cropped and placed to the
    /// host's bounds (DimmerAnimationHelper.java:286-287), relative -1 to the window (:257), alpha = amount (:293). Counted when refused.
    fn dim(&self, layer_id: i32, amount: f32, bounds: &Rect) {
        let Some(agent) = self.agent("relayout") else {
            return;
        };
        if let Err(e) = agent.dim_layer(
            layer_id,
            amount,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
        ) {
            counters::answered(
                WINDOW_SESSION,
                &format!("relayout: UI agent refused the dim layer: {e:?}"),
            );
        }
    }

    /// Names the window's Task to the agent: the composer sends that Task's bounds with it (SET_BOUNDS). Counted when the agent refuses it.
    fn name_task(&self, task: TaskId) {
        let Some(agent) = self.agent("addToDisplayAsUser") else {
            return;
        };
        if let Err(e) = agent.task(task.raw()) {
            counters::answered(
                WINDOW_SESSION,
                &format!("addToDisplayAsUser: UI agent refused the task: {e:?}"),
            );
        }
    }

    /// Hands a window's input channel server end to the agent, which publishes the window's input on it; false, counted, when no agent takes it (the fd then closes and the client reads EOF).
    fn give_input_server(&self, token: SpIBinder, fd: OwnedFd, flags: i32) -> bool {
        let Some(agent) = self.agent("addToDisplayAsUser") else {
            return false;
        };
        match agent.input_server(token, fd, flags) {
            Ok(()) => true,
            Err(e) => {
                counters::answered(
                    WINDOW_SESSION,
                    &format!("addToDisplayAsUser: UI agent refused the input server end: {e:?}"),
                );
                false
            }
        }
    }

    /// Tells the agent the window of input channel `token` is removed (WindowState.disposeInputChannel); counted when no agent takes it.
    fn remove_input(&self, token: SpIBinder) {
        let Some(agent) = self.agent("remove") else {
            return;
        };
        if let Err(e) = agent.remove_input(token) {
            counters::answered(
                WINDOW_SESSION,
                &format!("remove: UI agent refused the input removal: {e:?}"),
            );
        }
    }
}

/// One window the session added: its client binder (what `remove` and `relayout` find it by), its title (the layer name) and its Task (the activity its `attrs.token` names). Its input channel's server end went to the app's UI agent.
struct Window {
    client: SpIBinder,
    title: String,
    task: TaskId,
    laid_out: bool,
    /// The window's SurfaceControl once its agent gave one: kept, so the layer's handle lives as long as the window.
    surface: Option<SurfaceControl>,
    /// Where the agent placed the window's layer in its Task (a new layer sits at the origin): its surface origin ([`surface_origin`]) at the last relayout the agent took.
    position: (i32, i32),
    /// The token of the input channel the agent took; None for a window without one.
    input: Option<SpIBinder>,
}

struct WindowManager {
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
    permissions: Arc<Permissions>,
}

/// The session openSession gives: the windows it added, and the display they lay out on.
struct Session {
    agent: RegisteredAgent,
    tasks: Arc<Tasks<SpIBinder>>,
    windows: Mutex<Vec<Window>>,
}

impl Interface for WindowManager {}

impl Interface for Session {}

impl IWindowManager for WindowManager {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
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

    /// WindowManagerService.setInTouchMode:4045: records the mode and has the dispatcher tell the display's other windows (TouchModeEvent). The caller changes its own mode locally after the call
    /// (ViewRootImpl.ensureTouchMode:7233-7242), and an error there is a RuntimeException in the app. No other app window shares this display process's input: accept.
    fn r#setInTouchMode(&self, _in_touch: bool, _display_id: i32) -> binder::Result<()> {
        // counted: the window gate's input stage requires this line (xtask window.rs touch_mode_check)
        counters::answered(WINDOW_MANAGER, "setInTouchMode (answered)");
        Ok(())
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
                agent: RegisteredAgent {
                    agents: Arc::clone(&self.agents),
                    uid: ThreadState::get_calling_uid(),
                },
                tasks: Arc::clone(&self.tasks),
                windows: Mutex::new(Vec::new()),
            },
            BinderFeatures::default(),
        )))
    }
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Window>> {
        // a poisoned list still holds the windows
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
    Rect {
        left,
        top,
        right,
        bottom,
    }
}

/// The display's bounds: the insets' display frame (InsetsState is the display's, DisplayContent).
fn display_rect(tasks: &Tasks<SpIBinder>) -> Rect {
    let display = tasks.display();
    rect(0, 0, display.width, display.height)
}

/// A Task's bounds from the origin (tasks.rs): freeform, so the windows' bounds, not the display's.
fn task_rect(tasks: &Tasks<SpIBinder>, task: TaskId) -> Rect {
    let size = tasks.size(task);
    rect(0, 0, size.width, size.height)
}

/// The insets a window of the display sees: none (no system bars, no cutout), the display frame.
fn display_insets(tasks: &Tasks<SpIBinder>) -> InsetsState {
    InsetsState {
        display_frame: display_rect(tasks),
        rounded_corner_frame: display_rect(tasks),
        seq: 0,
        sources: Vec::new(),
    }
}

impl Placement {
    /// The add's placement: the LayoutParams fields the frame and the surface position depend on, nothing requested yet.
    fn of(a: &WindowManager_LayoutParams) -> Self {
        Placement {
            r#type: a.r#type,
            gravity: a.gravity,
            x: a.x,
            y: a.y,
            size: (a.width, a.height),
            requested: (0, 0),
            surface_inset: (a.surface_insets.left, a.surface_insets.top),
            flags: a.flags,
            dim_amount: a.dim_amount,
        }
    }
}

/// The frames of a window in a Task whose bounds are `task` (WindowLayout.computeFrames, WindowLayout.java:66-275, for a freeform window with no insets and no cutout):
/// display frame = window bounds (:86-87, the Task's bounds) = parent frame (:90); the frame is window_layout::frame. The same function answers relayout and `resized`,
/// so a window's frames never depend on which of the two reported last.
fn client_frames(p: Placement, task: Rect, seq: i32) -> ClientWindowFrames {
    let (l, t, r, b) =
        crate::window_layout::frame(&p, (task.left, task.top, task.right, task.bottom));
    let frame = rect(l, t, r, b);
    ClientWindowFrames {
        frame,
        display_frame: task.clone(),
        parent_frame: task,
        attached_frame: None,
        is_parent_frame_clipped_by_display_cutout: false,
        compat_scale: COMPAT_SCALE_NONE,
        seq,
    }
}

/// A rect as `l,t wxh` for the diagnostic lines.
fn show_rect(r: &Rect) -> String {
    format!(
        "{},{} {}x{}",
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top
    )
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

    /// WindowManagerService.addWindow for a window of the one display: the window is recorded by its client binder and its Task (the activity its `attrs.token` names); a window that takes
    /// input gets an input channel (the client end returned, the server end handed to the app's UI agent). Refused: a call without the window or its attributes (counted, BAD_VALUE); a token
    /// no activity was bound to (counted, ADD_BAD_APP_TOKEN).
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
            counters::refused(
                WINDOW_SESSION,
                "addToDisplayAsUser: no window or LayoutParams",
            );
            return Err(StatusCode::BAD_VALUE.into());
        };
        // a sub-window (a PopupWindow's panel) names its parent window by that window's IWindow and lives in the parent's Task (WindowManagerService.addWindow: windowForClientLocked,
        // else ADD_BAD_SUBWINDOW_TOKEN)
        let sub_window = (FIRST_SUB_WINDOW..=LAST_SUB_WINDOW).contains(&attrs.r#type);
        let task = if sub_window {
            attrs.token.as_ref().and_then(|parent| {
                self.lock()
                    .iter()
                    .find(|w| w.client == *parent)
                    .map(|w| w.task)
            })
        } else {
            self.tasks.task_of_token(attrs.token.as_ref())
        };
        let Some(task) = task else {
            if sub_window {
                counters::refused(
                    WINDOW_SESSION,
                    "addToDisplayAsUser: the sub-window's token names no window of the session",
                );
                return Ok(ADD_BAD_SUBWINDOW_TOKEN);
            }
            counters::refused(
                WINDOW_SESSION,
                "addToDisplayAsUser: the window's token names no activity's Task",
            );
            return Ok(ADD_BAD_APP_TOKEN);
        };
        self.agent.name_task(task);
        // every window but an INPUT_FEATURE_NO_INPUT_CHANNEL one gets a channel (WindowManagerService.addWindow -> openInputChannel), a NOT_TOUCHABLE toast too: ViewRootImpl.setView
        // builds its input receiver on whatever channel it passed in. The flags go to the agent, whose dispatcher skips NOT_TOUCHABLE / NOT_FOCUSABLE windows as InputDispatcher does.
        let (channel, input) = if attrs.input_features & INPUT_FEATURE_NO_INPUT_CHANNEL == 0 {
            let (server, client) = open_input_pair()?;
            // InputChannel.openInputChannelPair gives both ends a token; InputChannelCore reads it non-null (libinput readStrongBinder)
            let token = crate::token::new();
            // the server end is the app's: its UI agent publishes the window's input on it (no system InputDispatcher), keyed by the token as AOSP's dispatcher keys a connection
            let given = self
                .agent
                .give_input_server(token.clone(), server, attrs.flags);
            let channel = InputChannel {
                core: Some(InputChannelCore {
                    name: attrs.title.clone(),
                    fd: Arc::new(client),
                    token: Some(token.clone()),
                }),
            };
            (channel, given.then_some(token))
        } else {
            (InputChannel::default(), None)
        };
        self.tasks
            .add_window(task, window.as_binder(), Placement::of(attrs));
        self.lock().push(Window {
            client: window.as_binder(),
            title: attrs.title.clone().unwrap_or_default(),
            task,
            laid_out: false,
            surface: None,
            position: (0, 0),
            input,
        });
        *out_input_channel = Some(channel);
        *insets_state = Some(display_insets(&self.tasks));
        *active_controls = None;
        *attached_frame = Some(Rect::default());
        *size_compat_scale = Some(vec![COMPAT_SCALE_NONE]);
        // one line per add: what the app asked for and the Task it lands in, the evidence a misplaced window is read from
        activity::log(format_args!(
            "window add {} type {} attrs {}x{} gravity {:#x} at {},{} flags {:#x} dim {} surface_insets {:?} task {}",
            attrs.title.as_deref().unwrap_or(""),
            attrs.r#type,
            attrs.width,
            attrs.height,
            attrs.gravity,
            attrs.x,
            attrs.y,
            attrs.flags,
            attrs.dim_amount,
            attrs.surface_insets,
            show_rect(&task_rect(&self.tasks, task)),
        ));
        // WMS dispatches the app's visibility to its windows (WindowState's app visibility): ViewRootImpl draws only while its app is visible, and it starts out not.
        if let Err(e) = window.r#dispatchAppVisibility(true) {
            counters::answered(
                WINDOW_SESSION,
                &format!("addToDisplayAsUser: dispatchAppVisibility refused: {e:?}"),
            );
        }
        Ok(ADD_OKAY)
    }

    /// WindowManagerService.relayoutWindow for a window added by this session: the client frames and the display's configuration, the surface the UI agent gives (none
    /// until the composer), the insets. The first relayout reports RELAYOUT_RES_FIRST_TIME. Refused: a window this session did not add (counted, BAD_VALUE).
    fn r#relayout(
        &self,
        window: Option<&Strong<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>>,
        attrs: Option<&WindowManager_LayoutParams>,
        requested_width: i32,
        requested_height: i32,
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
        let (first, pending, task, title) = {
            let mut windows = self.lock();
            let Some(record) = windows.iter_mut().find(|w| w.client == client) else {
                counters::refused(WINDOW_SESSION, "relayout: window not added by this session");
                return Err(StatusCode::BAD_VALUE.into());
            };
            let first = !record.laid_out;
            record.laid_out = true;
            (
                first,
                record.surface.is_none().then(|| record.title.clone()),
                record.task,
                record.title.clone(),
            )
        };
        // relayout carries the LayoutParams only when they changed (ViewRootImpl sends null otherwise); WMS keeps the window's own (WindowState.mAttrs) and the requested size
        // (mRequestedWidth/Height), which `resized` lays out with again
        let previous = self.tasks.placement(&client).unwrap_or_default();
        let placement = Placement {
            requested: (requested_width, requested_height),
            ..attrs.map_or(previous, Placement::of)
        };
        self.tasks.set_placement(&client, placement);
        // The agent is another process, asked without the windows lock. A window gets its surface once; a failed call is asked again at the next relayout.
        let created = pending.and_then(|title| self.agent.window_surface(&title));
        if let Some(surface) = &created
            && let Some(record) = self.lock().iter_mut().find(|w| w.client == client)
        {
            record.surface = Some(surface.clone());
        }
        // every relayout returns the window's surface once it has one (the client keeps its copy only while the reply carries it); RELAYOUT_RES_SURFACE_CHANGED marks the creation
        let frames = client_frames(placement, task_rect(&self.tasks, task), seq);
        let (surface, placed) = {
            let windows = self.lock();
            let record = windows.iter().find(|w| w.client == client);
            (
                record.and_then(|w| w.surface.clone()),
                record.map(|w| w.position),
            )
        };
        // the window's layer goes to its surface origin (WindowState.updateSurfacePosition), asked without the windows lock; a refused call is asked again at the next relayout
        let frame = &frames.frame;
        let origin = crate::window_layout::surface_origin(&placement, (frame.left, frame.top));
        if let Some(layer_id) = surface
            .as_ref()
            .and_then(|s| s.native.as_ref())
            .map(|n| n.layer_id)
            && placed != Some(origin)
            && self.agent.place(layer_id, origin)
            && let Some(record) = self.lock().iter_mut().find(|w| w.client == client)
        {
            record.position = origin;
            // the line run-apk / a gate reads the placement from: the frame the window now shows, in its Task, and its layer's origin
            activity::log(format_args!(
                "window {} placed at {},{} size {}x{} surface at {},{}",
                record.title,
                frame.left,
                frame.top,
                frame.right - frame.left,
                frame.bottom - frame.top,
                origin.0,
                origin.1
            ));
        }
        // the dim follows every relayout (a resize relayouts too, with new Task bounds); a window that stopped dimming drops its dim layer
        if let Some(layer_id) = surface
            .as_ref()
            .and_then(|s| s.native.as_ref())
            .map(|n| n.layer_id)
            && (placement.dim() > 0.0 || previous.dim() > 0.0)
        {
            self.agent
                .dim(layer_id, placement.dim(), &frames.parent_frame);
        }
        let display = self.tasks.display();
        let size = self.tasks.size(task);
        let surface_changed = created.is_some();
        // one line per relayout: the frames the client lays out in
        activity::log(format_args!(
            "window relayout {} requested {}x{} frame {} display_frame {} parent_frame {}",
            title,
            requested_width,
            requested_height,
            show_rect(&frames.frame),
            show_rect(&frames.display_frame),
            show_rect(&frames.parent_frame),
        ));
        *out_relayout_result = Some(WindowRelayoutResult {
            frames,
            merged_configuration: MergedConfiguration {
                global: activity::global_configuration(&display),
                override_config: Configuration {
                    seq: self.tasks.config_seq(task),
                    ..activity::task_override(&display, size)
                },
                merged: activity::task_configuration(&display, size),
            },
            surface_control: surface.unwrap_or_default(),
            insets_state: display_insets(&self.tasks),
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

    /// WindowManagerService.removeWindow: the window leaves the session, and its input channel leaves the app's dispatcher (WindowState.removeImmediately -> disposeInputChannel -> InputDispatcher::removeInputChannel), asked
    /// without the windows lock. A window the session does not hold is nothing to remove (AOSP does nothing for an unknown window either).
    fn r#remove(&self, client_token: Option<&SpIBinder>) -> binder::Result<()> {
        let Some(token) = client_token else {
            counters::refused(WINDOW_SESSION, "remove: no client token");
            return Err(StatusCode::BAD_VALUE.into());
        };
        let (removed, layers): (Vec<SpIBinder>, Vec<i32>) = {
            let mut windows = self.lock();
            let mine = || windows.iter().filter(|w| w.client == *token);
            let inputs = mine().filter_map(|w| w.input.clone()).collect();
            let layers = mine()
                .filter_map(|w| w.surface.as_ref()?.native.as_ref().map(|n| n.layer_id))
                .collect();
            windows.retain(|w| w.client != *token);
            (inputs, layers)
        };
        // the dim goes with the window (Dimmer drops the dim layer of a window that no longer dims)
        if self.tasks.placement(token).is_some_and(|p| p.dim() > 0.0) {
            for layer_id in layers {
                self.agent.dim(layer_id, 0.0, &rect(0, 0, 0, 0));
            }
        }
        self.tasks.remove_window(token);
        for input in removed {
            self.agent.remove_input(input);
        }
        Ok(())
    }

    /// WindowManagerService.finishDrawingWindow (WindowManagerService.java:2879-2903) -> WindowState.finishDrawing (WindowState.java:5645-5703): with no sync active the post-draw Transaction
    /// reaches WindowStateAnimator.finishDrawingLocked (:251-252), merged into the window's pending transaction, which the traversal requestTraversal asks for applies to SurfaceFlinger.
    /// Here the app's composer is that SurfaceFlinger: the Transaction goes to it unchanged (bytes + objects, never decoded here) and applies with its next tick.
    /// Not modelled: BLAST sync groups (mPrepareSyncSeqId, so `seq_id` is unread and no buffer is dropped as out of date), draw handlers, AsyncRotationController, the draw state machine
    /// (mDrawState / commitFinishDrawingLocked) and `sanitize` (the composer only serves its own app). A window this session does not hold is ignored, counted (windowForClientLocked: null).
    fn r#finishDrawing(
        &self,
        window: Option<&Strong<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>>,
        post_draw: Option<&SurfaceControl_Transaction>,
        _seq_id: i32,
    ) -> binder::Result<()> {
        let known = window.is_some_and(|w| {
            let client = w.as_binder();
            self.lock().iter().any(|x| x.client == client)
        });
        if !known {
            counters::answered(WINDOW_SESSION, "finishDrawing: not this session's window");
            return Ok(());
        }
        let Some(t) = post_draw.filter(|t| t.present) else {
            return Ok(());
        };
        let Some(agent) = self.agent.agent("finishDrawing") else {
            return Ok(());
        };
        if let Err(e) = agent.post_draw_transaction(t) {
            counters::answered(
                WINDOW_SESSION,
                &format!("finishDrawing: UI agent refused the post-draw transaction: {e:?}"),
            );
        }
        Ok(())
    }
}

/// A new window service binder: the Tasks (which own the display every session lays out on) and the table of UI agents (system_services.rs gives it the registry's).
pub fn new_binder(
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
    permissions: Arc<Permissions>,
) -> SpIBinder {
    // finishDrawing's Transaction codec finds where the Transaction ends with libgui's reader (atl-parcelables cannot link it)
    SurfaceControl_Transaction_codec::set_framer(Transaction::frame);
    BnWindowManager::new_binder(
        WindowManager {
            agents,
            tasks,
            permissions,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

/// `task` changed size: each of its windows gets `IWindow.resized` (oneway) with the frames, merged configuration and insets of the new size. Returns the windows told.
/// AOSP's WindowState.reportResized (WindowState.java:3584) sends the same call; it skips a relaunching activity (`inRelaunchingActivity`), so callers do too (activity.rs task_resized, relaunch_finished).
/// forceLayout is set so the app relayouts with the new frames; reportDraw is false: no BLAST sync or draw state is modelled (Session::finishDrawing).
pub fn notify_resized(tasks: &Tasks<SpIBinder>, task: TaskId) -> usize {
    let display = tasks.display();
    let size = tasks.size(task);
    let merged = MergedConfiguration {
        global: activity::global_configuration(&display),
        override_config: Configuration {
            seq: tasks.config_seq(task),
            ..activity::task_override(&display, size)
        },
        merged: activity::task_configuration(&display, size),
    };
    let clients = tasks.windows_of(task);
    for (client, placement) in &clients {
        let window = match client
            .clone()
            .into_interface::<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>()
        {
            Ok(window) => window,
            Err(e) => {
                counters::answered(
                    WINDOW_SESSION,
                    &format!("resized: client is not an IWindow: {e:?}"),
                );
                continue;
            }
        };
        // a popup keeps its requested size and gravity, a MATCH_PARENT window takes the new bounds (client_frames, as relayout)
        let frames = client_frames(*placement, task_rect(tasks, task), tasks.next_seq());
        let insets = display_insets(tasks);
        if let Err(e) = window.r#resized(
            Some(&frames),
            false,
            Some(&merged),
            Some(&insets),
            true,
            false,
            activity::DEFAULT_DISPLAY,
            0,
            false,
            None,
        ) {
            counters::answered(WINDOW_SESSION, &format!("resized: IWindow refused: {e:?}"));
        }
    }
    clients.len()
}
