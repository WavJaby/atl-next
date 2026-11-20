// SPDX-License-Identifier: MIT
//! The UI thread's Android Looper as the platform layer sees it: [`AndroidLooper`] is the toolkit's fd seam ([`atl_platform_ui::Looper`], the temporary seam display-thread S5 deletes), [`MainPoster`] the liveness poster ([`atl_platform_ui::LooperPoster`]), both over atl-looper.
//! One trampoline serves every toolkit registration: each `ALooper_addFd` gets the same callback and the same data pointer, and the trampoline hands the readiness to the sink, which the platform sets once. The sink cell is leaked with the Looper, so a removal's leftover call from the same poll batch still reads valid memory; the sink then ignores the fd.
//! Thread-bound: created and used on the UI thread only, the thread that prepared the Looper.

use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::os::fd::RawFd;

use atl_looper::events;
use atl_platform_ui::{FdEvents, Looper, LooperPoster, UiError};

/// The readiness sink of this thread's Looper; leaked with the Looper (see module docs).
type Sink = RefCell<Option<Box<dyn FnMut(RawFd, FdEvents)>>>;

/// This thread's Looper as the platform's [`Looper`].
pub struct AndroidLooper {
    looper: atl_looper::Looper,
    sink: &'static Sink,
}

impl AndroidLooper {
    /// Wraps the thread's Looper, as `atl_looper::LooperApi::prepare_looper` returns it.
    pub fn new(looper: atl_looper::Looper) -> Self {
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
        // SAFETY: `data` is the leaked sink, valid for the process; `on_ready` reads it as `Sink`
        unsafe {
            self.looper
                .add_fd(fd, looper_events(events), Some(on_ready), data)
        }
        .map_err(|e| UiError::Looper(e.to_string()))
    }

    fn remove_fd(&self, fd: RawFd) -> Result<(), UiError> {
        self.looper
            .remove_fd(fd)
            .map_err(|e| UiError::Looper(e.to_string()))
    }
}

/// The main thread's `post` executor as the platform's liveness poster: the toolkit's liveness token runs in the main Looper's poll, as AOSP's input events do.
pub struct MainPoster(pub atl_looper::Poster);

impl LooperPoster for MainPoster {
    fn post(&self, f: Box<dyn FnOnce() + Send>) {
        self.0.post(f);
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
fn looper_events(e: FdEvents) -> c_int {
    let mut bits = 0;
    if e.input {
        bits |= events::INPUT;
    }
    if e.output {
        bits |= events::OUTPUT;
    }
    bits
}

/// The `ALOOPER_EVENT_*` bits a callback reports.
fn events_from_looper(bits: c_int) -> FdEvents {
    FdEvents {
        input: bits & events::INPUT != 0,
        output: bits & events::OUTPUT != 0,
        error: bits & events::ERROR != 0,
        hangup: bits & events::HANGUP != 0,
    }
}
