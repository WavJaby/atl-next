// SPDX-License-Identifier: MIT
//! The cdylib's exports (launcher-ui-module plan S2, `atl_platform_ui_api::abi`): a process loads `libatl_platform_ui_linux.so` with `dlopen(RTLD_NOW | RTLD_LOCAL)` and reaches GTK and PipeWire only through [`VTABLE`], so their closure never enters its global symbol scope. Hand-written shims (owner O1): one per trait method, each turning the handle and views into the implementation's types and calling it.
//! Handles are `Box`es of the implementation types, made and freed here. No state in a thread-local of this module, so the process's thread-local teardown never reaches a module object it did not drop itself.
//! This copy of `tracing` has none of the process's subscribers: `set_log` installs one that formats each record and hands it to the process's [`LogSink`].

use std::io;
use std::path::Path;
use std::sync::OnceLock;

use atl_platform_ui_api::abi::{
    self, AudioHandle, BytesOut, Callback, FillCallback, InvokerHandle, LogSink, LooperAbi,
    MonitorView, PaintedView, PcmHandle, RequesterHandle, SceneView, Status, Str, Task,
    TextInputEventView, TextInputHandle, UiHandle, UiVtable, WindowAttributesView, WindowHandle,
};
use atl_platform_ui_api::audio::{AudioHost, PcmOutput, PcmSpec};
use atl_platform_ui_api::{
    ContentHints, ContentPurpose, CursorShape, DeviceNumber, FrameClock, FrameRequester, FrameTick,
    InputEvent, InputSource, PhysRect, PhysSize, PresentError, PresentTiming, Surrounding,
    TextInput, Ui, UiInvoker, Window,
};
use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

use crate::ui::GtkInvoker;
use crate::window::GtkFrameRequester;
use crate::{GtkTextInput, GtkUi, GtkWindow, PipeWireHost, PipeWireOutput};

/// The module's functions; [`atl_platform_ui_entry`] returns it.
static VTABLE: UiVtable = UiVtable {
    abi: abi::ABI_ID,
    set_log,
    ui_attach,
    ui_drop,
    ui_invoker,
    invoker_invoke,
    invoker_drop,
    window_create,
    window_drop,
    window_text_input,
    window_set_size,
    window_set_title,
    window_set_cursor,
    window_minimize,
    window_size,
    window_scale,
    window_is_mapped,
    window_is_size_fixed,
    window_present,
    window_on_painted,
    window_capture,
    window_on_resize,
    window_on_monitor,
    window_request_frame,
    window_frame_requester,
    requester_request,
    requester_drop,
    window_on_tick,
    window_on_presented,
    window_on_events,
    text_focus_in,
    text_focus_out,
    text_reset,
    text_set_content,
    text_set_surrounding,
    text_set_cursor_rect,
    text_on_events,
    audio_connect,
    audio_drop,
    audio_open_output,
    pcm_drop,
    pcm_spec,
    pcm_quantum,
    pcm_set_active,
    pcm_rt_fallbacks,
    pcm_underruns,
};

/// [`abi::Entry`]: the static vtable; starts nothing.
#[unsafe(no_mangle)]
pub extern "C" fn atl_platform_ui_entry() -> *const UiVtable {
    &VTABLE
}

/// A handle as the implementation object it boxes.
///
/// # Safety
/// `h` came from `Box::into_raw` of a `T` here and has not been dropped.
unsafe fn obj<'a, H, T>(h: *mut H) -> &'a T {
    // SAFETY: the caller's contract.
    unsafe { &*h.cast::<T>() }
}

fn into_handle<T, H>(t: T) -> *mut H {
    Box::into_raw(Box::new(t)).cast()
}

/// # Safety
/// As [`obj`]; the handle is not used again.
unsafe fn drop_handle<H, T>(h: *mut H) {
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(h.cast::<T>()) });
}

/// Writes `text` to the call's error sink and returns `kind`.
///
/// # Safety
/// `err` is the sink the caller lent for this call (or null).
unsafe fn fail(err: *mut BytesOut, (kind, text): (Status, String)) -> Status {
    // SAFETY: the caller's contract.
    unsafe { BytesOut::put_to(err, text.as_bytes()) };
    kind
}

static LOG: OnceLock<LogSink> = OnceLock::new();

unsafe extern "C" fn set_log(sink: LogSink) {
    if LOG.set(sink).is_err() {
        return;
    }
    // a subscriber installed by someone else in this module copy keeps its place: try_init's error is that and nothing else
    let _ = tracing_subscriber::fmt()
        .with_writer(LogWriter)
        .with_ansi(false)
        .without_time()
        .with_level(false)
        .with_target(false)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
}

unsafe extern "C" fn ui_attach(
    looper: LooperAbi,
    device: u32,
    has_device: bool,
    out: *mut *mut UiHandle,
    err: *mut BytesOut,
) -> Status {
    let device = has_device.then_some(DeviceNumber(device));
    match GtkUi::attach(Box::new(looper), device) {
        Ok(ui) => {
            tracing::info!(target: "atl-platform-ui", "atl-platform-ui: module attached: GTK {}.{}.{} on the process's Looper", gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version());
            // SAFETY: `out` is the caller's out-parameter for this call.
            unsafe { *out = into_handle(ui) };
            abi::OK
        }
        // SAFETY: `err` is the caller's sink.
        Err(e) => unsafe { fail(err, abi::ui_error_parts(&e)) },
    }
}

unsafe extern "C" fn ui_drop(ui: *mut UiHandle) {
    // SAFETY: a handle `ui_attach` made, dropped once by its proxy.
    unsafe { drop_handle::<_, GtkUi>(ui) }
}

unsafe extern "C" fn ui_invoker(ui: *mut UiHandle) -> *mut InvokerHandle {
    // SAFETY: a live handle of `ui_attach`.
    into_handle(unsafe { obj::<_, GtkUi>(ui) }.invoker())
}

unsafe extern "C" fn invoker_invoke(invoker: *mut InvokerHandle, task: Task) {
    // SAFETY: a live handle of `ui_invoker`; GtkInvoker is Send + Sync, so any thread may call.
    unsafe { obj::<_, GtkInvoker>(invoker) }.invoke(Box::new(move || task.run()));
}

unsafe extern "C" fn invoker_drop(invoker: *mut InvokerHandle) {
    // SAFETY: a handle of `ui_invoker`, dropped once.
    unsafe { drop_handle::<_, GtkInvoker>(invoker) }
}

unsafe extern "C" fn window_create(
    ui: *mut UiHandle,
    attributes: *const WindowAttributesView,
    out: *mut *mut WindowHandle,
    err: *mut BytesOut,
) -> Status {
    // SAFETY: a live session handle; `attributes` borrowed for this call.
    let (ui, attributes) = unsafe { (obj::<_, GtkUi>(ui), (*attributes).get()) };
    match GtkWindow::create(ui, &attributes) {
        Ok(w) => {
            // SAFETY: the caller's out-parameter.
            unsafe { *out = into_handle(w) };
            abi::OK
        }
        // SAFETY: the caller's sink.
        Err(e) => unsafe { fail(err, abi::ui_error_parts(&e)) },
    }
}

/// # Safety
/// A live handle of `window_create`.
unsafe fn win<'a>(w: *mut WindowHandle) -> &'a GtkWindow {
    // SAFETY: the caller's contract.
    unsafe { obj(w) }
}

unsafe extern "C" fn window_drop(w: *mut WindowHandle) {
    // SAFETY: a handle of `window_create`, dropped once (its text input handle dies with it).
    unsafe { drop_handle::<_, GtkWindow>(w) }
}

unsafe extern "C" fn window_text_input(w: *mut WindowHandle) -> *mut TextInputHandle {
    // SAFETY: a live window; the text input is a field of the boxed window, so the pointer lives as long as the window.
    std::ptr::from_ref(unsafe { win(w) }.text_input())
        .cast_mut()
        .cast()
}

unsafe extern "C" fn window_set_size(w: *mut WindowHandle, size: PhysSize) {
    // SAFETY: a live window.
    unsafe { win(w) }.set_size(size)
}

unsafe extern "C" fn window_set_title(w: *mut WindowHandle, title: Str) {
    // SAFETY: a live window; `title` borrowed for this call.
    unsafe { win(w).set_title(&title.get()) }
}

unsafe extern "C" fn window_set_cursor(w: *mut WindowHandle, shape: CursorShape) {
    // SAFETY: a live window.
    unsafe { win(w) }.set_cursor(shape)
}

unsafe extern "C" fn window_minimize(w: *mut WindowHandle) {
    // SAFETY: a live window.
    unsafe { win(w) }.minimize()
}

unsafe extern "C" fn window_size(w: *mut WindowHandle) -> PhysSize {
    // SAFETY: a live window.
    unsafe { win(w) }.size()
}

unsafe extern "C" fn window_scale(w: *mut WindowHandle) -> f64 {
    // SAFETY: a live window.
    unsafe { win(w) }.scale().get()
}

unsafe extern "C" fn window_is_mapped(w: *mut WindowHandle) -> bool {
    // SAFETY: a live window.
    unsafe { win(w) }.is_mapped()
}

unsafe extern "C" fn window_is_size_fixed(w: *mut WindowHandle) -> bool {
    // SAFETY: a live window.
    unsafe { win(w) }.is_size_fixed()
}

unsafe extern "C" fn window_present(
    w: *mut WindowHandle,
    scene: *const SceneView,
    layer_id: *mut i32,
    err: *mut BytesOut,
) -> Status {
    // SAFETY: a live window; the scene view and its arrays are borrowed for this call.
    let (w, scene) = unsafe { (win(w), (*scene).get()) };
    match w.present(&scene) {
        Ok(()) => abi::OK,
        Err(PresentError::Buffer {
            layer_id: id,
            reason,
        }) => {
            // SAFETY: the caller's out-parameter and sink.
            unsafe {
                *layer_id = id;
                fail(err, (1, reason))
            }
        }
    }
}

unsafe extern "C" fn window_on_painted(w: *mut WindowHandle, mut f: Callback<PaintedView>) {
    let mut storage = Vec::new();
    // SAFETY: a live window.
    unsafe { win(w) }.on_painted(move |p| {
        let view = PaintedView::of(p, &mut storage);
        f.call(&view);
    })
}

unsafe extern "C" fn window_capture(
    w: *mut WindowHandle,
    size: *mut PhysSize,
    pixels: *mut BytesOut,
    err: *mut BytesOut,
) -> Status {
    // SAFETY: a live window.
    match unsafe { win(w) }.capture() {
        Ok(c) => {
            // SAFETY: the caller's out-parameter and sink.
            unsafe {
                *size = c.size;
                BytesOut::put_to(pixels, &c.rgba);
            }
            abi::OK
        }
        // SAFETY: the caller's sink.
        Err(e) => unsafe { fail(err, abi::ui_error_parts(&e)) },
    }
}

unsafe extern "C" fn window_on_resize(w: *mut WindowHandle, mut f: Callback<PhysSize>) {
    // SAFETY: a live window.
    unsafe { win(w) }.on_resize(move |s| f.call(&s))
}

unsafe extern "C" fn window_on_monitor(w: *mut WindowHandle, mut f: Callback<MonitorView>) {
    // SAFETY: a live window.
    unsafe { win(w) }.on_monitor(move |m| f.call(&MonitorView::of(&m)))
}

unsafe extern "C" fn window_request_frame(w: *mut WindowHandle) {
    // SAFETY: a live window.
    unsafe { win(w) }.request_frame()
}

unsafe extern "C" fn window_frame_requester(w: *mut WindowHandle) -> *mut RequesterHandle {
    // SAFETY: a live window.
    into_handle(unsafe { win(w) }.frame_requester())
}

unsafe extern "C" fn requester_request(r: *mut RequesterHandle) {
    // SAFETY: a live handle of `window_frame_requester`; the requester is Send + Sync, so any thread may call.
    unsafe { obj::<_, GtkFrameRequester>(r) }.request_frame()
}

unsafe extern "C" fn requester_drop(r: *mut RequesterHandle) {
    // SAFETY: a handle of `window_frame_requester`, dropped once.
    unsafe { drop_handle::<_, GtkFrameRequester>(r) }
}

unsafe extern "C" fn window_on_tick(w: *mut WindowHandle, mut f: Callback<FrameTick>) {
    // SAFETY: a live window.
    unsafe { win(w) }.on_tick(move |t| f.call(&t))
}

unsafe extern "C" fn window_on_presented(w: *mut WindowHandle, mut f: Callback<PresentTiming>) {
    // SAFETY: a live window.
    unsafe { win(w) }.on_presented(move |t| f.call(&t))
}

unsafe extern "C" fn window_on_events(w: *mut WindowHandle, mut f: Callback<InputEvent>) {
    // SAFETY: a live window.
    unsafe { win(w) }.on_events(move |e| f.call(&e))
}

/// # Safety
/// A live handle of `window_text_input` (its window alive).
unsafe fn text<'a>(t: *mut TextInputHandle) -> &'a GtkTextInput {
    // SAFETY: the caller's contract.
    unsafe { &*t.cast::<GtkTextInput>() }
}

unsafe extern "C" fn text_focus_in(t: *mut TextInputHandle) {
    // SAFETY: a live text input.
    unsafe { text(t) }.focus_in()
}

unsafe extern "C" fn text_focus_out(t: *mut TextInputHandle) {
    // SAFETY: a live text input.
    unsafe { text(t) }.focus_out()
}

unsafe extern "C" fn text_reset(t: *mut TextInputHandle) {
    // SAFETY: a live text input.
    unsafe { text(t) }.reset()
}

unsafe extern "C" fn text_set_content(
    t: *mut TextInputHandle,
    purpose: ContentPurpose,
    hints: ContentHints,
) {
    // SAFETY: a live text input.
    unsafe { text(t) }.set_content(purpose, hints)
}

unsafe extern "C" fn text_set_surrounding(
    t: *mut TextInputHandle,
    surrounding: Str,
    cursor: usize,
    anchor: usize,
) {
    // SAFETY: a live text input; the text borrowed for this call.
    let (t, s) = unsafe { (text(t), surrounding.get().into_owned()) };
    t.set_surrounding(&Surrounding {
        text: s,
        cursor_byte: cursor,
        anchor_byte: anchor,
    })
}

unsafe extern "C" fn text_set_cursor_rect(t: *mut TextInputHandle, rect: PhysRect) {
    // SAFETY: a live text input.
    unsafe { text(t) }.set_cursor_rect(rect)
}

unsafe extern "C" fn text_on_events(t: *mut TextInputHandle, mut f: Callback<TextInputEventView>) {
    // SAFETY: a live text input.
    unsafe { text(t) }.on_events(move |e| f.call(&TextInputEventView::of(&e)))
}

unsafe extern "C" fn audio_connect(
    remote: Str,
    out: *mut *mut AudioHandle,
    err: *mut BytesOut,
) -> Status {
    // SAFETY: borrowed for this call.
    let remote = unsafe { remote.get() }.into_owned();
    match PipeWireHost::connect(Path::new(&remote)) {
        Ok(h) => {
            // SAFETY: the caller's out-parameter.
            unsafe { *out = into_handle(h) };
            abi::OK
        }
        // SAFETY: the caller's sink.
        Err(e) => unsafe { fail(err, abi::audio_error_parts(&e)) },
    }
}

unsafe extern "C" fn audio_drop(a: *mut AudioHandle) {
    // SAFETY: a handle of `audio_connect`, dropped once.
    unsafe { drop_handle::<_, PipeWireHost>(a) }
}

unsafe extern "C" fn audio_open_output(
    a: *mut AudioHandle,
    want: PcmSpec,
    mut fill: FillCallback,
    out: *mut *mut PcmHandle,
    err: *mut BytesOut,
) -> Status {
    // SAFETY: a live connection.
    match unsafe { obj::<_, PipeWireHost>(a) }.open_output(want, move |buf| fill.fill(buf)) {
        Ok(o) => {
            // SAFETY: the caller's out-parameter.
            unsafe { *out = into_handle(o) };
            abi::OK
        }
        // SAFETY: the caller's sink.
        Err(e) => unsafe { fail(err, abi::audio_error_parts(&e)) },
    }
}

/// # Safety
/// A live handle of `audio_open_output`.
unsafe fn pcm<'a>(p: *mut PcmHandle) -> &'a PipeWireOutput {
    // SAFETY: the caller's contract.
    unsafe { obj(p) }
}

unsafe extern "C" fn pcm_drop(p: *mut PcmHandle) {
    // SAFETY: a handle of `audio_open_output`, dropped once.
    unsafe { drop_handle::<_, PipeWireOutput>(p) }
}

unsafe extern "C" fn pcm_spec(p: *mut PcmHandle) -> PcmSpec {
    // SAFETY: a live output.
    unsafe { pcm(p) }.spec()
}

unsafe extern "C" fn pcm_quantum(p: *mut PcmHandle) -> u32 {
    // SAFETY: a live output.
    unsafe { pcm(p) }.quantum()
}

unsafe extern "C" fn pcm_set_active(p: *mut PcmHandle, on: bool, err: *mut BytesOut) -> Status {
    // SAFETY: a live output.
    match unsafe { pcm(p) }.set_active(on) {
        Ok(()) => abi::OK,
        // SAFETY: the caller's sink.
        Err(e) => unsafe { fail(err, abi::audio_error_parts(&e)) },
    }
}

unsafe extern "C" fn pcm_rt_fallbacks(p: *mut PcmHandle) -> u64 {
    // SAFETY: a live output.
    unsafe { pcm(p) }.rt_fallbacks()
}

unsafe extern "C" fn pcm_underruns(p: *mut PcmHandle) -> u64 {
    // SAFETY: a live output.
    unsafe { pcm(p) }.underruns()
}

/// The subscriber's writer factory: one [`LogLine`] per record, carrying its level and target.
struct LogWriter;

/// One formatted record, handed to the process's sink when the formatter drops it.
struct LogLine {
    level: abi::Level,
    target: String,
    buf: Vec<u8>,
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogLine;

    fn make_writer(&'a self) -> LogLine {
        LogLine {
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
        let Some(sink) = LOG.get() else { return };
        let msg = self.buf.strip_suffix(b"\n").unwrap_or(&self.buf);
        // SAFETY: the LogSink contract (process-lifetime, any thread); both slices are valid for the call.
        unsafe {
            (sink.log)(
                sink.data,
                self.level,
                self.target.as_ptr(),
                self.target.len(),
                msg.as_ptr(),
                msg.len(),
            )
        }
    }
}

/// The handles that cross threads box Send + Sync objects, as the traits require.
#[allow(dead_code)]
fn crossing_threads() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<GtkInvoker>();
    send_sync::<GtkFrameRequester>();
}
