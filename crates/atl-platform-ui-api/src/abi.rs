// SPDX-License-Identifier: MIT
//! The C ABI between a process and the platform module it loads with `dlopen(RTLD_NOW | RTLD_LOCAL)` (launcher-ui-module plan S2, slices M1 + M3): the module's toolkit and host libraries (GTK, GLib, PipeWire and their closure) stay out of the process's global symbol scope, so an app `.so` that needs `libvulkan.so` or `libz.so` binds AOSP's copy, not the host's.
//! Shape: the module exports one symbol, [`ENTRY_SYMBOL`] (an [`Entry`]), which returns its static [`UiVtable`]: one `extern "C"` fn per trait method of this crate over opaque handles. The facade (`atl-platform-ui`) wraps the vtable in proxies implementing the traits; the module wraps its implementation in the exports. Both sides compile these definitions, so no layout is copied by hand.
//! Identity, not versioning: [`UiVtable::abi`] is [`ABI_ID`], a hash of this crate's sources, atl-platform-types' and the rustc version (build.rs). The loader reads it before any other field and refuses a module of another build by name.
//! Ownership: a callback ([`Callback`], [`FillCallback`], [`Task`], [`LooperAbi`]) is made by `new` on the side whose closure it carries, and its `drop` runs that side's code: nothing is freed by the other artifact's std. Views ([`Str`], [`SceneView`], [`PaintedView`], ...) borrow for one call; the fences of a [`PaintedView`] pass to the receiver.
//! No unwind crosses: an `extern "C"` fn aborts on panic. Strings cross as views because `String`'s layout is Rust's, unspecified between two artifacts.

use std::borrow::Cow;
use std::ffi::{c_int, c_void};
use std::mem::ManuallyDrop;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use atl_compositor::scene::{BufferContent, Content, Quad, Scene, SceneBuffer, SceneFd};
use atl_libgui_parcel::state::FloatRect;

use crate::audio::AudioError;
use crate::{
    FdEvents, Looper, Monitor, Painted, PhysSize, QuadRelease, Rotation, Scale, TextInputEvent,
    UiError,
};

/// The module's file name, found by the dynamic loader's search path (the runtime library dir the process's `LD_LIBRARY_PATH` names, as for `libGLES_atl.so`).
pub const MODULE_FILE: &str = "libatl_platform_ui_linux.so";

/// The module's one exported symbol, an [`Entry`].
pub const ENTRY_SYMBOL: &str = "atl_platform_ui_entry";

/// The build identity both sides compile in: equal only for a module and a process of one build.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiId(pub u64);

impl std::fmt::Display for AbiId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// This build's identity (build.rs: FNV-1a 64 of the ABI's sources and `rustc -V`).
pub const ABI_ID: AbiId = AbiId(include!(concat!(env!("OUT_DIR"), "/abi_id.rs")));

/// The entry ([`ENTRY_SYMBOL`]): the module's static vtable. Reads nothing and starts nothing; the caller checks [`UiVtable::abi`] before any other field.
pub type Entry = unsafe extern "C" fn() -> *const UiVtable;

/// [`FdEvents`] as bits: readable.
pub const FD_INPUT: u32 = 1;
/// Writable.
pub const FD_OUTPUT: u32 = 2;
/// Error condition.
pub const FD_ERROR: u32 = 4;
/// Peer hung up.
pub const FD_HANGUP: u32 = 8;

/// [`FdEvents`] -> `FD_*` bits.
pub fn fd_bits(e: FdEvents) -> u32 {
    (if e.input { FD_INPUT } else { 0 })
        | (if e.output { FD_OUTPUT } else { 0 })
        | (if e.error { FD_ERROR } else { 0 })
        | (if e.hangup { FD_HANGUP } else { 0 })
}

/// `FD_*` bits -> [`FdEvents`].
pub fn fd_events(b: u32) -> FdEvents {
    FdEvents {
        input: b & FD_INPUT != 0,
        output: b & FD_OUTPUT != 0,
        error: b & FD_ERROR != 0,
        hangup: b & FD_HANGUP != 0,
    }
}

/// A log record's level, `tracing`'s order: 1 error .. 5 trace.
pub type Level = u8;

/// The process's log subscriber as the module calls it: `target` and the formatted message, UTF-8, not NUL-terminated. Lives for the process (the module keeps it); callable from any thread (a record can come from PipeWire's thread).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogSink {
    /// Passed back to `log`.
    pub data: *mut c_void,
    /// One record.
    pub log: unsafe extern "C" fn(
        data: *mut c_void,
        level: Level,
        target: *const u8,
        target_len: usize,
        message: *const u8,
        message_len: usize,
    ),
}

// SAFETY: the process makes the sink for its whole life and its `log` is callable from any thread (the contract above).
unsafe impl Send for LogSink {}
// SAFETY: as Send; nothing in it is mutated after it is made.
unsafe impl Sync for LogSink {}

/// A borrowed UTF-8 string for one call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Str {
    ptr: *const u8,
    len: usize,
}

impl Str {
    /// Borrows `s`; valid while `s` is.
    pub fn of(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// The string.
    ///
    /// # Safety
    /// The `&str` this view was made from is still alive.
    pub unsafe fn get(&self) -> Cow<'_, str> {
        // SAFETY: the caller's contract: ptr/len are a live `&str`'s.
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(self.ptr, self.len) })
    }
}

unsafe extern "C" fn call_box<T, F: FnMut(&T)>(data: *mut c_void, value: *const T) {
    // SAFETY: `data` is the Box<F> `Callback::new` leaked, alive until `drop_box`; `value` is the `&T` `Callback::call` was given, alive for this call.
    unsafe { (*data.cast::<F>())(&*value) }
}

unsafe extern "C" fn drop_box<F>(data: *mut c_void) {
    // SAFETY: `data` came from Box::into_raw of a Box<F> in this file's `new`s, and each owner drops it once.
    drop(unsafe { Box::from_raw(data.cast::<F>()) });
}

/// An `FnMut(&T)` closure across the boundary; called and dropped on the thread that holds it (the UI thread: never Send).
#[repr(C)]
pub struct Callback<T> {
    data: *mut c_void,
    call: unsafe extern "C" fn(*mut c_void, *const T),
    drop: unsafe extern "C" fn(*mut c_void),
}

impl<T> Callback<T> {
    /// Carries `f`; its code (and its drop) stay this side's.
    pub fn new<F: FnMut(&T) + 'static>(f: F) -> Self {
        Self {
            data: Box::into_raw(Box::new(f)).cast(),
            call: call_box::<T, F>,
            drop: drop_box::<F>,
        }
    }

    /// Runs the closure with `value`.
    pub fn call(&mut self, value: &T) {
        // SAFETY: `data` and `call` were paired by `new` and `data` lives until drop.
        unsafe { (self.call)(self.data, value) }
    }
}

impl<T> Drop for Callback<T> {
    fn drop(&mut self) {
        // SAFETY: paired by `new`; Drop runs once.
        unsafe { (self.drop)(self.data) }
    }
}

unsafe extern "C" fn fill_box<F: FnMut(&mut [f32])>(data: *mut c_void, ptr: *mut f32, len: usize) {
    // SAFETY: `data` is the Box<F> `FillCallback::new` leaked; ptr/len are the `&mut [f32]` `fill` was given, alive and unaliased for this call.
    unsafe { (*data.cast::<F>())(std::slice::from_raw_parts_mut(ptr, len)) }
}

/// The PCM output's `fill`, called on the audio server's realtime thread (Send).
#[repr(C)]
pub struct FillCallback {
    data: *mut c_void,
    call: unsafe extern "C" fn(*mut c_void, *mut f32, usize),
    drop: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: `new` takes only Send closures.
unsafe impl Send for FillCallback {}

impl FillCallback {
    /// Carries `f`.
    pub fn new<F: FnMut(&mut [f32]) + Send + 'static>(f: F) -> Self {
        Self {
            data: Box::into_raw(Box::new(f)).cast(),
            call: fill_box::<F>,
            drop: drop_box::<F>,
        }
    }

    /// Fills `buf`.
    pub fn fill(&mut self, buf: &mut [f32]) {
        // SAFETY: paired by `new`; `buf` is valid for the call.
        unsafe { (self.call)(self.data, buf.as_mut_ptr(), buf.len()) }
    }
}

impl Drop for FillCallback {
    fn drop(&mut self) {
        // SAFETY: paired by `new`; Drop runs once.
        unsafe { (self.drop)(self.data) }
    }
}

unsafe extern "C" fn run_box<F: FnOnce()>(data: *mut c_void) {
    // SAFETY: `data` is the Box<F> `Task::new` leaked; `run` consumes it once, and the Task's Drop does not run after it.
    (unsafe { Box::from_raw(data.cast::<F>()) })()
}

/// An `FnOnce() + Send` closure ([`UiInvoker::invoke`](crate::UiInvoker::invoke)): run once, or dropped unrun.
#[repr(C)]
pub struct Task {
    data: *mut c_void,
    run: unsafe extern "C" fn(*mut c_void),
    drop: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: `new` takes only Send closures.
unsafe impl Send for Task {}

impl Task {
    /// Carries `f`.
    pub fn new<F: FnOnce() + Send + 'static>(f: F) -> Self {
        Self {
            data: Box::into_raw(Box::new(f)).cast(),
            run: run_box::<F>,
            drop: drop_box::<F>,
        }
    }

    /// Runs the closure, consuming it.
    pub fn run(self) {
        let task = ManuallyDrop::new(self);
        // SAFETY: paired by `new`; `run` frees `data`, and ManuallyDrop keeps Drop from freeing it again.
        unsafe { (task.run)(task.data) }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // SAFETY: paired by `new`; reached only for a task never run.
        unsafe { (self.drop)(self.data) }
    }
}

unsafe extern "C" fn put_vec(data: *mut c_void, ptr: *const u8, len: usize) {
    // SAFETY: `data` is the Vec `BytesOut::collect` lent for its call; ptr/len are the slice `put` was given.
    unsafe { (*data.cast::<Vec<u8>>()).extend_from_slice(std::slice::from_raw_parts(ptr, len)) }
}

/// A byte sink for one call: error messages, captured pixels.
#[repr(C)]
pub struct BytesOut {
    data: *mut c_void,
    put: unsafe extern "C" fn(*mut c_void, *const u8, usize),
}

impl BytesOut {
    /// Runs `f` with a sink appending to a buffer; returns `f`'s result and the bytes.
    pub fn collect<R>(f: impl FnOnce(*mut BytesOut) -> R) -> (R, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut out = BytesOut {
            data: std::ptr::from_mut(&mut bytes).cast(),
            put: put_vec,
        };
        let r = f(&mut out);
        (r, bytes)
    }

    /// Appends `bytes`.
    pub fn put(&mut self, bytes: &[u8]) {
        // SAFETY: made by `collect`, whose buffer outlives the call this sink was lent for.
        unsafe { (self.put)(self.data, bytes.as_ptr(), bytes.len()) }
    }

    /// Appends `bytes` to the sink behind `out`, if any.
    ///
    /// # Safety
    /// `out` is null or the pointer a `collect` call lent for the running call.
    pub unsafe fn put_to(out: *mut BytesOut, bytes: &[u8]) {
        // SAFETY: the caller's contract.
        if let Some(out) = unsafe { out.as_mut() } {
            out.put(bytes);
        }
    }
}

/// One readiness report of a Looper fd.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FdReady {
    /// The fd.
    pub fd: c_int,
    /// `FD_*` bits.
    pub events: u32,
}

/// The process's [`Looper`] as the module holds it: owned by the module from `ui_attach` on, dropped (the process's `drop`) when the module's main loop mirror ends.
#[repr(C)]
pub struct LooperAbi {
    data: *mut c_void,
    set_sink: unsafe extern "C" fn(*mut c_void, Callback<FdReady>),
    add_fd: unsafe extern "C" fn(*mut c_void, c_int, u32, *mut BytesOut) -> c_int,
    remove_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut BytesOut) -> c_int,
    drop: unsafe extern "C" fn(*mut c_void),
}

type BoxLooper = Box<dyn Looper>;

unsafe extern "C" fn looper_set_sink(data: *mut c_void, mut sink: Callback<FdReady>) {
    // SAFETY: `data` is the Box<BoxLooper> `LooperAbi::new` leaked, alive until `drop`.
    let looper = unsafe { &*data.cast::<BoxLooper>() };
    looper.set_sink(Box::new(move |fd, events| {
        sink.call(&FdReady {
            fd,
            events: fd_bits(events),
        })
    }));
}

unsafe extern "C" fn looper_add_fd(
    data: *mut c_void,
    fd: c_int,
    events: u32,
    err: *mut BytesOut,
) -> c_int {
    // SAFETY: as `looper_set_sink`.
    let looper = unsafe { &*data.cast::<BoxLooper>() };
    match looper.add_fd(fd, fd_events(events)) {
        Ok(()) => 0,
        Err(e) => {
            // SAFETY: `err` is the sink the caller lent for this call.
            unsafe { BytesOut::put_to(err, e.to_string().as_bytes()) };
            -1
        }
    }
}

unsafe extern "C" fn looper_remove_fd(data: *mut c_void, fd: c_int, err: *mut BytesOut) -> c_int {
    // SAFETY: as `looper_set_sink`.
    let looper = unsafe { &*data.cast::<BoxLooper>() };
    match looper.remove_fd(fd) {
        Ok(()) => 0,
        Err(e) => {
            // SAFETY: `err` is the sink the caller lent for this call.
            unsafe { BytesOut::put_to(err, e.to_string().as_bytes()) };
            -1
        }
    }
}

impl LooperAbi {
    /// Carries `looper` (the process side).
    pub fn new(looper: Box<dyn Looper>) -> Self {
        Self {
            data: Box::into_raw(Box::new(looper)).cast(),
            set_sink: looper_set_sink,
            add_fd: looper_add_fd,
            remove_fd: looper_remove_fd,
            drop: drop_box::<BoxLooper>,
        }
    }
}

impl Looper for LooperAbi {
    fn set_sink(&self, mut sink: Box<dyn FnMut(RawFd, FdEvents)>) {
        let sink = Callback::new(move |r: &FdReady| sink(r.fd, fd_events(r.events)));
        // SAFETY: paired by `new`; the sink's ownership passes to the process's Looper.
        unsafe { (self.set_sink)(self.data, sink) }
    }

    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
        // SAFETY: paired by `new`; `err` lives for the call.
        let (rc, msg) =
            BytesOut::collect(|err| unsafe { (self.add_fd)(self.data, fd, fd_bits(events), err) });
        looper_result(rc, msg)
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        // SAFETY: as add_fd.
        let (rc, msg) = BytesOut::collect(|err| unsafe { (self.remove_fd)(self.data, fd, err) });
        looper_result(rc, msg)
    }
}

/// The process's Looper error, already formatted: carried as `Looper` with its text.
fn looper_result(rc: c_int, msg: Vec<u8>) -> Result<(), UiError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(UiError::Looper(String::from_utf8_lossy(&msg).into_owned()))
    }
}

impl Drop for LooperAbi {
    fn drop(&mut self) {
        // SAFETY: paired by `new`; Drop runs once.
        unsafe { (self.drop)(self.data) }
    }
}

/// A call's outcome: 0, or an error kind (the `*_error` fns) with its text in the call's [`BytesOut`].
pub type Status = u32;
/// Success.
pub const OK: Status = 0;

/// [`UiError`] -> its kind and text.
pub fn ui_error_parts(e: &UiError) -> (Status, String) {
    match e {
        UiError::Unavailable(s) => (1, s.clone()),
        UiError::WrongThread => (2, String::new()),
        UiError::Looper(s) => (3, s.clone()),
        UiError::Capture(s) => (4, s.clone()),
    }
}

/// Kind and text -> [`UiError`]; an unknown kind is `Unavailable`.
pub fn ui_error(kind: Status, text: Vec<u8>) -> UiError {
    let text = String::from_utf8_lossy(&text).into_owned();
    match kind {
        2 => UiError::WrongThread,
        3 => UiError::Looper(text),
        4 => UiError::Capture(text),
        _ => UiError::Unavailable(text),
    }
}

/// [`AudioError`] -> its kind and text.
pub fn audio_error_parts(e: &AudioError) -> (Status, String) {
    match e {
        AudioError::Unavailable(s) => (1, s.clone()),
        AudioError::Stream(s) => (2, s.clone()),
    }
}

/// Kind and text -> [`AudioError`].
pub fn audio_error(kind: Status, text: Vec<u8>) -> AudioError {
    let text = String::from_utf8_lossy(&text).into_owned();
    match kind {
        2 => AudioError::Stream(text),
        _ => AudioError::Unavailable(text),
    }
}

/// [`WindowAttributes`](crate::WindowAttributes) for one call.
#[repr(C)]
pub struct WindowAttributesView {
    title: Str,
    app_id: Str,
    has_app_id: bool,
    size: PhysSize,
}

impl WindowAttributesView {
    /// Borrows `a`.
    pub fn of(a: &crate::WindowAttributes) -> Self {
        Self {
            title: Str::of(&a.title),
            app_id: Str::of(a.app_id.as_deref().unwrap_or("")),
            has_app_id: a.app_id.is_some(),
            size: a.size,
        }
    }

    /// The attributes.
    ///
    /// # Safety
    /// The attributes this view was made from are still alive.
    pub unsafe fn get(&self) -> crate::WindowAttributes {
        crate::WindowAttributes {
            // SAFETY: the caller's contract.
            title: unsafe { self.title.get() }.into_owned(),
            // SAFETY: the caller's contract.
            app_id: self
                .has_app_id
                .then(|| unsafe { self.app_id.get() }.into_owned()),
            size: self.size,
        }
    }
}

/// [`Monitor`] (its `Option<f32>` has no C layout).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MonitorView {
    size: PhysSize,
    refresh_hz: f32,
    has_refresh: bool,
    scale: f64,
    rotation: Rotation,
}

impl MonitorView {
    /// Of `m`.
    pub fn of(m: &Monitor) -> Self {
        Self {
            size: m.size,
            refresh_hz: m.refresh_hz.unwrap_or(0.0),
            has_refresh: m.refresh_hz.is_some(),
            scale: m.scale.get(),
            rotation: m.rotation,
        }
    }

    /// The monitor.
    pub fn get(&self) -> Monitor {
        Monitor {
            size: self.size,
            refresh_hz: self.has_refresh.then_some(self.refresh_hz),
            // a view is made from a valid Scale, so `new` refuses nothing here
            scale: Scale::new(self.scale).unwrap_or(Scale::ONE),
            rotation: self.rotation,
        }
    }
}

/// [`TextInputEvent`] for one call.
#[repr(C)]
pub struct TextInputEventView {
    kind: u32,
    text: Str,
    cursor_chars: usize,
    offset_chars: i32,
    n_chars: u32,
}

impl TextInputEventView {
    /// Borrows `e`.
    pub fn of(e: &TextInputEvent) -> Self {
        let empty = Self {
            kind: 0,
            text: Str::of(""),
            cursor_chars: 0,
            offset_chars: 0,
            n_chars: 0,
        };
        match e {
            TextInputEvent::Preedit { text, cursor_chars } => Self {
                text: Str::of(text),
                cursor_chars: *cursor_chars,
                ..empty
            },
            TextInputEvent::PreeditEnd => Self { kind: 1, ..empty },
            TextInputEvent::Commit(text) => Self {
                kind: 2,
                text: Str::of(text),
                ..empty
            },
            TextInputEvent::DeleteSurrounding {
                offset_chars,
                n_chars,
            } => Self {
                kind: 3,
                offset_chars: *offset_chars,
                n_chars: *n_chars,
                ..empty
            },
        }
    }

    /// The event.
    ///
    /// # Safety
    /// The event this view was made from is still alive.
    pub unsafe fn get(&self) -> TextInputEvent {
        match self.kind {
            0 => TextInputEvent::Preedit {
                // SAFETY: the caller's contract.
                text: unsafe { self.text.get() }.into_owned(),
                cursor_chars: self.cursor_chars,
            },
            1 => TextInputEvent::PreeditEnd,
            // SAFETY: the caller's contract.
            2 => TextInputEvent::Commit(unsafe { self.text.get() }.into_owned()),
            _ => TextInputEvent::DeleteSurrounding {
                offset_chars: self.offset_chars,
                n_chars: self.n_chars,
            },
        }
    }
}

/// [`Painted`] for one call: the release fences pass to the receiver.
#[repr(C)]
pub struct PaintedView {
    releases: *const ReleaseView,
    len: usize,
}

/// One [`QuadRelease`]; `fence` -1 = none.
#[repr(C)]
pub struct ReleaseView {
    layer_id: i32,
    buffer_key: u64,
    fence: c_int,
}

impl PaintedView {
    /// Releases `p`'s fences into `storage` and views it; the receiver owns the fds after the call.
    pub fn of(p: Painted, storage: &mut Vec<ReleaseView>) -> Self {
        storage.clear();
        storage.extend(p.releases.into_iter().map(|r| ReleaseView {
            layer_id: r.layer_id,
            buffer_key: r.buffer_key,
            fence: r.release_fence.map_or(-1, IntoRawFd::into_raw_fd),
        }));
        Self {
            releases: storage.as_ptr(),
            len: storage.len(),
        }
    }

    /// Takes the paint, owning its fences.
    ///
    /// # Safety
    /// The storage this view was made from is alive, and this is the view's only `take`.
    pub unsafe fn take(&self) -> Painted {
        // SAFETY: the caller's contract.
        let releases = unsafe { std::slice::from_raw_parts(self.releases, self.len) };
        Painted {
            releases: releases
                .iter()
                .map(|r| QuadRelease {
                    layer_id: r.layer_id,
                    buffer_key: r.buffer_key,
                    // SAFETY: `of` released this fd to the receiver; this is its one take.
                    release_fence: (r.fence >= 0).then(|| unsafe { OwnedFd::from_raw_fd(r.fence) }),
                })
                .collect(),
        }
    }
}

/// [`FloatRect`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RectView {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl RectView {
    fn of(r: &FloatRect) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    fn get(self) -> FloatRect {
        FloatRect {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

/// A quad's buffer: geometry, the gralloc handle's fds and ints (borrowed), sampling. Zeroed for a colour quad.
#[repr(C)]
pub struct BufferView {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    usage: u64,
    id: u64,
    fds: *const i32,
    fds_len: usize,
    ints: *const i32,
    ints_len: usize,
    frame_number: u64,
    acquire_fence: c_int,
    src: RectView,
    opaque: bool,
    buffer_transform: u32,
}

/// One [`Quad`].
#[repr(C)]
pub struct QuadView {
    layer_id: i32,
    name: Str,
    is_buffer: bool,
    color: [f32; 3],
    buffer: BufferView,
    dst: RectView,
    has_clip: bool,
    clip: RectView,
    alpha: f32,
}

/// A borrowed flat [`Scene`] (plan S2): the scene's fds stay the caller's, as `Window::present` borrows them.
#[repr(C)]
pub struct SceneView {
    frame: u64,
    layer_stack: u32,
    quads: *const QuadView,
    len: usize,
}

impl SceneView {
    /// Views `scene`; `quads` and `fds` (each buffer's fd numbers, `SceneFd` being Rust layout) are the caller's scratch, reused per frame. Valid while `scene` and the scratch are.
    pub fn of(scene: &Scene, quads: &mut Vec<QuadView>, fds: &mut Vec<Vec<i32>>) -> Self {
        quads.clear();
        fds.resize_with(scene.quads.len(), Vec::new);
        for (q, fd_store) in scene.quads.iter().zip(fds.iter_mut()) {
            fd_store.clear();
            let zero = RectView::of(&q.dst);
            let (is_buffer, color, buffer) = match &q.content {
                Content::Color(rgb) => (
                    false,
                    *rgb,
                    BufferView {
                        width: 0,
                        height: 0,
                        stride: 0,
                        format: 0,
                        usage: 0,
                        id: 0,
                        fds: std::ptr::null(),
                        fds_len: 0,
                        ints: std::ptr::null(),
                        ints_len: 0,
                        frame_number: 0,
                        acquire_fence: -1,
                        src: zero,
                        opaque: false,
                        buffer_transform: 0,
                    },
                ),
                Content::Buffer(b) => {
                    fd_store.extend(b.buffer.fds.iter().map(|f| f.raw()));
                    (
                        true,
                        [0.0; 3],
                        BufferView {
                            width: b.buffer.width,
                            height: b.buffer.height,
                            stride: b.buffer.stride,
                            format: b.buffer.format,
                            usage: b.buffer.usage,
                            id: b.buffer.id,
                            fds: fd_store.as_ptr(),
                            fds_len: fd_store.len(),
                            ints: b.buffer.ints.as_ptr(),
                            ints_len: b.buffer.ints.len(),
                            frame_number: b.frame_number,
                            acquire_fence: b.acquire_fence.map_or(-1, SceneFd::raw),
                            src: RectView::of(&b.src),
                            opaque: b.opaque,
                            buffer_transform: b.buffer_transform,
                        },
                    )
                }
            };
            quads.push(QuadView {
                layer_id: q.layer_id,
                name: Str::of(&q.name),
                is_buffer,
                color,
                buffer,
                dst: RectView::of(&q.dst),
                has_clip: q.clip.is_some(),
                clip: q.clip.as_ref().map_or(zero, RectView::of),
                alpha: q.alpha,
            });
        }
        Self {
            frame: scene.frame,
            layer_stack: scene.layer_stack,
            quads: quads.as_ptr(),
            len: quads.len(),
        }
    }

    /// The scene (its fds borrowed, as the view's).
    ///
    /// # Safety
    /// The scene and scratch this view was made from are still alive.
    pub unsafe fn get(&self) -> Scene {
        // SAFETY: the caller's contract.
        let quads = unsafe { std::slice::from_raw_parts(self.quads, self.len) };
        Scene {
            frame: self.frame,
            layer_stack: self.layer_stack,
            quads: quads
                .iter()
                .map(|q| Quad {
                    layer_id: q.layer_id,
                    // SAFETY: the caller's contract.
                    name: unsafe { q.name.get() }.into_owned(),
                    content: if q.is_buffer {
                        let b = &q.buffer;
                        // SAFETY: the caller's contract: both arrays belong to the live scene.
                        let (fds, ints) = unsafe {
                            (
                                std::slice::from_raw_parts(b.fds, b.fds_len),
                                std::slice::from_raw_parts(b.ints, b.ints_len),
                            )
                        };
                        Content::Buffer(BufferContent {
                            buffer: Arc::new(SceneBuffer {
                                width: b.width,
                                height: b.height,
                                stride: b.stride,
                                format: b.format,
                                usage: b.usage,
                                id: b.id,
                                fds: fds.iter().map(|&f| SceneFd::new(f)).collect(),
                                ints: ints.to_vec(),
                            }),
                            frame_number: b.frame_number,
                            acquire_fence: (b.acquire_fence >= 0)
                                .then(|| SceneFd::new(b.acquire_fence)),
                            src: b.src.get(),
                            opaque: b.opaque,
                            buffer_transform: b.buffer_transform,
                        })
                    } else {
                        Content::Color(q.color)
                    },
                    dst: q.dst.get(),
                    clip: q.has_clip.then(|| q.clip.get()),
                    alpha: q.alpha,
                })
                .collect(),
        }
    }
}

/// The toolkit session (`Ui`), opaque.
#[repr(C)]
pub struct UiHandle {
    _opaque: [u8; 0],
}
/// A `UiInvoker`, opaque; Send + Sync.
#[repr(C)]
pub struct InvokerHandle {
    _opaque: [u8; 0],
}
/// A `Window`, opaque.
#[repr(C)]
pub struct WindowHandle {
    _opaque: [u8; 0],
}
/// A window's `TextInput`, opaque; borrowed from its window.
#[repr(C)]
pub struct TextInputHandle {
    _opaque: [u8; 0],
}
/// A `FrameRequester`, opaque; Send + Sync.
#[repr(C)]
pub struct RequesterHandle {
    _opaque: [u8; 0],
}
/// An `AudioHost`, opaque.
#[repr(C)]
pub struct AudioHandle {
    _opaque: [u8; 0],
}
/// A `PcmOutput`, opaque.
#[repr(C)]
pub struct PcmHandle {
    _opaque: [u8; 0],
}

/// Everything the module offers: one fn per trait method of this crate. A fn on a handle runs on the thread its trait allows (UI thread, except the invoker, the frame requester and the audio calls the traits mark Send); a fn that can fail returns a [`Status`] and writes the error text to its `err` sink. `*_drop` ends a handle; a `*mut` out-parameter receives a new handle on [`OK`].
#[repr(C)]
pub struct UiVtable {
    /// The module's [`ABI_ID`]; the only field read before the check.
    pub abi: AbiId,
    /// Installs the module's log forwarding into `sink` (once per process; later calls keep the first).
    pub set_log: unsafe extern "C" fn(sink: LogSink),
    /// `Ui::attach`; `device` counts when `has_device`.
    pub ui_attach: unsafe extern "C" fn(
        looper: LooperAbi,
        device: u32,
        has_device: bool,
        out: *mut *mut UiHandle,
        err: *mut BytesOut,
    ) -> Status,
    /// Ends a session.
    pub ui_drop: unsafe extern "C" fn(ui: *mut UiHandle),
    /// `Ui::invoker`.
    pub ui_invoker: unsafe extern "C" fn(ui: *mut UiHandle) -> *mut InvokerHandle,
    /// `UiInvoker::invoke`, any thread.
    pub invoker_invoke: unsafe extern "C" fn(invoker: *mut InvokerHandle, task: Task),
    /// Ends an invoker, any thread.
    pub invoker_drop: unsafe extern "C" fn(invoker: *mut InvokerHandle),
    /// `Window::create`.
    pub window_create: unsafe extern "C" fn(
        ui: *mut UiHandle,
        attributes: *const WindowAttributesView,
        out: *mut *mut WindowHandle,
        err: *mut BytesOut,
    ) -> Status,
    /// Ends a window.
    pub window_drop: unsafe extern "C" fn(window: *mut WindowHandle),
    /// `Window::text_input`: valid until the window's drop.
    pub window_text_input: unsafe extern "C" fn(window: *mut WindowHandle) -> *mut TextInputHandle,
    /// `Window::set_size`.
    pub window_set_size: unsafe extern "C" fn(window: *mut WindowHandle, size: PhysSize),
    /// `Window::set_title`.
    pub window_set_title: unsafe extern "C" fn(window: *mut WindowHandle, title: Str),
    /// `Window::set_cursor`.
    pub window_set_cursor:
        unsafe extern "C" fn(window: *mut WindowHandle, shape: crate::CursorShape),
    /// `Window::minimize`.
    pub window_minimize: unsafe extern "C" fn(window: *mut WindowHandle),
    /// `Window::size`.
    pub window_size: unsafe extern "C" fn(window: *mut WindowHandle) -> PhysSize,
    /// `Window::scale`, as the factor.
    pub window_scale: unsafe extern "C" fn(window: *mut WindowHandle) -> f64,
    /// `Window::is_mapped`.
    pub window_is_mapped: unsafe extern "C" fn(window: *mut WindowHandle) -> bool,
    /// `Window::is_size_fixed`.
    pub window_is_size_fixed: unsafe extern "C" fn(window: *mut WindowHandle) -> bool,
    /// `Window::present`; on refusal (nonzero) `layer_id` names the quad.
    pub window_present: unsafe extern "C" fn(
        window: *mut WindowHandle,
        scene: *const SceneView,
        layer_id: *mut i32,
        err: *mut BytesOut,
    ) -> Status,
    /// `Window::on_painted`.
    pub window_on_painted:
        unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<PaintedView>),
    /// `Window::capture`: RGBA into `pixels`, the extent into `size`.
    pub window_capture: unsafe extern "C" fn(
        window: *mut WindowHandle,
        size: *mut PhysSize,
        pixels: *mut BytesOut,
        err: *mut BytesOut,
    ) -> Status,
    /// `Window::on_resize`.
    pub window_on_resize: unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<PhysSize>),
    /// `Window::on_monitor`.
    pub window_on_monitor:
        unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<MonitorView>),
    /// `FrameClock::request_frame`.
    pub window_request_frame: unsafe extern "C" fn(window: *mut WindowHandle),
    /// `FrameClock::frame_requester`.
    pub window_frame_requester:
        unsafe extern "C" fn(window: *mut WindowHandle) -> *mut RequesterHandle,
    /// `FrameRequester::request_frame`, any thread.
    pub requester_request: unsafe extern "C" fn(requester: *mut RequesterHandle),
    /// Ends a requester, any thread.
    pub requester_drop: unsafe extern "C" fn(requester: *mut RequesterHandle),
    /// `FrameClock::on_tick`.
    pub window_on_tick:
        unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<crate::FrameTick>),
    /// `FrameClock::on_presented`.
    pub window_on_presented:
        unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<crate::PresentTiming>),
    /// `InputSource::on_events`.
    pub window_on_events:
        unsafe extern "C" fn(window: *mut WindowHandle, f: Callback<crate::InputEvent>),
    /// `TextInput::focus_in`.
    pub text_focus_in: unsafe extern "C" fn(text: *mut TextInputHandle),
    /// `TextInput::focus_out`.
    pub text_focus_out: unsafe extern "C" fn(text: *mut TextInputHandle),
    /// `TextInput::reset`.
    pub text_reset: unsafe extern "C" fn(text: *mut TextInputHandle),
    /// `TextInput::set_content`.
    pub text_set_content: unsafe extern "C" fn(
        text: *mut TextInputHandle,
        purpose: crate::ContentPurpose,
        hints: crate::ContentHints,
    ),
    /// `TextInput::set_surrounding` (byte indices).
    pub text_set_surrounding: unsafe extern "C" fn(
        text: *mut TextInputHandle,
        surrounding: Str,
        cursor: usize,
        anchor: usize,
    ),
    /// `TextInput::set_cursor_rect`.
    pub text_set_cursor_rect:
        unsafe extern "C" fn(text: *mut TextInputHandle, rect: crate::PhysRect),
    /// `TextInput::on_events`.
    pub text_on_events:
        unsafe extern "C" fn(text: *mut TextInputHandle, f: Callback<TextInputEventView>),
    /// `AudioHost::connect` to the socket `remote` (a path).
    pub audio_connect:
        unsafe extern "C" fn(remote: Str, out: *mut *mut AudioHandle, err: *mut BytesOut) -> Status,
    /// Ends a connection.
    pub audio_drop: unsafe extern "C" fn(audio: *mut AudioHandle),
    /// `AudioHost::open_output`.
    pub audio_open_output: unsafe extern "C" fn(
        audio: *mut AudioHandle,
        want: crate::audio::PcmSpec,
        fill: FillCallback,
        out: *mut *mut PcmHandle,
        err: *mut BytesOut,
    ) -> Status,
    /// Ends an output (stops the stream).
    pub pcm_drop: unsafe extern "C" fn(pcm: *mut PcmHandle),
    /// `PcmOutput::spec`.
    pub pcm_spec: unsafe extern "C" fn(pcm: *mut PcmHandle) -> crate::audio::PcmSpec,
    /// `PcmOutput::quantum`.
    pub pcm_quantum: unsafe extern "C" fn(pcm: *mut PcmHandle) -> u32,
    /// `PcmOutput::set_active`.
    pub pcm_set_active:
        unsafe extern "C" fn(pcm: *mut PcmHandle, on: bool, err: *mut BytesOut) -> Status,
    /// `PcmOutput::rt_fallbacks`.
    pub pcm_rt_fallbacks: unsafe extern "C" fn(pcm: *mut PcmHandle) -> u64,
    /// `PcmOutput::underruns`.
    pub pcm_underruns: unsafe extern "C" fn(pcm: *mut PcmHandle) -> u64,
}
