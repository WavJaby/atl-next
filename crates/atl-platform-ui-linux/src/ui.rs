// SPDX-License-Identifier: MIT
//! The toolkit session: GTK initialised on the UI thread, whose GLib main context is mirrored into that thread's Android Looper ([`mirror`]).
//!
//! Thread model (owner decision 2026-11-05, plan Q6): the Looper is the outer loop. Java's `nativePollOnce` polls it, and GLib's fds and timeout are registered with it, so GTK's dispatch runs inside the Looper's poll with no AOSP patch. No GTK thread exists.
//! A UI thread that stops polling (an app blocking in `onCreate`) answers no `xdg_wm_base.ping`, so the compositor shows its native "not responding" dialog: GDK's ping is held until the app's main Looper ran a liveness token ([`ping`], display-thread plan D3), which holds whichever thread the toolkit runs on.

use std::marker::PhantomData;
use std::thread::{self, ThreadId};

use atl_platform_ui_api::{DeviceNumber, Looper, LooperPoster, Ui, UiError, UiInvoker};
use gtk4::glib;

use crate::{mirror, ping};

/// The session: GTK initialised on this thread, whose GLib main context the thread's Looper polls. Not `Send`: it must stay on the UI thread.
pub struct GtkUi {
    thread: ThreadId,
    /// The DRM device presented buffers must come from ([`Ui::attach`]).
    pub(crate) buffer_device: Option<DeviceNumber>,
    _not_send: PhantomData<*const ()>,
}

impl GtkUi {
    /// Every call is refused off the UI thread: GTK is not thread-safe, and a wrong-thread call would corrupt it silently.
    pub(crate) fn check_thread(&self) -> Result<(), UiError> {
        if thread::current().id() == self.thread {
            Ok(())
        } else {
            Err(UiError::WrongThread)
        }
    }
}

/// [`UiInvoker`] for GTK: the UI thread's GLib main context, safe to invoke on from any thread (its wakeup fd is among the fds the Looper polls).
pub struct GtkInvoker(glib::MainContext);

impl UiInvoker for GtkInvoker {
    fn invoke(&self, f: Box<dyn FnOnce() + Send>) {
        self.0.invoke(f);
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
            _not_send: PhantomData,
        })
    }
}
