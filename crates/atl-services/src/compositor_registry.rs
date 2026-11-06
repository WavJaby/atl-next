// SPDX-License-Identifier: MIT
//! The compositor registry (M3 slice 4, plan C3 "Amended 2026-11-04"): how an app process's in-process SurfaceFlinger tells the system process about its UI agent (ui_agent.rs) and its window.
//! The two service objects themselves are not registered anywhere: the app process serves them under AOSP's names to its own lookups (atl-service-view), so libgui finds them unchanged; other processes never need them.
//! The registry keys what it holds by the caller's uid, taken from the transaction. Processes of one uid trust each other (one app); a restarted process re-registers and replaces the dead entry.
//!
//! This module is the protocol (names, codes, properties): plain Rust, shared by the app side (atl-composer) and the registry service (`registry_service`, feature `converter`).

/// The registry's name in servicemanager.
pub const SERVICE: &str = "atl.compositor_registry";
/// Its interface descriptor.
pub const DESCRIPTOR: &str = "atl.ICompositorRegistry";
/// `IBinder.FIRST_CALL_TRANSACTION`: the app's UI agent binder; reply: a status only.
pub const REGISTER: u32 = 1;
/// The transaction a UI-mode app process sends when its window's content size changes: the Task's number (the one its UI agent was named, ui_agent.rs TASK), then width and height (i32, physical pixels). The caller's uid must own that Task (tasks.rs). Reply: a status only.
pub const SET_BOUNDS: u32 = 2;
/// The transaction a UI-mode app process sends when the monitor its window is on changes (another monitor, or its scale or mode): the monitor as Android's display, a String16 in the `--display` form (display_model.rs `DisplayModel::to_arg`). The caller's uid must own a Task (an app with a window). Reply: a status only.
pub const SET_DISPLAY: u32 = 3;
/// The system property holding the display the in-app compositor answers with (`DisplayModel::to_arg`): the same model atld forwards to the services, delivered to the app the way its other environment is. Its presence marks an ATL app process (atl-composer start).
pub const DISPLAY_PROPERTY: &str = "ro.atl.display";
/// The system property holding the headless pointer script (atld `--headless-pointer`, parsed by atl-composer `input::parse_script`), delivered like [`DISPLAY_PROPERTY`]. Unset: no input thread; the window handoff still works.
pub const POINTER_PROPERTY: &str = "ro.atl.headless.pointer";
/// The system property selecting an app process's UI mode (atld `--ui`): `gtk` presents the compositor's scenes into a GTK window of the process, on its main thread (atl-app-launcher lib.rs `start_ui`, atl-composer). Unset: headless.
pub const UI_PROPERTY: &str = "ro.atl.ui";
/// AOSP's service names, libgui's lookups (SurfaceComposerClient.cpp ComposerService / ComposerServiceAIDL), served in-process.
pub const SURFACE_FLINGER: &str = "SurfaceFlinger";
pub const SURFACE_FLINGER_AIDL: &str = "SurfaceFlingerAIDL";
