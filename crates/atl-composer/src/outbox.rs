// SPDX-License-Identifier: MIT
//! The outbox: one VM-attached thread for the binder calls the window's thread starts (display-thread plan D2 rule B1: the display thread makes no JNI or binder call).
//! An in-process Java binder runs its onTransact on the caller's thread and needs that thread's JNIEnv (AOSP android_util_Binder.cpp `javavm_to_jnienv` is GetEnv, NULL on an unattached thread),
//! and a capture listener in the app attaches the calling thread itself (android_window_ScreenCapture.cpp `getenv`): so capture replies, the Task's reports and the input method's calls
//! on the editor's connection (atl-ime field.rs `send`) are posted here. The editor's answers come back asynchronously, so GTK's `retrieve-surrounding` is answered on the display thread from the input method's own copy of the editor's text (atl-ime editor_text.rs).
//! The thread owns a Looper and an `atl_looper::Poster` on it: one FIFO in post order; posts after the thread ended are dropped and counted by the Poster. Posting never blocks the caller.

use std::sync::mpsc;

use atl_looper::{LooperApi, Poster};
use atl_platform_ui::LooperPoster;

use crate::host::VmHost;

/// The outbox thread's name; the IME's `sent` log line names the thread its call ran on, and the window ime stage checks it is this one.
pub const THREAD: &str = "atl-outbox";

/// Posts closures to the outbox thread.
pub struct Outbox(Poster);

impl Outbox {
    /// Starts the outbox thread on `threads` and waits for its Poster: a one-time startup handshake (thread start + Looper prepare), before the window is installed.
    pub fn spawn(threads: &dyn VmHost) -> Result<Outbox, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        threads
            .spawn(
                THREAD,
                Box::new(move || {
                    let looper = match LooperApi::load().and_then(|api| api.prepare_looper()) {
                        Ok(looper) => looper,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    match Poster::attach(&looper) {
                        Ok(poster) => {
                            let _ = tx.send(Ok(poster));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    }
                    loop {
                        looper.poll_once(-1);
                    }
                }),
            )
            .map_err(|e| format!("outbox thread: {e}"))?;
        let poster = rx
            .recv()
            .map_err(|_| "outbox thread ended before its Looper".to_owned())??;
        Ok(Outbox(poster))
    }
}

impl LooperPoster for Outbox {
    fn post(&self, f: Box<dyn FnOnce() + Send>) {
        // break `outbox` (window ime `--break-outbox`): the call runs on the poster's thread, as before the outbox, so the ime stage's outbox sub-gate must fail
        if cfg!(feature = "break-outbox") {
            f();
        } else {
            self.0.post(f);
        }
    }
}
