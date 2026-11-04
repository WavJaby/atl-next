// SPDX-License-Identifier: MIT
//! The toolkit session and the one main loop.
//!
//! Thread model (plan Q6, owner 2026-11-04): GLib's default main context is iterated by the thread that attached, and that thread is also the Android Looper's. The Looper's epoll fd is added to the GLib context (`watch_fd`), not GLib's fds to the Looper, because:
//! * GLib's fd set is large and changes every pass (Wayland connection, GL, portals, input method), and each source has its own timeout; mirroring that into `ALooper_addFd` means re-running `prepare/query/check/dispatch` by hand. The Looper contributes one stable fd.
//! * the Java `MessageQueue` computes its own timeout and calls `nativePollOnce(timeout)`; that native call becomes one bounded [`GtkUi::iterate`] pass, and a readable Looper fd runs `pollOnce(0)` inside it, so the Java loop is unchanged.
//!
//! No GTK thread exists. A UI thread that stops iterating (an app blocking in `onCreate`) stops dispatching the Wayland connection, so GDK never answers `xdg_wm_base.ping` and the compositor shows its native "not responding" dialog.

use std::marker::PhantomData;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::thread::{self, ThreadId};
use std::time::Duration;

use atl_platform_ui_api::{Ui, UiError};
use gtk4::glib;
use rustix::io::fcntl_dupfd_cloexec;

/// The session: GTK initialised on this thread, whose GLib main context [`iterate`](Ui::iterate) runs. Not `Send`: it must stay on the UI thread.
pub struct GtkUi {
    thread: ThreadId,
    _not_send: PhantomData<*const ()>,
}

/// A watched fd; dropping it removes the watch, then closes the duplicate it watched.
pub struct FdWatch {
    source: Option<glib::SourceId>,
    _duplicate: OwnedFd,
}

impl Drop for FdWatch {
    fn drop(&mut self) {
        if let Some(id) = self.source.take() {
            id.remove();
        }
    }
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
    type Watch = FdWatch;

    fn attach() -> Result<Self, UiError> {
        gtk4::init().map_err(|e| UiError::Unavailable(e.to_string()))?;
        // GTK may have been started by another thread earlier in this process: that thread owns it, this one cannot
        if !gtk4::is_initialized_main_thread() {
            return Err(UiError::WrongThread);
        }
        Ok(GtkUi {
            thread: thread::current().id(),
            _not_send: PhantomData,
        })
    }

    fn watch_fd<F: FnMut() + 'static>(
        &self,
        fd: BorrowedFd<'_>,
        mut on_readable: F,
    ) -> Result<FdWatch, UiError> {
        self.check_thread()?;
        // GLib watches a duplicate of the fd (same open file description, so the same readiness): the guard owns it, which removes any lifetime contract with the caller.
        let duplicate = fcntl_dupfd_cloexec(fd, 0).map_err(|e| UiError::Watch(e.to_string()))?;
        let id = glib_unix::unix_fd_add_local(
            duplicate.as_raw_fd(),
            glib::IOCondition::IN,
            move |_, _| {
                on_readable();
                glib::ControlFlow::Continue
            },
        );
        Ok(FdWatch {
            source: Some(id),
            _duplicate: duplicate,
        })
    }

    fn iterate(&self, timeout: Option<Duration>) -> Result<(), UiError> {
        self.check_thread()?;
        let context = glib::MainContext::default();
        match timeout {
            Some(t) if t.is_zero() => {
                context.iteration(false);
            }
            Some(t) => {
                // a one-shot timer bounds the blocking wait; whatever dispatches first ends the pass
                let timer = glib::timeout_source_new(t, None, glib::Priority::DEFAULT, || {
                    glib::ControlFlow::Break
                });
                timer.attach(Some(&context));
                context.iteration(true);
                timer.destroy();
            }
            None => {
                context.iteration(true);
            }
        }
        Ok(())
    }
}
