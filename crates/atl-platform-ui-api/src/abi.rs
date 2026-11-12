// SPDX-License-Identifier: MIT
//! The C ABI between a process and the platform module loaded with `dlopen(RTLD_NOW | RTLD_LOCAL)` (launcher-ui-module plan S2, slice M1): the module's toolkit and host libraries (GTK, GLib, PipeWire and their closure) then stay out of the process's global symbol scope, so an app `.so` that needs `libvulkan.so` or `libz.so` binds AOSP's copy, not the host's.
//! M1 scope only: one entry, the Looper seam as C callbacks, a log sink, one window presenting a colour scene, a painted count and the audio connect. The full vtable (every trait, `SceneView`, the ABI identity check) is slice M3.
//! Both sides compile these definitions, so no layout is copied by hand. Callbacks cross as `extern "C"` fns plus an opaque `data` pointer owned by the side that made it; no Rust unwind crosses (an `extern "C"` fn aborts on panic).

use std::ffi::{c_char, c_int, c_void};

/// The module's file name, found by the dynamic loader's search path (the runtime library dir the process's `LD_LIBRARY_PATH` names, as for `libGLES_atl.so`).
pub const MODULE_FILE: &str = "libatl_platform_ui_linux.so";

/// The module's one exported symbol, an [`Entry`].
pub const ENTRY_SYMBOL: &str = "atl_platform_ui_entry";

/// [`FdEvents`](crate::FdEvents) as bits: readable.
pub const FD_INPUT: u32 = 1;
/// Writable.
pub const FD_OUTPUT: u32 = 2;
/// Error condition.
pub const FD_ERROR: u32 = 4;
/// Peer hung up.
pub const FD_HANGUP: u32 = 8;

/// A log record's level, `tracing`'s order: 1 error .. 5 trace.
pub type Level = u8;

/// The process side the module calls back: its Looper and its log subscriber. Valid until the process exits (the module keeps the pointer).
#[repr(C)]
pub struct Host {
    /// Passed back as the first argument of every function here.
    pub data: *mut c_void,
    /// Receives each module log record (`target`, formatted message; UTF-8, not NUL-terminated). None: the module's records go nowhere.
    pub log: Option<
        unsafe extern "C" fn(
            data: *mut c_void,
            level: Level,
            target: *const c_char,
            target_len: usize,
            message: *const c_char,
            message_len: usize,
        ),
    >,
    /// [`Looper::set_sink`](crate::Looper::set_sink): the Looper owns `sink` from here on and calls `sink.drop` when it ends.
    pub set_sink: unsafe extern "C" fn(data: *mut c_void, sink: FdSink),
    /// [`Looper::add_fd`](crate::Looper::add_fd), events as `FD_*` bits; 0 or an errno.
    pub add_fd: unsafe extern "C" fn(data: *mut c_void, fd: c_int, events: u32) -> c_int,
    /// [`Looper::remove_fd`](crate::Looper::remove_fd); 0 or an errno.
    pub remove_fd: unsafe extern "C" fn(data: *mut c_void, fd: c_int) -> c_int,
}

/// The module's readiness callback, owned by the Looper after `set_sink`; `call` and `drop` run on the UI thread.
#[repr(C)]
pub struct FdSink {
    /// Module-owned state.
    pub data: *mut c_void,
    /// One readiness report, events as `FD_*` bits.
    pub call: unsafe extern "C" fn(data: *mut c_void, fd: c_int, events: u32),
    /// Frees `data`; called once, after the last `call`.
    pub drop: unsafe extern "C" fn(data: *mut c_void),
}

/// What the module offers (M1). Every function runs on the thread that called the entry (the UI thread). Errors: a negative return, the reason logged through [`Host::log`].
#[repr(C)]
pub struct Module {
    /// Opens one window of `width` x `height` physical pixels titled `title` (NUL-terminated) that presents a scene of one opaque colour quad `rgb` covering it at each frame-clock tick; 0 on success.
    pub open_color_window: unsafe extern "C" fn(
        title: *const c_char,
        width: u32,
        height: u32,
        rgb: *const [f32; 3],
    ) -> c_int,
    /// Paints of that window's scenes so far (`Window::on_painted`).
    pub painted: unsafe extern "C" fn() -> u64,
    /// `AudioHost::connect` to the audio server socket `remote` (NUL-terminated path): 1 connected, 0 refused (reason logged). The connection is dropped at once.
    pub audio_connect: unsafe extern "C" fn(remote: *const c_char) -> c_int,
}

/// The entry ([`ENTRY_SYMBOL`]): attaches the toolkit to `host`'s Looper on the calling thread, which becomes the UI thread, and returns the module's functions; null on failure (reason logged). Called once per process; `host` must outlive the process.
pub type Entry = unsafe extern "C" fn(host: *const Host) -> *const Module;
