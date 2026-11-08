// SPDX-License-Identifier: MIT
//! UI mode's window (M3 slice 6): the process's one window, a GTK window of the UI thread that shows the compositor's display. The compositor's clock thread publishes each scene into the [`Mailbox`] and
//! asks for a frame; the window's frame clock takes the latest there and presents it, and its paint reports the buffers it stopped drawing back into the mailbox. Nothing the compositor does waits for this
//! thread (owner D1, 2026-11-08). Its pointer events reach the input thread ([`Message::Event`]).
//! GTK objects live on the UI thread only: [`install`] keeps the session in a thread-local on the thread that attached the toolkit (the launcher's startup) and opens the window there, before any client; no other thread touches GTK.
//! The window is one Task's toplevel (freeform: the display is the monitor, not this window): one per process, opened at install at its Task's size (registry TASK_BOUNDS, start.rs) (the app's first traversal waits for a vsync, which this window's frame clock supplies). Its size changes go to the services as the Task's bounds, the monitor it is on as the display (task_sender.rs); the monitor is also this process's compositor display (Composer::set_display).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use atl_platform_ui::{
    FrameClock, FrameRequester, InputSource, PhysSize, PlatformUi, PlatformWindow, Ui, UiInvoker,
    Window, WindowAttributes,
};

use atl_services::display_model::DisplayModel;

use crate::host::Composer;
use crate::input::{InputSender, Message};
use crate::mailbox::Mailbox;
use crate::sink::fnv1a64;
use crate::task_sender::TaskSender;

/// Set by the window gates: each settled picture is read back and logged (`atl-composer: ui: capture ...`). Unset in normal runs: a readback is a GPU render plus a download on the UI thread.
const CAPTURE_ENV: &str = "ATL_UI_CAPTURE";
/// Ticks without a new scene after which the picture counts as settled: 1/3 s at 60 Hz, longer than the gaps between the frames of one app redraw.
const SETTLED_TICKS: u32 = 20;

/// The UI thread's session: the toolkit, the compositor and its mailbox, the input thread, the window's size at install and the window once it exists.
struct Session {
    ui: PlatformUi,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
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

/// Keeps the session on the calling thread, which must be the UI thread that attached `ui`, and opens the window now (the services expect the Task's window from the start).
pub fn install(
    ui: PlatformUi,
    composer: Arc<Composer>,
    mailbox: Arc<Mailbox>,
    input: InputSender<Message>,
    size: PhysSize,
    sender: Arc<TaskSender>,
) {
    let _ = INVOKER.set(ui.invoker());
    SESSION.with(|cell| {
        *cell.borrow_mut() = Some(Session {
            ui,
            composer,
            mailbox,
            input,
            size,
            sender,
            window: None,
        });
    });
    open();
}

/// Window pixels each capture also reads, `x,y;x,y` (origin top-left, as the capture): one `atl-composer: ui: capture pixel` line each. Set by the window gate's stage gles2 (xtask/src/window/gles2.rs), which checks the composed picture at points it chose.
const PIXELS_ENV: &str = "ATL_UI_CAPTURE_PIXELS";

/// [`PIXELS_ENV`]'s points; a pair that does not parse is dropped, so the gate that wanted its line fails on the missing line.
fn pixel_points() -> Vec<(u32, u32)> {
    let Some(text) = std::env::var_os(PIXELS_ENV) else {
        return Vec::new();
    };
    text.to_string_lossy()
        .split(';')
        .filter_map(|pair| {
            let (x, y) = pair.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        })
        .collect()
}

/// `(x, y)` of a capture (rows top to bottom, no padding) as RRGGBBAA; None outside it.
fn pixel_rgba(rgba: &[u8], width: u32, height: u32, x: u32, y: u32) -> Option<String> {
    if x >= width || y >= height {
        return None;
    }
    let i = (y as usize * width as usize + x as usize) * 4;
    let [r, g, b, a]: [u8; 4] = rgba.get(i..i + 4)?.try_into().ok()?;
    Some(format!("{r:02x}{g:02x}{b:02x}{a:02x}"))
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
    let (width, height) = (capture.size.width, capture.size.height);
    for (x, y) in pixel_points() {
        match pixel_rgba(&capture.rgba, width, height, x, y) {
            Some(rgba) => tracing::info!(
                "atl-composer: ui: capture pixel presented={presented} x={x} y={y} rgba={rgba}"
            ),
            None => tracing::warn!(
                "atl-composer: ui: capture pixel x={x} y={y} is outside the {width}x{height} capture"
            ),
        }
    }
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
        let mailbox = Arc::clone(&session.mailbox);
        let composer = Arc::clone(&session.composer);
        let capture = std::env::var_os(CAPTURE_ENV).is_some();
        let (mut tick_count, mut presented, mut quiet_ticks) = (0u32, 0u32, 0u32);
        // ticks that found no scene and were not the opening or a settling tick: the clock is asked only by a publish, so this stays near 0 (two publishes between ticks leave one empty tick)
        let mut unwanted = 0u32;
        let requester = window.frame_requester();
        session
            .composer
            .set_frame_requester(Box::new(move || requester.request_frame()));
        window.on_tick(move |tick| {
            let time_ns = tick.time_ns;
            let settling = capture && presented > 0 && quiet_ticks < SETTLED_TICKS;
            quiet_ticks = quiet_ticks.saturating_add(1);
            match mailbox.take() {
                Some(taken) => {
                    tracing::debug!(
                        "atl-composer: ui: present n={} time_ns={time_ns}",
                        presented + 1
                    );
                    match shown.present(&taken.scene) {
                        Ok(()) => {
                            presented += 1;
                            quiet_ticks = 0;
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
                None if tick_count > 0 && !settling => unwanted += 1,
                None => {}
            }
            if tick_count == 0 {
                tracing::info!("atl-composer: ui: first frame mapped={}", shown.is_mapped());
            }
            if capture && presented > 0 && quiet_ticks == SETTLED_TICKS {
                capture_report(&shown, presented);
            }
            tick_count += 1;
            if capture && presented > 0 && quiet_ticks < SETTLED_TICKS {
                shown.request_frame();
            } else if capture && quiet_ticks == SETTLED_TICKS {
                // gates only: the readback settled, the clock waits for the next publish
                tracing::info!(
                    "atl-composer: ui: clock stopped ticks={tick_count} unwanted={unwanted}"
                );
            }
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
