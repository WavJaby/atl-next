// SPDX-License-Identifier: MIT
//! What the window reports to the services about its Task (compositor_registry; tasks.rs there): its content size as the Task's bounds (`SET_BOUNDS`) and the monitor it is on as the display (`SET_DISPLAY`).
//! The services name the Task first (the UI agent's TASK, when the window is added); a report before that waits, since no Task can take it yet (the registry refuses SET_DISPLAY from an app with no Task).
//! The platform reports at each change, many in a burst during a drag: only the latest of each is kept and sent, so a burst costs one transaction, not one per step. The display goes before the size: a size measured on the new monitor belongs to the new display.
//! The window's thread only stores the values. A transaction is a blocking binder call and runs on the outbox (outbox.rs): one flush is posted when a value arrives and none is pending, so the toolkit's loop never stalls.
//! One window per process, so one Task: a different number named later is refused, and nothing is ever sent under the first Task's number for it.
//! The desktop closing the window is `REMOVE_TASK`, the Shell's removeTask: sent after any pending display and size, once per close request.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_platform_ui::{LooperPoster, PhysSize};
use atl_services_types::compositor_registry::{REMOVE_TASK, SERVICE, SET_BOUNDS, SET_DISPLAY};
use atl_services_types::display_model::DisplayModel;
use binder::binder_impl::IBinderInternal;
use binder::{SpIBinder, Strong};

use crate::start::IRegistry;

/// The Task the window is (none until the services name it), the latest values not yet sent, and whether a flush is already posted.
#[derive(Default)]
struct Pending {
    task: Option<i32>,
    size: Option<PhysSize>,
    display: Option<DisplayModel>,
    /// The desktop asked to close the window; requests before the send coalesce into one.
    remove: bool,
    /// A flush is posted to the outbox and has not yet found nothing left to send.
    flushing: bool,
}

/// One report, taken from [`Pending`].
enum Report {
    Display(DisplayModel),
    Size(i32, PhysSize),
    RemoveTask(i32),
}

impl Pending {
    /// The next report to send, once the Task is named: the display first, the removal last.
    fn take(&mut self) -> Option<Report> {
        let task = self.task?;
        if let Some(display) = self.display.take() {
            return Some(Report::Display(display));
        }
        if let Some(size) = self.size.take() {
            return Some(Report::Size(task, size));
        }
        std::mem::take(&mut self.remove).then_some(Report::RemoveTask(task))
    }

    /// Whether a change needs a flush posted: something is sendable and no flush is pending. Marks the flush pending.
    fn needs_flush(&mut self) -> bool {
        let sendable =
            self.task.is_some() && (self.display.is_some() || self.size.is_some() || self.remove);
        let post = sendable && !self.flushing;
        self.flushing |= post;
        post
    }

    /// The flush's next report; None ends the flush (a later change posts a new one).
    fn next(&mut self) -> Option<Report> {
        let report = self.take();
        self.flushing = report.is_some();
        report
    }
}

/// The window's Task reports: stored by the window's thread, sent on the outbox.
pub struct TaskReports {
    pending: Mutex<Pending>,
    outbox: Arc<dyn LooperPoster>,
}

impl TaskReports {
    /// The handle the window submits values to and the UI agent names the Task to; transactions run on `outbox`.
    pub fn new(outbox: Arc<dyn LooperPoster>) -> Arc<Self> {
        Arc::new(TaskReports {
            pending: Mutex::new(Pending::default()),
            outbox,
        })
    }

    /// The window's Task (the UI agent's TASK). A number different from the one already named is refused and logged; the same number again changes nothing.
    pub fn set_task(self: &Arc<Self>, task: i32) {
        self.change(|pending| match pending.task {
            Some(current) if current != task => {
                tracing::warn!(
                    "atl-composer: task reports: task {task} refused: the window is task {current}"
                );
            }
            _ => pending.task = Some(task),
        });
    }

    /// A size change on the window's thread: replaces the pending size, so a size overtaken before it was sent is never sent.
    pub fn submit_size(self: &Arc<Self>, size: PhysSize) {
        self.change(|p| p.size = Some(size));
    }

    /// The window's monitor changed: replaces the pending display. The services ignore a display equal to theirs (tasks.rs `set_display`).
    pub fn submit_display(self: &Arc<Self>, display: DisplayModel) {
        self.change(|p| p.display = Some(display));
    }

    /// The desktop asked to close the window: the Task's removal goes after the pending reports.
    pub fn submit_remove_task(self: &Arc<Self>) {
        self.change(|p| p.remove = true);
    }

    fn change(self: &Arc<Self>, edit: impl FnOnce(&mut Pending)) {
        let post = {
            let mut pending = self.lock();
            edit(&mut pending);
            pending.needs_flush()
        };
        if post {
            let reports = Arc::clone(self);
            self.outbox.post(Box::new(move || reports.flush()));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        // a poisoned state still holds the pending values
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// On the outbox: sends until nothing is left, the lock released during each transaction.
    fn flush(&self) {
        loop {
            let Some(report) = self.lock().next() else {
                return;
            };
            if let Err(e) = send(&report) {
                tracing::warn!("atl-composer: task reports: {e}");
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
        Report::RemoveTask(task) => {
            data.write(task).map_err(|e| e.to_string())?;
            tracing::info!("atl-composer: task reports: REMOVE_TASK task {task}");
            REMOVE_TASK
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
        let first = DisplayModel::from_monitor(1920, 1080, None, 1.0, 0);
        let second = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6, 0);
        p.size = Some(size(800, 600));
        p.display = Some(first);
        p.display = Some(second);
        assert!(p.take().is_none());
        p.task = Some(3);
        assert!(matches!(p.take(), Some(Report::Display(d)) if d == second));
        assert!(matches!(p.take(), Some(Report::Size(3, s)) if s == size(800, 600)));
        assert!(p.take().is_none());
    }

    /// A close waits for the Task, goes after a pending size, and two requests before the send are one removal.
    #[test]
    fn remove_task_goes_last_once() {
        let mut p = Pending {
            remove: true,
            size: Some(size(800, 600)),
            ..Pending::default()
        };
        assert!(p.take().is_none());
        p.task = Some(3);
        p.remove = true;
        assert!(matches!(p.take(), Some(Report::Size(3, _))));
        assert!(matches!(p.take(), Some(Report::RemoveTask(3))));
        assert!(p.take().is_none());
    }

    /// One flush per burst: nothing is posted before the Task, the first sendable change posts, later ones join that flush; the flush ending (nothing left) lets the next change post again.
    #[test]
    fn a_burst_posts_one_flush() {
        let mut p = Pending::default();
        p.size = Some(size(800, 600));
        assert!(!p.needs_flush(), "no Task: nothing to send");
        p.task = Some(3);
        assert!(p.needs_flush(), "the Task named: the first flush");
        p.size = Some(size(801, 600));
        assert!(!p.needs_flush(), "a flush is pending");
        assert!(matches!(p.next(), Some(Report::Size(3, s)) if s == size(801, 600)));
        assert!(p.next().is_none());
        p.remove = true;
        assert!(p.needs_flush(), "the flush ended: a new change posts again");
    }
}
