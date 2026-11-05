// SPDX-License-Identifier: MIT
//! UI mode's window (M3 slice 6): the process's one window, a GTK window of the UI thread that shows the compositor's display. Its frame clock ticks the compositor ([`UiFrames::tick`], no clock thread) and its pointer events reach the input thread ([`Message::Event`]).
//! GTK objects live on the UI thread only: [`install`] keeps the session in a thread-local on the thread that attached the toolkit (the launcher's startup) and opens the window there, before any client; no other thread touches GTK.
//! The window is the display: one per process, opened at install (the app's first traversal waits for a vsync, which this window's frame clock supplies).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use atl_platform_ui::{
    FrameClock, InputSource, PhysSize, PlatformUi, PlatformWindow, Window, WindowAttributes,
};

use crate::host::UiFrames;
use crate::input::Message;
use crate::sink::fnv1a64;

/// The frame whose readback the log names (`atl-composer: ui: capture ...`): the gate compares its digest with the fixture's expected pixels.
const CAPTURE_FRAME: u32 = 30;

/// The UI thread's session: the toolkit, the compositor's frame path, the input thread, the display's size and the window once it exists.
struct Session {
    ui: PlatformUi,
    frames: Arc<UiFrames>,
    input: Sender<Message>,
    size: PhysSize,
    window: Option<Rc<PlatformWindow>>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Keeps the session on the calling thread, which must be the UI thread that attached `ui`, and opens the window now. The window cannot wait for the first client: the app's first traversal waits for a vsync, and vsync comes from this window's frame clock.
pub fn install(ui: PlatformUi, frames: UiFrames, input: Sender<Message>, size: PhysSize) {
    SESSION.with(|cell| {
        *cell.borrow_mut() = Some(Session {
            ui,
            frames: Arc::new(frames),
            input,
            size,
            window: None,
        });
    });
    open();
}

/// The readback of the window at [`CAPTURE_FRAME`]: its digest and size, and the frames presented so far, for the gate
/// (`atl-composer: ui: capture`); the gate compares the digest with its own expectation.
fn capture_report(window: &PlatformWindow, presented: u32) {
    let capture = match window.capture() {
        Ok(capture) => capture,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "atl-composer: ui: capture: {e}");
            return;
        }
    };
    let _ = writeln!(
        std::io::stderr(),
        "atl-composer: ui: capture presented={presented} size={}x{} fnv1a64={:016x}",
        capture.size.width,
        capture.size.height,
        fnv1a64(&capture.rgba)
    );
}

/// Creates the window, once, on the UI thread.
fn open() {
    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return;
        };
        let _ = writeln!(std::io::stderr(), "atl-composer: ui: opening the window");
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
                let _ = writeln!(std::io::stderr(), "atl-composer: ui window: {e}");
                return;
            }
        };
        let input = session.input.clone();
        window.on_events(move |event| {
            // an error means the input thread is gone: the process is ending
            let _ = input.send(Message::Event(event));
        });
        let shown = Rc::clone(&window);
        let frames = Arc::clone(&session.frames);
        let (mut tick_count, mut presented) = (0u32, 0u32);
        window.on_tick(move |tick| {
            let time_ns = i64::try_from(tick.time_ns).unwrap_or(i64::MAX);
            frames.tick(time_ns, |scene| {
                presented += 1;
                let _ = writeln!(
                    std::io::stderr(),
                    "atl-composer: ui: present n={presented} time_ns={time_ns}"
                );
                if let Err(e) = shown.present(scene) {
                    let _ = writeln!(std::io::stderr(), "atl-composer: ui present: {e}");
                }
            });
            if tick_count == 0 {
                let _ = writeln!(
                    std::io::stderr(),
                    "atl-composer: ui: first frame mapped={}",
                    shown.is_mapped()
                );
            }
            if tick_count == CAPTURE_FRAME {
                capture_report(&shown, presented);
            }
            tick_count += 1;
            shown.request_frame();
        });
        window.request_frame();
        session.window = Some(window);
    });
}
