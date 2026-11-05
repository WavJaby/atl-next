// SPDX-License-Identifier: MIT
//! The UI thread's Android Looper as the platform layer sees it ([`atl_platform_ui::Looper`]), over libandroid's `ALooper_addFd` / `ALooper_removeFd` ([`ffi::Looper`]).
//! One trampoline serves every registration: each `ALooper_addFd` gets the same callback and the same data pointer, and the trampoline hands the readiness to the sink, which the platform sets once. The sink cell is leaked with the Looper, so a removal's leftover call from the same poll batch still reads valid memory; the sink then ignores the fd.
//! Thread-bound: created and used on the UI thread only, the thread that prepared the Looper.

use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::os::fd::RawFd;

use atl_platform_ui::{FdEvents, Looper, UiError};

use crate::ffi;
use crate::generated::looper;

/// The readiness sink of this thread's Looper; leaked with the Looper (see module docs).
type Sink = RefCell<Option<Box<dyn FnMut(RawFd, FdEvents)>>>;

/// This thread's Looper as the platform's [`Looper`].
pub struct AndroidLooper {
    looper: ffi::Looper,
    sink: &'static Sink,
}

impl AndroidLooper {
    /// Wraps the thread's Looper, as `ffi::LooperApi::prepare_looper` returns it.
    pub fn new(looper: ffi::Looper) -> Self {
        AndroidLooper {
            looper,
            sink: Box::leak(Box::new(RefCell::new(None))),
        }
    }
}

impl Looper for AndroidLooper {
    fn set_sink(&self, sink: Box<dyn FnMut(RawFd, FdEvents)>) {
        *self.sink.borrow_mut() = Some(sink);
    }

    fn add_fd(&self, fd: RawFd, events: FdEvents) -> Result<(), UiError> {
        let data = std::ptr::from_ref(self.sink).cast_mut().cast::<c_void>();
        self.looper
            .add_fd(fd, looper_events(events), Some(on_ready), data)
            .map_err(|e| UiError::Looper(format!("{e:#}")))
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        self.looper
            .remove_fd(fd)
            .map_err(|e| UiError::Looper(format!("{e:#}")))
    }
}

/// `ALooper_callbackFunc` of every registration: hands the readiness to the sink and stays registered.
unsafe extern "C" fn on_ready(fd: c_int, events: c_int, data: *mut c_void) -> c_int {
    // SAFETY: `data` is the `Sink` that `AndroidLooper::new` leaked, valid for the process; the Looper calls back on the thread that registered, and the sink never polls the Looper, so no other borrow of the cell is live
    let cell = unsafe { &*data.cast::<Sink>() };
    if let Some(sink) = cell.borrow_mut().as_mut() {
        sink(fd, events_from_looper(events));
    }
    1
}

/// The events a registration asks for, as `ALOOPER_EVENT_*` bits (error and hangup are reported, never asked for).
fn looper_events(events: FdEvents) -> c_int {
    let mut bits = 0;
    if events.input {
        bits |= looper::ALOOPER_EVENT_INPUT as c_int;
    }
    if events.output {
        bits |= looper::ALOOPER_EVENT_OUTPUT as c_int;
    }
    bits
}

/// The `ALOOPER_EVENT_*` bits a callback reports.
fn events_from_looper(bits: c_int) -> FdEvents {
    FdEvents {
        input: (bits & looper::ALOOPER_EVENT_INPUT as c_int) != 0,
        output: (bits & looper::ALOOPER_EVENT_OUTPUT as c_int) != 0,
        error: (bits & looper::ALOOPER_EVENT_ERROR as c_int) != 0,
        hangup: (bits & looper::ALOOPER_EVENT_HANGUP as c_int) != 0,
    }
}
