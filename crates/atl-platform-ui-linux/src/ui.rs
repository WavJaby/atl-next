// SPDX-License-Identifier: MIT
//! The toolkit session: GTK initialised on the UI thread, whose GLib main context is mirrored into that thread's Android Looper ([`mirror`]).
//!
//! Thread model (owner decision 2026-11-05, plan Q6): the Looper is the outer loop. Java's `nativePollOnce` polls it, and GLib's fds and timeout are registered with it, so GTK's dispatch runs inside the Looper's poll with no AOSP patch. No GTK thread exists.
//! A UI thread that stops polling (an app blocking in `onCreate`) answers no `xdg_wm_base.ping`, so the compositor shows its native "not responding" dialog: GDK's ping is held until the app's main Looper ran a liveness token ([`ping`], display-thread plan D3), which holds whichever thread the toolkit runs on.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};

use atl_platform_ui_api::{DeviceNumber, DisplayInvoker, Looper, LooperPoster, Ui, UiError};
use gtk4::glib;

use crate::{mirror, ping};

/// The session: GTK initialised on this thread, the display thread, whose GLib main context the thread's Looper polls.
/// Send + Sync by construction: its state is plain data (the thread's id, the buffer device) and holds no GTK object; every operation that reaches GTK refuses off the display thread ([`check_thread`](GtkUi::check_thread)) or hops there ([`GtkInvoker`]).
pub struct GtkUi {
    thread: ThreadId,
    /// The DRM device presented buffers must come from ([`Ui::attach`]).
    pub(crate) buffer_device: Option<DeviceNumber>,
}

impl GtkUi {
    /// Every call is refused off the display thread: GTK is not thread-safe, and a wrong-thread call would corrupt it silently.
    pub(crate) fn check_thread(&self) -> Result<(), UiError> {
        if thread::current().id() == self.thread {
            Ok(())
        } else {
            Err(UiError::WrongThread)
        }
    }
}

/// [`DisplayInvoker`] for GTK: the display thread's GLib main context, safe to invoke on from any thread (its wakeup fd is among the fds the Looper polls).
pub struct GtkInvoker(glib::MainContext);

impl DisplayInvoker for GtkInvoker {
    fn invoke(&self, f: Box<dyn FnOnce() + Send>) {
        self.0.invoke(f);
    }

    fn call(&self, f: Box<dyn FnOnce() + Send>) -> bool {
        // the display thread owns (acquired) the context: queueing and waiting would wait on itself
        if self.0.is_owner() {
            tracing::info!("atl-platform-ui: display: call on display");
            f();
            return true;
        }
        let done = Arc::new(CallDone::default());
        let signal = CallSignal(Arc::clone(&done));
        self.0.invoke(move || {
            f();
            signal.finish(true);
        });
        done.wait()
    }
}

/// One cross-thread [`GtkInvoker::call`]'s outcome: None while pending, then whether the closure ran.
#[derive(Default)]
struct CallDone {
    ran: Mutex<Option<bool>>,
    cv: Condvar,
}

impl CallDone {
    fn wait(&self) -> bool {
        let mut ran = self.ran.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(ran) = *ran {
                return ran;
            }
            ran = self.cv.wait(ran).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// The display side of a call: set once the closure ran; dropped unrun (the context dropped the queued closure) it ends the wait with false, so the caller never waits for a closure that is gone.
struct CallSignal(Arc<CallDone>);

impl CallSignal {
    fn finish(&self, ran: bool) {
        let mut slot = self.0.ran.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(ran);
            self.0.cv.notify_all();
        }
    }
}

impl Drop for CallSignal {
    fn drop(&mut self) {
        self.finish(false);
    }
}

impl Ui for GtkUi {
    type Invoker = GtkInvoker;

    fn invoker(&self) -> GtkInvoker {
        GtkInvoker(glib::MainContext::default())
    }

    fn attach(
        looper: Box<dyn Looper>,
        liveness: Box<dyn LooperPoster>,
        buffer_device: Option<DeviceNumber>,
    ) -> Result<Self, UiError> {
        // before GTK init: GDK registers its xdg_wm_base listener while opening the display
        ping::prepare(liveness);
        gtk4::init().map_err(|e| UiError::Unavailable(e.to_string()))?;
        // GTK may have been started by another thread earlier in this process: that thread owns it, this one cannot
        if !gtk4::is_initialized_main_thread() {
            return Err(UiError::WrongThread);
        }
        ping::report();
        mirror::install(looper)?;
        Ok(GtkUi {
            thread: thread::current().id(),
            buffer_device,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CallDone, CallSignal};

    #[test]
    fn a_call_whose_closure_is_dropped_unrun_ends_with_false() {
        let done = Arc::new(CallDone::default());
        let signal = CallSignal(Arc::clone(&done));
        let dropper = std::thread::spawn(move || drop(signal));
        assert!(!done.wait());
        dropper.join().expect("dropper");
    }

    #[test]
    fn a_call_that_ran_ends_with_true_and_the_drop_after_keeps_it() {
        let done = Arc::new(CallDone::default());
        let signal = CallSignal(Arc::clone(&done));
        let runner = std::thread::spawn(move || signal.finish(true));
        assert!(done.wait());
        runner.join().expect("runner");
    }
}
