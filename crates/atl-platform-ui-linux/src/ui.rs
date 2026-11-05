// SPDX-License-Identifier: MIT
//! The toolkit session: GTK initialised on the UI thread, whose GLib main context is mirrored into that thread's Android Looper ([`mirror`]).
//!
//! Thread model (owner decision 2026-11-05, plan Q6): the Looper is the outer loop. Java's `nativePollOnce` polls it, and GLib's fds and timeout are registered with it, so GTK's dispatch runs inside the Looper's poll with no AOSP patch. No GTK thread exists.
//! A UI thread that stops polling (an app blocking in `onCreate`) stops dispatching the Wayland connection, so GDK never answers `xdg_wm_base.ping` and the compositor shows its native "not responding" dialog.

use std::marker::PhantomData;
use std::thread::{self, ThreadId};

use atl_platform_ui_api::{Looper, Ui, UiError};

use crate::mirror;

/// The session: GTK initialised on this thread, whose GLib main context the thread's Looper polls. Not `Send`: it must stay on the UI thread.
pub struct GtkUi {
    thread: ThreadId,
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

impl Ui for GtkUi {
    fn attach(looper: Box<dyn Looper>) -> Result<Self, UiError> {
        gtk4::init().map_err(|e| UiError::Unavailable(e.to_string()))?;
        // GTK may have been started by another thread earlier in this process: that thread owns it, this one cannot
        if !gtk4::is_initialized_main_thread() {
            return Err(UiError::WrongThread);
        }
        mirror::install(looper)?;
        Ok(GtkUi {
            thread: thread::current().id(),
            _not_send: PhantomData,
        })
    }
}
