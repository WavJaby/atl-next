// SPDX-License-Identifier: MIT
//! UI mode's window (M3 slice 6): the process's one window, a GTK window of the UI thread that shows the compositor's display. The compositor's clock thread publishes each scene into the [`Mailbox`] and
//! asks for a frame; the window's frame clock takes the latest there and presents it, and its paint reports the buffers it stopped drawing back into the mailbox. Nothing the compositor does waits for this
//! thread (owner D1, 2026-11-08). Its pointer events reach the input thread ([`Message::Event`]).
//! GTK objects live on the UI thread only: [`install`] keeps the session in a thread-local on the thread that attached the toolkit (the launcher's startup) and opens the window there, before any client; no other thread touches GTK.
//! The window is one Task's toplevel (freeform: the display is the monitor, not this window): one per process, opened at install at its Task's size and app id (registry TASK_INFO, start.rs) (the app's first traversal waits for a vsync, which this window's frame clock supplies). Its size changes go to the services as the Task's bounds, the monitor it is on as the display (task_sender.rs); the monitor is also this process's compositor display (Composer::set_display).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use atl_platform_ui::{
    Capture, FrameClock, FrameRequester, InputSource, PlatformUi, PlatformWindow, Ui, UiError,
    UiInvoker, Window, WindowAttributes,
};

use atl_services_types::display_model::DisplayModel;

use crate::host::Composer;
use crate::input::{InputSender, Message};
use crate::mailbox::Mailbox;
use crate::task_sender::TaskSender;

/// The UI thread's session: the toolkit, the compositor and its mailbox, the input thread, the window's attributes at install (size, app id) and the window once it exists.
struct Session {
    ui: PlatformUi,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    attributes: WindowAttributes,
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

/// Whether captures can be served: UI mode attached the toolkit (a headless compositor has no window to read back).
pub fn available() -> bool {
    INVOKER.get().is_some()
}

/// Serves one capture on the UI thread: presents the scene the clock published and not yet shown (if any), reads the window back, and calls `answer` with the picture (or why there is none).
/// The clock is not asked for a frame: the caller polls, and the next publish presents as usual.
pub fn capture(answer: Box<dyn FnOnce(Result<Capture, UiError>) + Send>) {
    let Some(invoker) = INVOKER.get() else {
        return;
    };
    invoker.invoke(Box::new(move || {
        SESSION.with_borrow(|session| {
            let Some(session) = session.as_ref() else {
                return answer(Err(UiError::Capture("no window session".to_owned())));
            };
            let Some(window) = session.window.as_ref() else {
                return answer(Err(UiError::Capture("the window is not open".to_owned())));
            };
            if let Some(taken) = session.mailbox.take() {
                let shown = window.present(&taken.scene);
                if let Err(e) = shown {
                    tracing::warn!("atl-composer: ui present: {e}");
                    session.mailbox.refused(taken);
                    session.composer.presenter_reported();
                }
            }
            answer(window.capture());
        });
    }));
}

/// Keeps the session on the calling thread, which must be the UI thread that attached `ui`, and opens the window now (the services expect the Task's window from the start).
pub fn install(
    ui: PlatformUi,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    attributes: WindowAttributes,
    sender: Arc<TaskSender>,
) {
    let _ = INVOKER.set(ui.invoker());
    SESSION.with(|cell| {
        *cell.borrow_mut() = Some(Session {
            ui,
            composer,
            mailbox,
            input,
            attributes,
            sender,
            window: None,
        });
    });
    open();
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
        let window = match PlatformWindow::create(&session.ui, &session.attributes) {
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
        let mailbox = Arc::clone(&session.mailbox);
        let composer = Arc::clone(&session.composer);
        let (mut tick_count, mut presented) = (0u32, 0u32);
        let requester = window.frame_requester();
        session
            .composer
            .set_frame_requester(Box::new(move || requester.request_frame()));
        window.on_tick(move |tick| {
            let time_ns = tick.time_ns;
            if let Some(taken) = mailbox.take() {
                tracing::debug!(
                    "atl-composer: ui: present n={} time_ns={time_ns}",
                    presented + 1
                );
                match shown.present(&taken.scene) {
                    Ok(()) => {
                        presented += 1;
                        if presented == 1 {
                            let _ = first_frame.send(Message::FirstFrame);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("atl-composer: ui present: {e}");
                        mailbox.refused(taken);
                        composer.presenter_reported();
                    }
                }
            }
            if tick_count == 0 {
                tracing::info!("atl-composer: ui: first frame mapped={}", shown.is_mapped());
            }
            tick_count += 1;
        });
        // a paint ends the GPU reads of the buffers it stopped showing: they leave the hold set with their read fences (none = export failed, reusable now), and the clock releases them
        let reports = Arc::clone(&session.mailbox);
        let composer = Arc::clone(&session.composer);
        window.on_painted(move |p| {
            reports.released(
                p.releases
                    .into_iter()
                    .map(|r| (r.buffer_key, r.release_fence)),
            );
            composer.presenter_reported();
        });
        // the presented frames' timings (the display's feedback) are the clock's vsync grid: the clock moves onto each one
        let presented = Arc::clone(&session.composer);
        window.on_presented(move |sample| presented.on_presented(sample));
        let sizes = Arc::clone(&session.sender);
        window.on_resize(move |size| sizes.submit_size(size));
        // the monitor is this process's compositor display (its vsync interval, SurfaceFlinger's display info) and the services' display (DisplayInfo, Configuration)
        let monitors = Arc::clone(&session.sender);
        let frames = Arc::clone(&session.composer);
        window.on_monitor(move |m| {
            let model = DisplayModel::from_monitor(
                m.size.width,
                m.size.height,
                m.refresh_hz,
                m.scale.get(),
                m.rotation.quarter_turns(),
            );
            if frames.set_display(model) {
                tracing::info!("atl-composer: ui: display {}", model.to_arg());
            }
            monitors.submit_display(model);
        });
        window.request_frame();
        // the in-app input method's desktop side: the window's TextInput
        atl_ime::attach(Rc::clone(&window));
        session.window = Some(window);
    });
}
