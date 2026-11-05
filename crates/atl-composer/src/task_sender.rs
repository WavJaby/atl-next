// SPDX-License-Identifier: MIT
//! What the window reports to the services about its Task (compositor_registry; tasks.rs there): its content size as the Task's bounds (`SET_BOUNDS`) and the monitor it is on as the display (`SET_DISPLAY`).
//! The services name the Task first (the UI agent's TASK, when the window is added); a report before that waits, since no Task can take it yet (the registry refuses SET_DISPLAY from an app with no Task).
//! The platform reports at each change, many in a burst during a drag: the sender keeps only the latest of each and sends that, so a burst costs one transaction, not one per step. The display goes before the size: a size measured on the new monitor belongs to the new display.
//! The UI thread only stores the values. A transaction is a blocking binder call and runs on the sender's own thread, so it never stalls the toolkit's main loop.
//! One window per process, so one Task: a different number named later is refused, and nothing is ever sent under the first Task's number for it.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use atl_platform_ui::PhysSize;
use atl_services::compositor_registry::{SERVICE, SET_BOUNDS, SET_DISPLAY};
use atl_services::display_model::DisplayModel;
use binder::binder_impl::IBinderInternal;
use binder::{SpIBinder, Strong};

use crate::host::ThreadHost;
use crate::start::IRegistry;

/// What the sender waits for: the Task the window is (none until the services name it) and the latest values not yet sent.
#[derive(Default)]
struct Pending {
    task: Option<i32>,
    size: Option<PhysSize>,
    display: Option<DisplayModel>,
}

/// One report, taken from [`Pending`].
enum Report {
    Display(DisplayModel),
    Size(i32, PhysSize),
}

impl Pending {
    /// The next report to send, once the Task is named: the display first.
    fn take(&mut self) -> Option<Report> {
        let task = self.task?;
        if let Some(display) = self.display.take() {
            return Some(Report::Display(display));
        }
        self.size.take().map(|size| Report::Size(task, size))
    }
}

/// The pending state and the condition the sender thread waits on.
pub struct TaskSender {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl TaskSender {
    /// Starts the sender's thread on `threads`; the handle is what the UI thread submits values to, and what the UI agent names the Task to.
    pub fn spawn(threads: &dyn ThreadHost) -> std::io::Result<Arc<Self>> {
        let sender = Arc::new(TaskSender {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&sender);
        threads.spawn("atl-task-sender", Box::new(move || worker.run()))?;
        Ok(sender)
    }

    /// The window's Task (the UI agent's TASK). A number different from the one already named is refused and logged; the same number again changes nothing.
    pub fn set_task(&self, task: i32) {
        let mut pending = self.lock();
        match pending.task {
            Some(current) if current != task => {
                tracing::warn!(
                    "atl-composer: task sender: task {task} refused: the window is task {current}"
                );
            }
            _ => {
                pending.task = Some(task);
                self.wake.notify_one();
            }
        }
    }

    /// A size change on the UI thread: replaces the pending size, so a size overtaken before it was sent is never sent.
    pub fn submit_size(&self, size: PhysSize) {
        self.lock().size = Some(size);
        self.wake.notify_one();
    }

    /// The window's monitor changed (UI thread): replaces the pending display. The services ignore a display equal to theirs (tasks.rs `set_display`).
    pub fn submit_display(&self, display: DisplayModel) {
        self.lock().display = Some(display);
        self.wake.notify_one();
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        // a poisoned state still holds the pending values
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn run(&self) {
        loop {
            let report = {
                let mut pending = self.lock();
                loop {
                    if let Some(report) = pending.take() {
                        break report;
                    }
                    pending = self.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                }
            };
            if let Err(e) = send(&report) {
                tracing::warn!("atl-composer: task sender: {e}");
            }
        }
    }
}

/// The registry, looked up at each send (the services are up before any app starts). The binder is converted to the registry's interface first: libbinder_ndk's prepare_transact refuses a remote binder with no class (`AIBinder_associateClass`), and the conversion associates it, as REGISTER does (start.rs).
fn registry() -> Result<SpIBinder, String> {
    let registry: Strong<dyn IRegistry> = binder::check_service(SERVICE)
        .ok_or_else(|| format!("{SERVICE}: not registered"))?
        .into_interface()
        .map_err(|e| format!("{SERVICE}: not the registry: {e}"))?;
    Ok(registry.as_binder())
}

/// One transaction: SET_DISPLAY with the `--display` form, or SET_BOUNDS with the Task's number, then the size.
fn send(report: &Report) -> Result<(), String> {
    let binder = registry()?;
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    let code = match report {
        Report::Display(display) => {
            data.write(&display.to_arg()).map_err(|e| e.to_string())?;
            SET_DISPLAY
        }
        Report::Size(task, size) => {
            let width = i32::try_from(size.width).map_err(|e| e.to_string())?;
            let height = i32::try_from(size.height).map_err(|e| e.to_string())?;
            data.write(task).map_err(|e| e.to_string())?;
            data.write(&width).map_err(|e| e.to_string())?;
            data.write(&height).map_err(|e| e.to_string())?;
            SET_BOUNDS
        }
    };
    binder
        .submit_transact(code, data, 0)
        .map_err(|e| format!("transaction {code}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> PhysSize {
        PhysSize { width, height }
    }

    /// Nothing goes before the Task is named; then the display goes before the size, and each only once (the latest).
    #[test]
    fn reports_wait_for_the_task_and_put_the_display_first() {
        let mut p = Pending::default();
        let first = DisplayModel::from_monitor(1920, 1080, None, 1.0);
        let second = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6);
        p.size = Some(size(800, 600));
        p.display = Some(first);
        p.display = Some(second);
        assert!(p.take().is_none());
        p.task = Some(3);
        assert!(matches!(p.take(), Some(Report::Display(d)) if d == second));
        assert!(matches!(p.take(), Some(Report::Size(3, s)) if s == size(800, 600)));
        assert!(p.take().is_none());
    }
}
