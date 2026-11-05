// SPDX-License-Identifier: MIT
//! The window's content size, sent to the services as its Task's bounds (compositor_registry `SET_BOUNDS`; tasks.rs there). The platform reports a size at each change, many in a burst
//! during a drag: the sender keeps only the latest and sends that, so a burst costs one transaction, not one per step.
//! The UI thread only stores the size. The transaction is a blocking binder call and runs on the sender's own thread, so it never stalls the toolkit's main loop.

use std::sync::{Arc, Condvar, Mutex};

use atl_platform_ui::PhysSize;
use atl_services::compositor_registry::{SERVICE, SET_BOUNDS};
use binder::SpIBinder;
use binder::binder_impl::IBinderInternal;

use crate::host::ThreadHost;

/// The latest size not yet sent, and the condition the sender thread waits on.
pub struct BoundsSender {
    latest: Mutex<Option<PhysSize>>,
    wake: Condvar,
}

impl BoundsSender {
    /// Starts the sender's thread on `threads`; the handle is what the UI thread submits sizes to.
    pub fn spawn(threads: &dyn ThreadHost) -> std::io::Result<Arc<Self>> {
        let sender = Arc::new(BoundsSender {
            latest: Mutex::new(None),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&sender);
        threads.spawn("atl-task-bounds", Box::new(move || worker.run()))?;
        Ok(sender)
    }

    /// A size change on the UI thread: replaces the pending size, so a size overtaken before it was sent is never sent.
    pub fn submit(&self, size: PhysSize) {
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(size);
        self.wake.notify_one();
    }

    fn run(&self) {
        loop {
            let size = {
                let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(size) = latest.take() {
                        break size;
                    }
                    latest = self.wake.wait(latest).unwrap_or_else(|e| e.into_inner());
                }
            };
            if let Err(e) = send(size) {
                tracing::warn!("atl-composer: task bounds: {e}");
            }
        }
    }
}

/// One SET_BOUNDS transaction, the services looked up at each send (they are up before any app starts).
fn send(size: PhysSize) -> Result<(), String> {
    let registry: SpIBinder = binder::check_service(SERVICE).ok_or_else(|| {
        format!(
            "{SERVICE}: not registered, bounds {}x{} not sent",
            size.width, size.height
        )
    })?;
    let width = i32::try_from(size.width).map_err(|e| e.to_string())?;
    let height = i32::try_from(size.height).map_err(|e| e.to_string())?;
    let mut data = registry.prepare_transact().map_err(|e| e.to_string())?;
    data.write(&width).map_err(|e| e.to_string())?;
    data.write(&height).map_err(|e| e.to_string())?;
    registry
        .submit_transact(SET_BOUNDS, data, 0)
        .map_err(|e| format!("set bounds: {e}"))?;
    Ok(())
}
