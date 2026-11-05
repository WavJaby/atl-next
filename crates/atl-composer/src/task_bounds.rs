// SPDX-License-Identifier: MIT
//! The window's content size, sent to the services as its Task's bounds (compositor_registry `SET_BOUNDS`; tasks.rs there). The services name the Task first (the UI agent's TASK, when the window is added); a size before that waits, since no Task can take it yet.
//! The platform reports a size at each change, many in a burst during a drag: the sender keeps only the latest and sends that, so a burst costs one transaction, not one per step.
//! The UI thread only stores the size. The transaction is a blocking binder call and runs on the sender's own thread, so it never stalls the toolkit's main loop.
//! One window per process, so one Task: a different number named later is refused, and its size is never sent under the first Task's number.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use atl_platform_ui::PhysSize;
use atl_services::compositor_registry::{SERVICE, SET_BOUNDS};
use binder::Strong;
use binder::binder_impl::IBinderInternal;

use crate::host::ThreadHost;
use crate::start::IRegistry;

/// What the sender waits for: the Task the window is (none until the services name it) and the latest size not yet sent.
#[derive(Default)]
struct Pending {
    task: Option<i32>,
    size: Option<PhysSize>,
}

/// The pending state and the condition the sender thread waits on.
pub struct BoundsSender {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl BoundsSender {
    /// Starts the sender's thread on `threads`; the handle is what the UI thread submits sizes to, and what the UI agent names the Task to.
    pub fn spawn(threads: &dyn ThreadHost) -> std::io::Result<Arc<Self>> {
        let sender = Arc::new(BoundsSender {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&sender);
        threads.spawn("atl-task-bounds", Box::new(move || worker.run()))?;
        Ok(sender)
    }

    /// The window's Task (the UI agent's TASK). A number different from the one already named is refused and logged; the same number again changes nothing.
    pub fn set_task(&self, task: i32) {
        let mut pending = self.lock();
        match pending.task {
            Some(current) if current != task => {
                tracing::warn!(
                    "atl-composer: task bounds: task {task} refused: the window is task {current}"
                );
            }
            _ => {
                pending.task = Some(task);
                self.wake.notify_one();
            }
        }
    }

    /// A size change on the UI thread: replaces the pending size, so a size overtaken before it was sent is never sent.
    pub fn submit(&self, size: PhysSize) {
        self.lock().size = Some(size);
        self.wake.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        // a poisoned state still holds the pending values
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn run(&self) {
        loop {
            let (task, size) = {
                let mut pending = self.lock();
                loop {
                    if let Some(task) = pending.task
                        && let Some(size) = pending.size.take()
                    {
                        break (task, size);
                    }
                    pending = self.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                }
            };
            if let Err(e) = send(task, size) {
                tracing::warn!("atl-composer: task bounds: {e}");
            }
        }
    }
}

/// One SET_BOUNDS transaction: the Task's number, then the size. The services are looked up at each send (they are up before any app starts).
/// The binder is converted to the registry's interface first: libbinder_ndk's prepare_transact refuses a remote binder with no class (`AIBinder_associateClass`), and the conversion associates it, as REGISTER does (start.rs).
fn send(task: i32, size: PhysSize) -> Result<(), String> {
    let registry: Strong<dyn IRegistry> = binder::check_service(SERVICE)
        .ok_or_else(|| {
            format!(
                "{SERVICE}: not registered, task {task} bounds {}x{} not sent",
                size.width, size.height
            )
        })?
        .into_interface()
        .map_err(|e| format!("{SERVICE}: not the registry: {e}"))?;
    let binder = registry.as_binder();
    let width = i32::try_from(size.width).map_err(|e| e.to_string())?;
    let height = i32::try_from(size.height).map_err(|e| e.to_string())?;
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    data.write(&task).map_err(|e| e.to_string())?;
    data.write(&width).map_err(|e| e.to_string())?;
    data.write(&height).map_err(|e| e.to_string())?;
    binder
        .submit_transact(SET_BOUNDS, data, 0)
        .map_err(|e| format!("set bounds: {e}"))?;
    Ok(())
}
