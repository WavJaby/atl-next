// SPDX-License-Identifier: MIT
//! UI mode's window (M3 slice 6): the process's one window, a GTK window on the platform's display thread that shows the compositor's display. The compositor's clock thread publishes each scene into the [`Mailbox`] and
//! asks for a frame; the window's frame clock takes the latest there and presents it, and its paint reports the buffers it stopped drawing back into the mailbox. Nothing the compositor does waits for this
//! thread (owner D1, 2026-11-08). Its pointer events reach the input thread ([`Message::Event`]).
//! GTK objects live on the display thread only (the platform module's own thread, started when the toolkit attaches): the launcher's startup attaches the toolkit and [`open`]s the window before ART boots, showing the starting window (`ro.atl.splash`); [`install`] keeps the session in a thread-local of the display thread and wires the window to the compositor. Both run their work there by `call`, so the caller holds only the Send [`OpenWindow`] token; no other thread touches GTK.
//! The window is one Task's toplevel (freeform: the display is the monitor, not this window): one per process that has a Task (none for a provider, service or receiver process), opened at its Task's size and app id (registry TASK_INFO, start.rs) (the app's first traversal waits for a vsync, which this window's frame clock supplies). Its size changes go to the services as the Task's bounds, the monitor it is on as the display (task_reports.rs); the monitor is also this process's compositor display (Composer::set_display).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use atl_compositor::scene::{Content, Quad, Scene};
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui::{
    Capture, DisplayInvoker, FrameClock, FrameRequester, InputSource, LooperPoster, PlatformUi,
    PlatformWindow, Ui, UiError, Window, WindowAttributes,
};

use atl_services_types::compositor_registry::Splash;
use atl_services_types::display_model::DisplayModel;

use crate::frame_rate::{FrameRate, largest_buffer};
use crate::host::Composer;
use crate::input::{InputSender, Message};
use crate::mailbox::Mailbox;
use crate::task_reports::TaskReports;

/// The display thread's session: the toolkit (kept attached for the process), the compositor and its mailbox, the input thread and the window once wired.
struct Session {
    _ui: PlatformUi,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    /// the window's size and monitor changes go here (the Task's bounds and the display in the services)
    reports: Arc<TaskReports>,
    window: Option<Rc<PlatformWindow>>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// The window [`open`] made, on the display thread, until [`install`] wires it into [`SESSION`].
    static OPENED: RefCell<Option<Rc<PlatformWindow>>> = const { RefCell::new(None) };
}

/// The display thread's invoker, set at install: other threads' calls into the window go through it.
static INVOKER: OnceLock<<PlatformUi as Ui>::Invoker> = OnceLock::new();

/// The outbox (outbox.rs), set at install with [`INVOKER`]: the binder calls the window's thread starts run there.
static OUTBOX: OnceLock<Arc<dyn LooperPoster>> = OnceLock::new();

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

/// The services' moveTaskToBack of this app's Task (an unhandled Back on its root activity, UI-agent MINIMIZE): the window is minimized; from any thread. Headless mode has no window.
pub fn minimize() {
    let Some(invoker) = INVOKER.get() else {
        return;
    };
    invoker.invoke(Box::new(|| {
        SESSION.with_borrow(|session| {
            if let Some(window) = session.as_ref().and_then(|s| s.window.as_ref()) {
                window.minimize();
            }
        });
    }));
}

/// The task's title (UI agent TASK_TITLE) as the window's title; from any thread. Logged as `atl-composer: ui: title "<title>"`. Headless mode has no window.
pub fn set_title(title: String) {
    let Some(invoker) = INVOKER.get() else {
        return;
    };
    invoker.invoke(Box::new(move || {
        SESSION.with_borrow(|session| {
            if let Some(window) = session.as_ref().and_then(|s| s.window.as_ref()) {
                tracing::info!("atl-composer: ui: title {title:?}");
                window.set_title(&title);
            }
        });
    }));
}

/// Whether captures can be served: UI mode attached the toolkit (a headless compositor has no window to read back).
pub fn available() -> bool {
    INVOKER.get().is_some()
}

/// Serves one capture on the display thread: presents the scene the clock published and not yet shown (if any), reads the window back, and calls `answer` with the picture (or why there is none) on the
/// outbox: the answer allocates a buffer and replies to the listener, binder calls the window's thread does not make (outbox.rs). The clock is not asked for a frame: the caller polls, and the next publish presents as usual.
pub fn capture(answer: Box<dyn FnOnce(Result<Capture, UiError>) + Send>) {
    let (Some(invoker), Some(outbox)) = (INVOKER.get(), OUTBOX.get()) else {
        return;
    };
    let outbox = Arc::clone(outbox);
    invoker.invoke(Box::new(move || {
        let answer = move |picture| outbox.post(Box::new(move || answer(picture)));
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

/// The Task's window, opened by the launcher before ART boots ([`open`]) and showing the starting window ([`Splash`]) until [`install`] hands it to the compositor and the app's first present replaces it.
/// A Send token: the window itself stays on the display thread ([`OPENED`]); the caller holds only the session handle (Send + Sync, no toolkit object) and the splash's paint flag.
pub struct OpenWindow {
    /// taken by [`install`]; Some until then
    ui: Option<PlatformUi>,
    /// the splash's first paint has happened (or there is no splash to wait for); set on the display thread
    painted: Arc<PaintedFlag>,
    /// the launcher's start: the origin of the `splash:` lines' `after=`
    started: Instant,
    splash: bool,
}

// The token crosses from the launcher's thread to the display thread, and the window must not: checked at compile time.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<OpenWindow>();
    // compiles only while PlatformWindow is not Send (both impls would apply to a Send type, and the `_` would be ambiguous)
    trait AmbiguousIfSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    struct IsSend;
    impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}
    let _ = <PlatformWindow as AmbiguousIfSend<_>>::check;
};

/// The splash's first paint, set once on the display thread and waited for on the launcher's thread.
struct PaintedFlag {
    set: Mutex<bool>,
    cv: Condvar,
}

impl PaintedFlag {
    fn new(set: bool) -> Self {
        Self {
            set: Mutex::new(set),
            cv: Condvar::new(),
        }
    }

    /// Sets the flag; true the first time.
    fn set(&self) -> bool {
        let mut set = self.set.lock().unwrap_or_else(|p| p.into_inner());
        let first = !*set;
        *set = true;
        self.cv.notify_all();
        first
    }

    /// Waits up to `timeout` for the flag; whether it is set.
    fn wait(&self, timeout: Duration) -> bool {
        let set = self.set.lock().unwrap_or_else(|p| p.into_inner());
        let (set, _) = self
            .cv
            .wait_timeout_while(set, timeout, |set| !*set)
            .unwrap_or_else(|p| p.into_inner());
        *set
    }
}

impl OpenWindow {
    /// Waits up to `timeout` until the splash has been painted (or there is none): the launcher's wait before ART boot. The display thread paints on its own loop, so this thread only waits; false = not painted in time.
    pub fn wait_painted(&self, timeout: Duration) -> bool {
        self.painted.wait(timeout)
    }

    /// The display thread's invoker (the in-app input method's calls into the window).
    pub(crate) fn invoker(&self) -> Option<<PlatformUi as Ui>::Invoker> {
        self.ui.as_ref().map(Ui::invoker)
    }
}

/// A token dropped before [`install`] (the compositor failed to start) closes its window on the display thread, as dropping the window itself did.
impl Drop for OpenWindow {
    fn drop(&mut self) {
        if let Some(ui) = &self.ui {
            ui.invoker().invoke(Box::new(|| drop(OPENED.take())));
        }
    }
}

/// ms since `started`, for the `splash:` lines.
fn after_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

/// The starting window as a scene: one colour quad over the window's content (`size`), the colour int's alpha as the quad's opacity.
fn splash_scene(splash: Splash, size: atl_platform_ui::PhysSize) -> Scene {
    let [a, r, g, b] = splash.background.to_be_bytes();
    let unit = |c: u8| f32::from(c) / f32::from(u8::MAX);
    Scene {
        frame: 0,
        layer_stack: 0,
        quads: vec![Quad {
            layer_id: SPLASH_LAYER,
            name: "Splash Screen".to_owned(),
            content: Content::Color([unit(r), unit(g), unit(b)]),
            dst: FloatRect {
                left: 0.0,
                top: 0.0,
                right: size.width as f32,
                bottom: size.height as f32,
            },
            clip: None,
            alpha: unit(a),
        }],
    }
}

/// The splash quad's layer id: below every id the compositor hands out (its ids start at 1), so it names no app layer.
const SPLASH_LAYER: i32 = 0;

/// Opens the Task's window on the display thread of `ui` (by `call`, from any app thread), before the compositor exists, and presents the starting window into it. With no `splash` (`ro.atl.splash` unset)
/// the window opens empty, logged `splash: none (no spec)` and counted. The window's frame clock is not wired to anything until [`install`].
pub fn open(
    ui: PlatformUi,
    attributes: WindowAttributes,
    splash: Option<Splash>,
    started: Instant,
) -> Result<OpenWindow, String> {
    let invoker = ui.invoker();
    atl_platform_ui::call(&invoker, move || {
        open_on_display(ui, &attributes, splash, started)
    })
    .ok_or_else(|| "ui window: the display thread ended".to_owned())?
}

/// [`open`]'s work, on the display thread: the window goes to [`OPENED`], the token back to the caller.
fn open_on_display(
    ui: PlatformUi,
    attributes: &WindowAttributes,
    splash: Option<Splash>,
    started: Instant,
) -> Result<OpenWindow, String> {
    tracing::info!("atl-composer: ui: opening the window");
    let window =
        Rc::new(PlatformWindow::create(&ui, attributes).map_err(|e| format!("ui window: {e}"))?);
    let painted = Arc::new(PaintedFlag::new(splash.is_none()));
    match splash {
        Some(splash) => {
            window
                .present(&splash_scene(splash, attributes.size))
                .map_err(|e| format!("splash present: {e}"))?;
            let done = Arc::clone(&painted);
            window.on_painted(move |_| {
                if done.set() {
                    tracing::info!(
                        "splash: shown after={} background={}",
                        after_ms(started),
                        splash.to_arg()
                    );
                }
            });
            window.request_frame();
        }
        None => {
            atl_diag::count("splash", "none", Some("no spec"));
            tracing::info!("splash: none (no spec)");
        }
    }
    OPENED.set(Some(window));
    Ok(OpenWindow {
        ui: Some(ui),
        painted,
        started,
        splash: splash.is_some(),
    })
}

/// Keeps the session on the display thread that opened `opened` (by `call`, from any app thread: returns once it is installed, so the window is wired before the composer registers) and wires the window to the compositor: from here the compositor's scenes replace the starting window.
pub fn install(
    opened: OpenWindow,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    reports: Arc<TaskReports>,
    outbox: Arc<dyn LooperPoster>,
) -> Result<(), String> {
    let Some(invoker) = opened.invoker() else {
        return Err("ui window install: the window was installed before".to_owned());
    };
    let ran = invoker.call(Box::new(move || {
        install_on_display(opened, composer, mailbox, input, reports, outbox)
    }));
    if ran {
        Ok(())
    } else {
        Err("ui window install: the display thread ended".to_owned())
    }
}

/// [`install`]'s work, on the display thread.
fn install_on_display(
    mut opened: OpenWindow,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    reports: Arc<TaskReports>,
    outbox: Arc<dyn LooperPoster>,
) {
    let (Some(ui), started, splash) = (opened.ui.take(), opened.started, opened.splash) else {
        return;
    };
    drop(opened);
    let Some(window) = OPENED.take() else {
        tracing::warn!("atl-composer: ui: install without an opened window on this thread");
        return;
    };
    // before the invoker: `available` reads the invoker, and `capture` needs both
    let _ = OUTBOX.set(outbox);
    let _ = INVOKER.set(ui.invoker());
    SESSION.with(|cell| {
        *cell.borrow_mut() = Some(Session {
            _ui: ui,
            composer,
            mailbox,
            input,
            reports,
            window: None,
        });
    });
    wire(window, splash.then_some(started));
}

/// Connects the window to the session, once, on the display thread. `splash_started`: the launcher's start when a splash is shown, for the `splash: replaced` line at the app's first present.
fn wire(window: Rc<PlatformWindow>, splash_started: Option<Instant>) {
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return;
        };
        if session.window.is_some() {
            return;
        }
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
        let mut tick_count = 0u32;
        let mut rate = FrameRate::default();
        let requester = window.frame_requester();
        session
            .composer
            .set_frame_requester(Box::new(move || requester.request_frame()));
        window.on_tick(move |tick| {
            let time_ns = tick.time_ns;
            if let Some(taken) = mailbox.take() {
                // per frame: the first and each power-of-two count (M60 R2); the `composer: fps` line is the per-second summary
                let n = rate.total() + 1;
                if n.is_power_of_two() {
                    tracing::debug!("atl-composer: ui: present n={n} time_ns={time_ns}");
                }
                let buffer = largest_buffer(&taken.scene);
                let sizes = match shown.present(&taken.scene) {
                    Ok(()) => {
                        let size = shown.size();
                        Some((buffer, (size.width, size.height)))
                    }
                    Err(e) => {
                        tracing::warn!("atl-composer: ui present: {e}");
                        mailbox.refused(taken);
                        composer.presenter_reported();
                        None
                    }
                };
                let first = sizes.is_some() && rate.total() == 0;
                if let Some(second) = rate.record(time_ns, mailbox.stats().taken, sizes) {
                    tracing::info!("{}", second.line());
                }
                if first {
                    if let Some(started) = splash_started {
                        tracing::info!("splash: replaced after={}", after_ms(started));
                    }
                    let _ = first_frame.send(Message::FirstFrame);
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
        // the presented frames' timings (the display's feedback) choose the clock's vsync source tier; vsync-marked ones are its phase
        let presented = Arc::clone(&session.composer);
        window.on_presented(move |sample| presented.on_presented(sample));
        let sizes = Arc::clone(&session.reports);
        window.on_resize(move |size| sizes.submit_size(size));
        // the desktop closing the window is the Shell removing its Task (REMOVE_TASK, task_reports.rs)
        let closes = Arc::clone(&session.reports);
        window.on_close_requested(move || closes.submit_remove_task());
        // the monitor is this process's compositor display (its vsync interval, SurfaceFlinger's display info) and the services' display (DisplayInfo, Configuration)
        let monitors = Arc::clone(&session.reports);
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
        // the in-app input method's desktop side: the window's TextInput (its calls on the editor's connection go to the outbox, atl-ime field.rs)
        atl_ime::attach(Rc::clone(&window));
        session.window = Some(window);
    });
}
