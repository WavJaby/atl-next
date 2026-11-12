// SPDX-License-Identifier: MIT
//! The cdylib entry (launcher-ui-module plan slice M1, `atl_platform_ui_api::abi`): a process loads `libatl_platform_ui_linux.so` with `dlopen(RTLD_NOW | RTLD_LOCAL)` and reaches GTK only through these C functions, so GTK, GLib, PipeWire and their closure never enter its global symbol scope.
//! M1 surface only (one colour window, painted count, audio connect); slice M3 replaces it with the full vtable over every trait.
//! This copy of `tracing` has no subscriber of the process's: [`entry`] installs one that formats each record and hands it to `Host::log`.
//! Everything runs on the thread that called the entry (the UI thread); state lives in a thread-local, so a call from another thread finds none and fails.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::io;
use std::os::fd::RawFd;
use std::path::Path;
use std::rc::Rc;

use atl_compositor::scene::{Content, Quad, Scene};
use atl_libgui_parcel::state::FloatRect;
use atl_platform_ui_api::abi::{self, FdSink, Host, Module};
use atl_platform_ui_api::audio::AudioHost;
use atl_platform_ui_api::{
    FdEvents, FrameClock, Looper, PhysSize, Ui, UiError, Window, WindowAttributes,
};
use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

use crate::{GtkUi, GtkWindow, PipeWireHost};

/// The process's callbacks; the entry's caller guarantees it outlives the process.
#[derive(Clone, Copy)]
struct HostRef(&'static Host);

// SAFETY: `Host` is plain data the process made for the whole process life; the log function is the process's subscriber entry, callable from any thread (a log record can come from PipeWire's thread).
unsafe impl Send for HostRef {}
// SAFETY: as Send: nothing in `Host` is mutated after the entry.
unsafe impl Sync for HostRef {}

struct State {
    ui: GtkUi,
    window: Option<Rc<GtkWindow>>,
    painted: Rc<Cell<u64>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

static MODULE: Module = Module {
    open_color_window,
    painted,
    audio_connect,
};

/// [`abi::Entry`].
///
/// # Safety
/// `host` points to a valid [`Host`] that lives until the process exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atl_platform_ui_entry(host: *const Host) -> *const Module {
    // SAFETY: the caller's contract (above).
    let Some(host) = (unsafe { host.as_ref() }) else {
        return std::ptr::null();
    };
    let host = HostRef(host);
    if host.0.log.is_some() {
        // a second entry in the process keeps the first subscriber: try_init's error is that and nothing else
        let _ = tracing_subscriber::fmt()
            .with_writer(LogSink(host))
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
    }
    match GtkUi::attach(Box::new(HostLooper(host)), None) {
        Ok(ui) => {
            tracing::info!(target: "atl-platform-ui", "module attached: GTK {}.{}.{} on the host's Looper", gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version());
            STATE.with_borrow_mut(|s| {
                *s = Some(State {
                    ui,
                    window: None,
                    painted: Rc::new(Cell::new(0)),
                })
            });
            &MODULE
        }
        Err(e) => {
            tracing::error!(target: "atl-platform-ui", "module attach: {e}");
            std::ptr::null()
        }
    }
}

unsafe extern "C" fn open_color_window(
    title: *const c_char,
    width: u32,
    height: u32,
    rgb: *const [f32; 3],
) -> c_int {
    if title.is_null() || rgb.is_null() {
        return -1;
    }
    // SAFETY: both non-null; the ABI contract makes `title` NUL-terminated and `rgb` three floats, both borrowed for this call.
    let (title, rgb) = (
        unsafe { CStr::from_ptr(title) }
            .to_string_lossy()
            .into_owned(),
        unsafe { *rgb },
    );
    STATE.with_borrow_mut(|s| {
        let Some(s) = s else {
            return -1;
        };
        let attributes = WindowAttributes {
            title,
            app_id: None,
            size: PhysSize { width, height },
        };
        let window = match GtkWindow::create(&s.ui, &attributes) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                tracing::error!(target: "atl-platform-ui", "module window: {e}");
                return -1;
            }
        };
        let scene = color_scene(width, height, rgb);
        let weak = Rc::downgrade(&window);
        let painted = s.painted.clone();
        window.on_tick(move |_| {
            let Some(w) = weak.upgrade() else { return };
            if let Err(e) = w.present(&scene) {
                tracing::error!(target: "atl-platform-ui", "module present: {e}");
            }
            if painted.get() == 0 {
                w.request_frame();
            }
        });
        let painted = s.painted.clone();
        window.on_painted(move |_| painted.set(painted.get() + 1));
        window.request_frame();
        s.window = Some(window);
        0
    })
}

unsafe extern "C" fn painted() -> u64 {
    STATE.with_borrow(|s| s.as_ref().map_or(0, |s| s.painted.get()))
}

unsafe extern "C" fn audio_connect(remote: *const c_char) -> c_int {
    if remote.is_null() {
        return -1;
    }
    // SAFETY: non-null, NUL-terminated by the ABI contract, borrowed for this call.
    let remote = unsafe { CStr::from_ptr(remote) }
        .to_string_lossy()
        .into_owned();
    match PipeWireHost::connect(Path::new(&remote)) {
        Ok(_) => {
            tracing::info!(target: "atl-platform-ui", "module audio: connected to {remote}");
            1
        }
        Err(e) => {
            tracing::info!(target: "atl-platform-ui", "module audio: {e}");
            0
        }
    }
}

/// One opaque colour quad covering `width` x `height`.
fn color_scene(width: u32, height: u32, rgb: [f32; 3]) -> Scene {
    let dst = FloatRect {
        left: 0.0,
        top: 0.0,
        right: width as f32,
        bottom: height as f32,
    };
    Scene {
        frame: 1,
        layer_stack: 0,
        quads: vec![Quad {
            layer_id: 0,
            name: "color".into(),
            content: Content::Color(rgb),
            dst,
            clip: Some(dst),
            alpha: 1.0,
        }],
    }
}

/// The process's Looper behind [`Host`]'s callbacks.
struct HostLooper(HostRef);

fn bits(e: FdEvents) -> u32 {
    (if e.input { abi::FD_INPUT } else { 0 })
        | (if e.output { abi::FD_OUTPUT } else { 0 })
        | (if e.error { abi::FD_ERROR } else { 0 })
        | (if e.hangup { abi::FD_HANGUP } else { 0 })
}

fn events(b: u32) -> FdEvents {
    FdEvents {
        input: b & abi::FD_INPUT != 0,
        output: b & abi::FD_OUTPUT != 0,
        error: b & abi::FD_ERROR != 0,
        hangup: b & abi::FD_HANGUP != 0,
    }
}

type SinkFn = Box<dyn FnMut(RawFd, FdEvents)>;

unsafe extern "C" fn sink_call(data: *mut c_void, fd: c_int, ev: u32) {
    // SAFETY: `data` is the Box<SinkFn> `set_sink` leaked, alive until `sink_drop`; the Looper calls on the UI thread only.
    let sink = unsafe { &mut *data.cast::<SinkFn>() };
    sink(fd, events(ev));
}

unsafe extern "C" fn sink_drop(data: *mut c_void) {
    // SAFETY: `data` came from Box::into_raw in `set_sink`, and the Looper drops it once.
    drop(unsafe { Box::from_raw(data.cast::<SinkFn>()) });
}

fn errno(rc: c_int, what: &str) -> Result<(), UiError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(UiError::Unavailable(format!(
            "host looper {what}: {}",
            io::Error::from_raw_os_error(rc)
        )))
    }
}

impl Looper for HostLooper {
    fn set_sink(&self, sink: SinkFn) {
        let data = Box::into_raw(Box::new(sink)).cast::<c_void>();
        let sink = FdSink {
            data,
            call: sink_call,
            drop: sink_drop,
        };
        // SAFETY: the Host contract; ownership of `sink` passes to the Looper.
        unsafe { (self.0.0.set_sink)(self.0.0.data, sink) }
    }

    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
        // SAFETY: the Host contract.
        errno(
            unsafe { (self.0.0.add_fd)(self.0.0.data, fd, bits(events)) },
            "add_fd",
        )
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        // SAFETY: the Host contract.
        errno(
            unsafe { (self.0.0.remove_fd)(self.0.0.data, fd) },
            "remove_fd",
        )
    }
}

/// The subscriber's writer factory: one [`LogLine`] per record, carrying its level and target.
struct LogSink(HostRef);

/// One formatted record, handed to `Host::log` when the formatter drops it.
struct LogLine {
    host: HostRef,
    level: abi::Level,
    target: String,
    buf: Vec<u8>,
}

impl<'a> MakeWriter<'a> for LogSink {
    type Writer = LogLine;

    fn make_writer(&'a self) -> LogLine {
        LogLine {
            host: self.0,
            level: 3,
            target: String::new(),
            buf: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> LogLine {
        let level = match *meta.level() {
            tracing::Level::ERROR => 1,
            tracing::Level::WARN => 2,
            tracing::Level::INFO => 3,
            tracing::Level::DEBUG => 4,
            tracing::Level::TRACE => 5,
        };
        LogLine {
            host: self.0,
            level,
            target: meta.target().to_owned(),
            buf: Vec::new(),
        }
    }
}

impl io::Write for LogLine {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogLine {
    fn drop(&mut self) {
        let Some(log) = self.host.0.log else { return };
        let msg = self.buf.strip_suffix(b"\n").unwrap_or(&self.buf);
        // SAFETY: the Host contract; both slices are valid for the call.
        unsafe {
            log(
                self.host.0.data,
                self.level,
                self.target.as_ptr().cast(),
                self.target.len(),
                msg.as_ptr().cast(),
                msg.len(),
            )
        }
    }
}
