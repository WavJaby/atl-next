// SPDX-License-Identifier: MIT
//! The toolkit session: GTK started on the module's own display thread (`atl-display`), which runs GLib's default main loop for the process's life.
//!
//! Thread model (owner 2026-11-20, display-thread plan D2): the display thread owns GLib's default context, GTK and GDK's Wayland connection, and waits on no app thread (rule B1); app threads reach it only through [`GtkInvoker`] (`invoke`, `call`). Android's UI thread runs its own Looper apart from it.
//! A UI thread that stops polling (an app blocking in `onCreate`) answers no `xdg_wm_base.ping`, so the compositor shows its native "not responding" dialog: GDK's ping is held until the app's main Looper ran a liveness token ([`ping`], display-thread plan D3).

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, ThreadId};

use atl_platform_ui_api::{DeviceNumber, DisplayInvoker, LooperPoster, Ui, UiError};
use gtk4::glib;

use crate::ping;

/// The session: GTK started on the display thread ([`attach`](GtkUi::attach) spawns it).
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

/// [`DisplayInvoker`] for GTK: the display thread's GLib main context, safe to invoke on from any thread (GLib wakes the display thread's loop).
/// Order: invokes from one other thread run in queue order. GLib 2.84.4 `gmain.c`: a non-owner `g_main_context_invoke_full` attaches an idle source at the given priority, `source_add_to_context` appends it at its priority list's tail, check collects ready sources in list order, `g_main_dispatch` runs them in that order;
/// the one branch that runs inline (the context is the caller's thread default and `g_main_context_acquire` succeeds) cannot happen while the display thread holds the context (`run_display`). Pinned by the test `invokes_from_another_thread_run_in_queue_order`.
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
        liveness: Box<dyn LooperPoster>,
        buffer_device: Option<DeviceNumber>,
    ) -> Result<Self, UiError> {
        let (ready, started) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(DISPLAY_THREAD.to_owned())
            .spawn(move || run_display(liveness, ready))
            .map_err(|e| UiError::Unavailable(format!("display thread: {e}")))?;
        let thread = started.recv().map_err(|_| {
            UiError::Unavailable("the display thread ended before GTK started".to_owned())
        })??;
        Ok(GtkUi {
            thread,
            buffer_device,
        })
    }
}

/// The display thread's name (D2: owns GTK, GDK's Wayland connection and the GLib default context).
pub(crate) const DISPLAY_THREAD: &str = "atl-display";

/// The display thread's body: GTK started here, the outcome handed to [`attach`](GtkUi::attach) (its thread id on success), then the default context's main loop for the process's life.
/// No display thread can end quietly while `call`s wait on it: a panic before the outcome is sent drops `ready`, so attach returns an error; every later Rust frame runs inside a GLib dispatch
/// (`extern "C"`: a panic aborts the process there), and the process's crates are built `panic = "abort"` (xtask packages.rs) anyway. Gate: `xtask test ui-module` `--panic-on-display`.
fn run_display(
    liveness: Box<dyn LooperPoster>,
    ready: mpsc::SyncSender<Result<ThreadId, UiError>>,
) {
    // GTK started by another thread earlier in this process: that thread owns it, this one cannot (gtk4's init would panic)
    if gtk4::is_initialized() {
        let _ = ready.send(Err(UiError::WrongThread));
        return;
    }
    // before GTK init: GDK registers its xdg_wm_base listener while opening the display
    ping::prepare(liveness);
    if let Err(e) = gtk4::init() {
        let _ = ready.send(Err(UiError::Unavailable(e.to_string())));
        return;
    }
    // gtk4's init acquired the default main context and never releases it (gtk4 rt.rs `init`): this thread owns it for the process's life, so `is_owner` holds here only and every other thread's invoke is queued to this loop
    ping::report();
    let main_loop = glib::MainLoop::new(None, false);
    let _ = ready.send(Ok(thread::current().id()));
    main_loop.run();
    tracing::info!("atl-platform-ui: display: main loop ended");
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};

    use atl_platform_ui_api::DisplayInvoker;
    use gtk4::glib;

    use super::{CallDone, CallSignal, GtkInvoker};

    /// The WebView claim's replay and a later focus are two invokes from an app thread; the second must not run first (sideband.rs). Also any `invoke` caller's ordering assumption.
    #[test]
    fn invokes_from_another_thread_run_in_queue_order() {
        // enough that a reversed or batched-out-of-order dispatch shows; GLib dispatches several ready idles per iteration
        const N: usize = 500;
        // a context of its own: the process default has one owner thread, as the display thread holds it
        let ctx = glib::MainContext::new();
        let (owned, is_owned) = mpsc::channel();
        let owner_ctx = ctx.clone();
        let owner = std::thread::spawn(move || {
            let _held = owner_ctx.acquire().expect("acquire");
            let main_loop = glib::MainLoop::new(Some(&owner_ctx), false);
            owned.send(main_loop.clone()).expect("send");
            main_loop.run();
        });
        let main_loop = is_owned.recv().expect("owner");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let invoker = GtkInvoker(ctx);
        for i in 0..N {
            let seen = Arc::clone(&seen);
            invoker.invoke(Box::new(move || seen.lock().expect("seen").push(i)));
        }
        invoker.invoke(Box::new(move || main_loop.quit()));
        owner.join().expect("owner thread");
        let seen = seen.lock().expect("seen");
        assert_eq!(*seen, (0..N).collect::<Vec<_>>());
    }

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
