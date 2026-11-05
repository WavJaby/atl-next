// SPDX-License-Identifier: MIT
//! UI mode's window (M3 slice 6): the process's one window, a GTK window of the UI thread that shows the compositor's display. Its frame clock ticks the compositor ([`UiFrames::tick`], no clock thread) and its pointer events reach the input thread ([`Message::Event`]).
//! GTK objects live on the UI thread only: [`install`] keeps the session in a thread-local on the thread that attached the toolkit (the launcher's startup) and opens the window there, before any client; no other thread touches GTK.
//! The window is one Task's toplevel (freeform: the display is the monitor, not this window): one per process, opened at install at its Task's opening size (DisplayModel::opening_size) (the app's first traversal waits for a vsync, which this window's frame clock supplies). Its size changes go to the services as the Task's bounds, the monitor it is on as the display (task_sender.rs); the monitor is also this process's compositor display (Composer::set_display).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};

use atl_platform_ui::{
    FrameClock, FrameRequester, InputSource, PhysSize, PlatformUi, PlatformWindow, Ui, UiInvoker,
    Window, WindowAttributes,
};

use atl_services::display_model::DisplayModel;

use crate::host::UiFrames;
use crate::input::Message;
use crate::sink::fnv1a64;
use crate::task_sender::TaskSender;

/// Set by the window gates: each settled picture is read back and logged (`atl-composer: ui: capture ...`). Unset in normal runs: a readback is a GPU render plus a download on the UI thread.
const CAPTURE_ENV: &str = "ATL_UI_CAPTURE";
/// Ticks without a new scene after which the picture counts as settled: 1/3 s at 60 Hz, longer than the gaps between the frames of one app redraw.
const SETTLED_TICKS: u32 = 20;

/// The UI thread's session: the toolkit, the compositor's frame path, the input thread, the window's opening size and the window once it exists.
struct Session {
    ui: PlatformUi,
    frames: Arc<UiFrames>,
    input: Sender<Message>,
    size: PhysSize,
    /// the window's size and monitor changes go here (the Task's bounds and the display in the services)
    sender: Arc<TaskSender>,
    window: Option<Rc<PlatformWindow>>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// The UI thread's invoker, set at install: other threads' calls into the window go through it.
static INVOKER: OnceLock<<PlatformUi as Ui>::Invoker> = OnceLock::new();

/// Android pointer icon `kind` (IInputManager.setPointerIcon, relayed by the services' UI-agent call) as the window's cursor; from any thread. A type with no desktop cursor keeps the current one (cursor.rs).
pub fn set_pointer_icon(kind: i32) {
    let (Some(invoker), Some(shape)) = (INVOKER.get(), crate::cursor::shape(kind)) else {
        return;
    };
    invoker.invoke(Box::new(move || {
        SESSION.with_borrow(|session| {
            if let Some(window) = session.as_ref().and_then(|s| s.window.as_ref()) {
                tracing::info!("atl-composer: ui: cursor type={kind} shape={shape:?}");
                window.set_cursor(shape);
            }
        });
    }));
}

/// Keeps the session on the calling thread, which must be the UI thread that attached `ui`, and opens the window now. The window cannot wait for the first client: the app's first traversal waits for a vsync, and vsync comes from this window's frame clock.
pub fn install(
    ui: PlatformUi,
    frames: UiFrames,
    input: Sender<Message>,
    size: PhysSize,
    sender: Arc<TaskSender>,
) {
    let _ = INVOKER.set(ui.invoker());
    SESSION.with(|cell| {
        *cell.borrow_mut() = Some(Session {
            ui,
            frames: Arc::new(frames),
            input,
            size,
            sender,
            window: None,
        });
    });
    open();
}

/// The readback of a settled window: its digest and size, and the frames presented so far, for the gate
/// (`atl-composer: ui: capture`); the gate compares the digest with its own expectation.
fn capture_report(window: &PlatformWindow, presented: u32) {
    let capture = match window.capture() {
        Ok(capture) => capture,
        Err(e) => {
            tracing::warn!("atl-composer: ui: capture: {e}");
            return;
        }
    };
    tracing::info!(
        "atl-composer: ui: capture presented={presented} size={}x{} fnv1a64={:016x}",
        capture.size.width,
        capture.size.height,
        fnv1a64(&capture.rgba)
    );
}

/// Whether the window clock asks for another tick: only while the compositor wants one (a vsync request, a queued transaction or buffer, calls to deliver: UiFrames::wants_frame) or a gate's readback waits for the picture to settle.
/// Otherwise it stops, as an app's vsync does on Android: SurfaceFlinger's EventThread (EventThread.cpp:622 `shouldConsumeEvent`) sends a connection vsync only after its `requestNextVsync` (:401), which DisplayEventDispatcher::scheduleVsync (DisplayEventDispatcher.cpp:79-93) makes for Choreographer.scheduleVsyncLocked (Choreographer.java:1214), one frame at a time.
/// Any later request restarts it (UiFrames::set_frame_requester).
fn keep_ticking(wants_frame: bool, settling: bool) -> bool {
    wants_frame || settling
}

/// Creates the window, once, on the UI thread.
fn open() {
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return;
        };
        tracing::info!("atl-composer: ui: opening the window");
        if session.window.is_some() {
            return;
        }
        let attributes = WindowAttributes {
            title: "atl".to_owned(),
            size: session.size,
        };
        let window = match PlatformWindow::create(&session.ui, &attributes) {
            Ok(window) => Rc::new(window),
            Err(e) => {
                tracing::error!("atl-composer: ui window: {e}");
                return;
            }
        };
        let input = session.input.clone();
        window.on_events(move |event| {
            // an error means the input thread is gone: the process is ending
            let _ = input.send(Message::Event(event));
        });
        let shown = Rc::clone(&window);
        // the scripted pointer (run-apk --pointer) starts at the first present, as the headless one at its first frame
        let first_frame = session.input.clone();
        let frames = Arc::clone(&session.frames);
        let capture = std::env::var_os(CAPTURE_ENV).is_some();
        let (mut tick_count, mut presented, mut quiet_ticks) = (0u32, 0u32, 0u32);
        // ticks that ran with nothing wanting them (not the opening tick): the on-demand clock keeps this 0
        let mut unwanted = 0u32;
        let requester = window.frame_requester();
        session
            .frames
            .set_frame_requester(Box::new(move || requester.request_frame()));
        window.on_tick(move |tick| {
            let time_ns = i64::try_from(tick.time_ns).unwrap_or(i64::MAX);
            let settling = capture && presented > 0 && quiet_ticks < SETTLED_TICKS;
            if tick_count > 0 && !frames.wants_frame() && !settling {
                unwanted += 1;
            }
            quiet_ticks = quiet_ticks.saturating_add(1);
            frames.tick(time_ns, |scene| {
                presented += 1;
                quiet_ticks = 0;
                if presented == 1 {
                    let _ = first_frame.send(Message::FirstFrame);
                }
                tracing::debug!("atl-composer: ui: present n={presented} time_ns={time_ns}");
                if let Err(e) = shown.present(scene) {
                    tracing::warn!("atl-composer: ui present: {e}");
                }
            });
            if tick_count == 0 {
                tracing::info!("atl-composer: ui: first frame mapped={}", shown.is_mapped());
            }
            if capture && presented > 0 && quiet_ticks == SETTLED_TICKS {
                capture_report(&shown, presented);
            }
            tick_count += 1;
            let settling = capture && presented > 0 && quiet_ticks < SETTLED_TICKS;
            if keep_ticking(frames.wants_frame(), settling) {
                shown.request_frame();
            } else if capture {
                // gates only (one line per stop: an animating app stops and restarts every frame)
                tracing::info!(
                    "atl-composer: ui: clock stopped ticks={tick_count} unwanted={unwanted}"
                );
            }
        });
        let sizes = Arc::clone(&session.sender);
        window.on_resize(move |size| sizes.submit_size(size));
        // the monitor is this process's compositor display (its vsync interval, SurfaceFlinger's display info) and the services' display (DisplayInfo, Configuration)
        let monitors = Arc::clone(&session.sender);
        let frames = Arc::clone(&session.frames);
        window.on_monitor(move |m| {
            let model = DisplayModel::from_monitor(
                m.size.width,
                m.size.height,
                m.refresh_hz,
                m.scale.get(),
            );
            if frames.set_display(model) {
                tracing::info!("atl-composer: ui: display {}", model.to_arg());
            }
            monitors.submit_display(model);
        });
        window.request_frame();
        session.window = Some(window);
    });
}
