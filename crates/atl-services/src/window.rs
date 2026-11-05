// SPDX-License-Identifier: MIT
//! The `window` service (Context.WINDOW_SERVICE): the converter's IWindowManager stub, and the sessions it opens (WMS-lite, M3 slice 5). The three answers an activity reads before
//! any window exists: hasNavigationBar: false. WindowManagerService.hasNavigationBar (WindowManagerService.java:6669-6677) asks the display policy, which reads
//! config_showNavigationBar (frameworks/base/core/res/res/values/config.xml:2743, false); the desktop has no software navigation bar.
//! getCurrentAnimatorScale: 1.0. WindowManagerService.getCurrentAnimatorScale (:3687-3691) returns mAnimatorDurationScaleSetting, whose default is 1.0f (:1052), or 0 with animations disabled.
//! openSession: a session for the caller's callback (WindowManagerService.openSession:6004-6005); the session's window methods are below.
//! Session, the window path: addToDisplayAsUser (WindowManagerService.addWindow: ADD_OKAY, the client end of the window's input channel (its server end goes to the app's UI agent), the insets, the frame, scale 1),
//! relayout (WindowManagerService.relayoutWindow: the client frames, the merged configuration, the SurfaceControl the window's UI agent gives, the insets), remove (the window goes:
//! its input channel's server end stays with the app: no close signal yet, a known gap). The frames are the Task's bounds (tasks.rs, freeform); the display frame is the display; no insets (the desktop shows no system bars).
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
use atl_parcelables::android::window::ClientWindowFrames;
use atl_parcelables::codecs::android::view::InputChannel::InputChannelCore;
use binder::{
    BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, StatusCode, Strong, ThreadState,
};

use crate::display_model::DisplayModel;
use crate::tasks::{TaskId, Tasks};
use crate::ui_agent::{IUiAgent, UiAgents};
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
/// WindowManagerGlobal.ADD_BAD_APP_TOKEN (WindowManagerGlobal.java:127): the window's token is no activity's, so the add fails and ViewRootImpl throws BadTokenException.
const ADD_BAD_APP_TOKEN: i32 = -1;

/// The UI agent of the app that opened a session (ui_agent.rs): the registry holds one per uid, so a session asks the agent of its own uid.
struct RegisteredAgent {
    agents: Arc<UiAgents>,
    uid: u32,
}

impl RegisteredAgent {
    /// The app's UI agent; None, counted under `call`, when the app registered no agent or it is not an IUiAgent.
    fn agent(&self, call: &str) -> Option<Strong<dyn IUiAgent>> {
        let Some(binder) = self.agents.get(self.uid) else {
            counters::count(
                WINDOW_SESSION,
                &format!("{call}: no UI agent registered for the app's uid"),
            );
            return None;
        };
        match binder.into_interface() {
            Ok(agent) => Some(agent),
            Err(e) => {
                counters::count(
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
                counters::count(
                    WINDOW_SESSION,
                    &format!("relayout: UI agent refused the window surface: {e:?}"),
                );
                None
            }
        }
    }

    /// Names the window's Task to the agent: the composer sends that Task's bounds with it (SET_BOUNDS). Counted when the agent refuses it.
    fn name_task(&self, task: TaskId) {
        let Some(agent) = self.agent("addToDisplayAsUser") else {
            return;
        };
        if let Err(e) = agent.task(task.raw()) {
            counters::count(
                WINDOW_SESSION,
                &format!("addToDisplayAsUser: UI agent refused the task: {e:?}"),
            );
        }
    }

    /// Hands a window's input channel server end to the agent, which publishes the window's input on it; false, counted, when no agent takes it (the fd then closes and the client reads EOF).
    fn give_input_server(&self, token: SpIBinder, fd: OwnedFd) -> bool {
        let Some(agent) = self.agent("addToDisplayAsUser") else {
            return false;
        };
        match agent.input_server(token, fd) {
            Ok(()) => true,
            Err(e) => {
                counters::count(
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
            counters::count(
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
    /// The token of the input channel the agent took; None for a window without one.
    input: Option<SpIBinder>,
}

struct WindowManager {
    model: DisplayModel,
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
}

/// The session openSession gives: the windows it added, and the display they lay out on.
struct Session {
    model: DisplayModel,
    agent: RegisteredAgent,
    tasks: Arc<Tasks<SpIBinder>>,
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

    /// WindowManagerService.setInTouchMode:4045: records the mode and has the dispatcher tell the display's other windows (TouchModeEvent). The caller changes its own mode locally after the call
    /// (ViewRootImpl.ensureTouchMode:7233-7242), and an error there is a RuntimeException in the app. No other app window shares this display process's input: accept.
    fn r#setInTouchMode(&self, _in_touch: bool, _display_id: i32) -> binder::Result<()> {
        // counted: the window gate's input stage requires this line (xtask window.rs touch_mode_check)
        counters::count(WINDOW_MANAGER, "setInTouchMode (answered)");
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
                model: self.model,
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
    /// The display's frame: the insets' display frame and the global bounds. A window's frame is [`Session::task_rect`].
    fn display_rect(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.model.width,
            bottom: self.model.height,
        }
    }

    /// A window's frame: from the origin, the size of its Task (tasks.rs). Freeform, so it is the window's bounds, not the display's.
    fn task_rect(&self, task: TaskId) -> Rect {
        let size = self.tasks.size(task);
        Rect {
            left: 0,
            top: 0,
            right: size.width,
            bottom: size.height,
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
        let Some(task) = self.tasks.task_of_activity(attrs.token.as_ref()) else {
            counters::refused(
                WINDOW_SESSION,
                "addToDisplayAsUser: the window's token names no activity's Task",
            );
            return Ok(ADD_BAD_APP_TOKEN);
        };
        self.agent.name_task(task);
        let (channel, input) = if attrs.input_features & INPUT_FEATURE_NO_INPUT_CHANNEL == 0 {
            let (server, client) = open_input_pair()?;
            // InputChannel.openInputChannelPair gives both ends a token; InputChannelCore reads it non-null (libinput readStrongBinder)
            let token = crate::token::new();
            // the server end is the app's: its UI agent publishes the window's input on it (no system InputDispatcher), keyed by the token as AOSP's dispatcher keys a connection
            let given = self.agent.give_input_server(token.clone(), server);
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
        self.tasks.add_window(task, window.as_binder());
        self.lock().push(Window {
            client: window.as_binder(),
            title: attrs.title.clone().unwrap_or_default(),
            task,
            laid_out: false,
            surface: None,
            input,
        });
        *out_input_channel = Some(channel);
        *insets_state = Some(self.insets_state());
        *active_controls = None;
        *attached_frame = Some(Rect::default());
        *size_compat_scale = Some(vec![COMPAT_SCALE_NONE]);
        // WMS dispatches the app's visibility to its windows (WindowState's app visibility): ViewRootImpl draws only while its app is visible, and it starts out not.
        if let Err(e) = window.r#dispatchAppVisibility(true) {
            counters::count(
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
        let (first, pending, task) = {
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
            )
        };
        // The agent is another process, asked without the windows lock. A window gets its surface once; a failed call is asked again at the next relayout.
        let created = pending.and_then(|title| self.agent.window_surface(&title));
        if let Some(surface) = &created
            && let Some(record) = self.lock().iter_mut().find(|w| w.client == client)
        {
            record.surface = Some(surface.clone());
        }
        // every relayout returns the window's surface once it has one (the client keeps its copy only while the reply carries it); RELAYOUT_RES_SURFACE_CHANGED marks the creation
        let surface = self
            .lock()
            .iter()
            .find(|w| w.client == client)
            .and_then(|w| w.surface.clone());
        let size = self.tasks.size(task);
        let surface_changed = created.is_some();
        *out_relayout_result = Some(WindowRelayoutResult {
            frames: ClientWindowFrames {
                frame: self.task_rect(task),
                display_frame: self.display_rect(),
                parent_frame: self.task_rect(task),
                attached_frame: None,
                is_parent_frame_clipped_by_display_cutout: false,
                compat_scale: COMPAT_SCALE_NONE,
                seq,
            },
            merged_configuration: MergedConfiguration {
                global: activity::global_configuration(&self.model),
                override_config: Configuration {
                    seq: self.tasks.config_seq(task),
                    ..activity::task_override(&self.model, size)
                },
                merged: activity::task_configuration(&self.model, size),
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

    /// WindowManagerService.removeWindow: the window leaves the session, and its input channel leaves the app's dispatcher (WindowState.removeImmediately -> disposeInputChannel -> InputDispatcher::removeInputChannel), asked
    /// without the windows lock. A window the session does not hold is nothing to remove (AOSP does nothing for an unknown window either).
    fn r#remove(&self, client_token: Option<&SpIBinder>) -> binder::Result<()> {
        let Some(token) = client_token else {
            counters::refused(WINDOW_SESSION, "remove: no client token");
            return Err(StatusCode::BAD_VALUE.into());
        };
        let removed: Vec<SpIBinder> = {
            let mut windows = self.lock();
            let inputs = windows
                .iter()
                .filter(|w| w.client == *token)
                .filter_map(|w| w.input.clone())
                .collect();
            windows.retain(|w| w.client != *token);
            inputs
        };
        self.tasks.remove_window(token);
        for input in removed {
            self.agent.remove_input(input);
        }
        Ok(())
    }
}

/// A new window service binder: the one display's model, which every session lays out on, and the table of UI agents (system_services.rs gives it the registry's).
pub fn new_binder(
    model: DisplayModel,
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
) -> SpIBinder {
    BnWindowManager::new_binder(
        WindowManager {
            model,
            agents,
            tasks,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

/// `task` changed size: each of its windows gets `IWindow.resized` (oneway) with the frames, merged configuration and insets of the new size. Returns the windows told.
/// AOSP's WindowState.reportResized (WindowState.java:3584) sends the same call; it skips a relaunching activity (`inRelaunchingActivity`), so callers do too (activity.rs task_resized, relaunch_finished).
/// forceLayout is set so the app relayouts with the new frames; reportDraw is false because finishDrawing is not modelled (module doc).
pub fn notify_resized(tasks: &Tasks<SpIBinder>, task: TaskId) -> usize {
    let display = tasks.display();
    let size = tasks.size(task);
    let rect = |right: i32, bottom: i32| Rect {
        left: 0,
        top: 0,
        right,
        bottom,
    };
    let merged = MergedConfiguration {
        global: activity::global_configuration(&display),
        override_config: Configuration {
            seq: tasks.config_seq(task),
            ..activity::task_override(&display, size)
        },
        merged: activity::task_configuration(&display, size),
    };
    let clients = tasks.windows_of(task);
    for client in &clients {
        let window = match client
            .clone()
            .into_interface::<dyn atl_aidl::mangled::_7_android_4_view_7_IWindow>()
        {
            Ok(window) => window,
            Err(e) => {
                counters::count(
                    WINDOW_SESSION,
                    &format!("resized: client is not an IWindow: {e:?}"),
                );
                continue;
            }
        };
        let frames = ClientWindowFrames {
            frame: rect(size.width, size.height),
            display_frame: rect(display.width, display.height),
            parent_frame: rect(size.width, size.height),
            attached_frame: None,
            is_parent_frame_clipped_by_display_cutout: false,
            compat_scale: COMPAT_SCALE_NONE,
            seq: tasks.next_seq(),
        };
        let insets = InsetsState {
            display_frame: rect(display.width, display.height),
            rounded_corner_frame: rect(display.width, display.height),
            seq: 0,
            sources: Vec::new(),
        };
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
            counters::count(WINDOW_SESSION, &format!("resized: IWindow refused: {e:?}"));
        }
    }
    clients.len()
}
