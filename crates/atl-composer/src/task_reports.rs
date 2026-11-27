// SPDX-License-Identifier: MIT
//! What the window reports to the services about its Task (compositor_registry; tasks.rs there): its content size as the Task's bounds (`SET_BOUNDS`) and the monitor it is on (`REPORT_MONITOR`).
//! The monitor's reply is the display the services answer with; this process's compositor takes it as its display ([`Composer::set_display`]). The registry needs only the app's uid for a monitor (the Task exists from process start), so a monitor goes at once; the size and the removal need the Task's number and wait for it.
//! The platform reports at each change, many in a burst during a drag: only the latest of each is kept and sent, so a burst costs one transaction, not one per step. The monitor goes before the size: a size measured on the new monitor belongs to the new display.
//! The window's thread only stores the values. A transaction is a blocking binder call and runs on the outbox (outbox.rs): one flush is posted when a value arrives and none is pending, so the toolkit's loop never stalls.
//! One window per process, so one Task: a different number named later is refused, and nothing is ever sent under the first Task's number for it.
//! The desktop closing the window is `REMOVE_TASK`, the Shell's removeTask: sent after any pending size, once per close request.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::host::Composer;
use crate::start::IRegistry;
use atl_platform_ui::{LooperPoster, PhysSize, Scale};
use atl_services_types::compositor_registry::{REMOVE_TASK, REPORT_MONITOR, SERVICE, SET_BOUNDS};
use atl_services_types::display_model::DisplayModel;
use binder::binder_impl::IBinderInternal;
use binder::{SpIBinder, Strong};

/// The Task the window is (none until the services name it), the latest values not yet sent, and whether a flush is already posted.
#[derive(Default)]
struct Pending {
    task: Option<i32>,
    /// The content size and the window scale it was measured at; converted to the display's pixels only when sent ([`Measured::at`]).
    size: Option<Measured>,
    monitor: Option<DisplayModel>,
    /// The desktop asked to close the window; requests before the send coalesce into one.
    remove: bool,
    /// A flush is posted to the outbox and has not yet found nothing left to send.
    flushing: bool,
    /// The desktop answered the window's first commit (`Window::on_placed`): any monitor it entered then is in `monitor` or sent.
    placed: bool,
    /// A monitor is taken and its answer not yet adopted.
    monitor_sending: bool,
}

/// A size as the window measured it: physical px at the window's scale then. Before the window's opening is granted that scale is the opening's (TASK_INFO, the display at process start),
/// which on another monitor's density is not the display the services answer for the window's monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Measured {
    px: PhysSize,
    scale: Scale,
}

impl Measured {
    /// The size in `display`'s pixels: the logical size (dp) the window has, at the display's scale. The display is the composer's, adopted from the services' monitor answer, so a size sent
    /// after that answer is in the window's monitor's pixels whatever scale it was measured at. break `size-at-measured-scale`: the measured px as they are.
    fn at(self, display: &DisplayModel) -> PhysSize {
        let Some(to) = crate::start::window_scale(display.dpi) else {
            return self.px;
        };
        if cfg!(feature = "break-size-at-measured-scale") || to == self.scale {
            return self.px;
        }
        let side = |v: u32| to.to_physical(self.scale.to_logical(f64::from(v))).round() as u32;
        PhysSize {
            width: side(self.px.width),
            height: side(self.px.height),
        }
    }
}

/// One report, taken from [`Pending`].
enum Report {
    Monitor(DisplayModel),
    Size(i32, Measured),
    RemoveTask(i32),
}

impl Pending {
    /// The next report to send: the monitor first (it needs no Task number), then the size and the removal once the Task is named.
    fn take(&mut self) -> Option<Report> {
        if let Some(monitor) = self.take_monitor() {
            self.monitor_sending = true;
            return Some(Report::Monitor(monitor));
        }
        let task = self.task?;
        if let Some(size) = self.size.take() {
            return Some(Report::Size(task, size));
        }
        std::mem::take(&mut self.remove).then_some(Report::RemoveTask(task))
    }

    /// Whether the pending monitor may go now. break `bind-before-placed` (with start.rs not waiting): it waits for the Task (named at the first window add), so the app binds on the start
    /// monitor by construction and the report relaunches it.
    fn monitor_sendable(&self) -> bool {
        self.monitor.is_some()
            && !(cfg!(feature = "break-bind-before-placed") && self.task.is_none())
    }

    /// The window's display is settled for binding: the first commit answered, and the monitor it entered (if any) sent and its answer adopted.
    fn settled(&self) -> bool {
        self.placed && self.monitor.is_none() && !self.monitor_sending
    }

    fn take_monitor(&mut self) -> Option<DisplayModel> {
        if self.monitor_sendable() {
            self.monitor.take()
        } else {
            None
        }
    }

    /// Whether a change needs a flush posted: something is sendable and no flush is pending. Marks the flush pending.
    fn needs_flush(&mut self) -> bool {
        let sendable = self.monitor_sendable()
            || (self.task.is_some() && (self.size.is_some() || self.remove));
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

/// The window's Task reports: stored by the window's thread, sent on the outbox. A monitor's reply becomes `composer`'s display.
pub struct TaskReports {
    pending: Mutex<Pending>,
    /// Notified at each change of `pending` that may settle it ([`TaskReports::wait_settled`]).
    settled: Condvar,
    outbox: Arc<dyn LooperPoster>,
    composer: Arc<Composer>,
}

impl TaskReports {
    /// The handle the window submits values to and the UI agent names the Task to; transactions run on `outbox`, and `composer` takes the display the services answer with.
    pub fn new(outbox: Arc<dyn LooperPoster>, composer: Arc<Composer>) -> Arc<Self> {
        Arc::new(TaskReports {
            pending: Mutex::new(Pending::default()),
            settled: Condvar::new(),
            outbox,
            composer,
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

    /// A size change on the window's thread, `size` in physical px at the window's `scale`: replaces the pending size, so a size overtaken before it was sent is never sent.
    pub fn submit_size(self: &Arc<Self>, size: PhysSize, scale: Scale) {
        self.change(|p| p.size = Some(Measured { px: size, scale }));
    }

    /// The window's monitor changed: replaces the pending monitor. The services answer with their display, which the composer adopts after the send.
    pub fn submit_monitor(self: &Arc<Self>, monitor: DisplayModel) {
        self.change(|p| p.monitor = Some(monitor));
    }

    /// The desktop asked to close the window: the Task's removal goes after the pending reports.
    pub fn submit_remove_task(self: &Arc<Self>) {
        self.change(|p| p.remove = true);
    }

    /// The window's first commit was answered (`Window::on_placed`, after any monitor it entered was submitted).
    pub fn placed(self: &Arc<Self>) {
        self.change(|p| p.placed = true);
    }

    /// On the launcher thread before REGISTER (start.rs; an app thread, display-thread rule B1): returns once the window's display is settled, so the app binds on it. A window on no monitor
    /// at its first commit (a hidden workspace) settles at once on the services' current display; its later enter is an ordinary monitor report (a configuration change). No timer: the
    /// desktop answers a commit in one roundtrip. break `bind-before-placed`: returns at once.
    pub fn wait_settled(&self) {
        if cfg!(feature = "break-bind-before-placed") {
            return;
        }
        let mut pending = self.lock();
        while !pending.settled() {
            pending = self
                .settled
                .wait(pending)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    fn change(self: &Arc<Self>, edit: impl FnOnce(&mut Pending)) {
        let post = {
            let mut pending = self.lock();
            edit(&mut pending);
            self.settled.notify_all();
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

    /// On the outbox: sends until nothing is left, the pending lock released during each transaction. A monitor's answer is adopted before the wait it settles is woken.
    fn flush(&self) {
        loop {
            let Some(report) = self.lock().next() else {
                return;
            };
            self.deliver(&report);
            if matches!(report, Report::Monitor(_)) {
                self.lock().monitor_sending = false;
                self.settled.notify_all();
            }
        }
    }

    /// One report sent, a size in the composer's display's pixels at the send; a monitor's answer becomes the composer's display.
    fn deliver(&self, report: &Report) {
        match send(report, &self.composer.display()) {
            Ok(Some(answer)) => {
                if self.composer.set_display(answer) {
                    tracing::info!("atl-composer: ui: display {}", answer.to_arg());
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("atl-composer: task reports: {e}"),
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

/// One transaction: REPORT_MONITOR with the monitor, whose reply is the display the services answer with (`Some`); SET_BOUNDS with the Task's number, then the size in `display`'s pixels; or REMOVE_TASK.
fn send(report: &Report, display: &DisplayModel) -> Result<Option<DisplayModel>, String> {
    let binder = registry()?;
    let mut data = binder.prepare_transact().map_err(|e| e.to_string())?;
    let code = match report {
        Report::Monitor(monitor) => {
            data.write(&monitor.to_arg()).map_err(|e| e.to_string())?;
            REPORT_MONITOR
        }
        Report::Size(task, measured) => {
            let size = measured.at(display);
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
    let reply = binder
        .submit_transact(code, data, 0)
        .map_err(|e| format!("transaction {code}: {e}"))?;
    // the window ime stage's outbox sub-gate: every report is sent from the outbox thread (outbox.rs `THREAD`)
    let thread = std::thread::current();
    tracing::info!(
        "atl-composer: task reports: sent {code} thread={}",
        thread.name().unwrap_or("?")
    );
    let Report::Monitor(monitor) = report else {
        return Ok(None);
    };
    let text: String = reply
        .read()
        .map_err(|e| format!("transaction {code} reply: {e}"))?;
    let answered =
        DisplayModel::parse_arg(&text).map_err(|e| format!("transaction {code} reply: {e}"))?;
    // break `declared-adopt`: the composer keeps the monitor it reported, so its SurfaceFlinger display is not the services' DisplayInfo
    Ok(Some(if cfg!(feature = "break-declared-adopt") {
        *monitor
    } else {
        answered
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Measured {
        Measured {
            px: PhysSize { width, height },
            scale: Scale::ONE,
        }
    }

    /// A size measured at the opening scale (1, the start display) is sent in the display's px: 2160x1350 dp at 256 dpi (1.6) = 3456x2160, by hand; at 160 dpi it stays 2160x1350.
    /// 1800x1125 px at 1.6 = 1125x703.125 dp -> 1125x703 at 160 dpi; unchanged at 256 (same scale, no dp round trip).
    #[test]
    fn a_size_goes_in_the_display_pixels() {
        let at_dpi = |dpi| DisplayModel {
            dpi,
            ..DisplayModel::default()
        };
        let opening = size(2160, 1350);
        assert_eq!(
            opening.at(&at_dpi(256)),
            PhysSize {
                width: 3456,
                height: 2160
            }
        );
        assert_eq!(
            opening.at(&at_dpi(160)),
            PhysSize {
                width: 2160,
                height: 1350
            }
        );
        let fractional = Measured {
            px: PhysSize {
                width: 1800,
                height: 1125,
            },
            scale: Scale::new(1.6).unwrap(),
        };
        assert_eq!(fractional.at(&at_dpi(256)), fractional.px);
        assert_eq!(
            fractional.at(&at_dpi(160)),
            PhysSize {
                width: 1125,
                height: 703
            }
        );
    }

    /// The monitor goes without the Task (the registry checks the uid's Task only); the size waits for the Task. Each goes once, the latest: the monitor before the size.
    #[test]
    fn the_monitor_goes_first_and_needs_no_task() {
        let mut p = Pending::default();
        let first = DisplayModel::from_monitor(1920, 1080, None, 1.0, 0);
        let second = DisplayModel::from_monitor(2560, 1600, Some(120.0), 1.6, 0);
        p.size = Some(size(800, 600));
        p.monitor = Some(first);
        p.monitor = Some(second);
        assert!(matches!(p.take(), Some(Report::Monitor(d)) if d == second));
        assert!(p.take().is_none(), "the size waits for the Task");
        p.task = Some(3);
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

    /// One flush per burst: nothing is posted for a size before the Task, the first sendable change posts, later ones join that flush; the flush ending (nothing left) lets the next change post again.
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

    /// A monitor posts a flush with no Task named: the registry takes it from the uid alone.
    #[test]
    fn a_monitor_posts_before_the_task_is_named() {
        let mut p = Pending::default();
        p.monitor = Some(DisplayModel::from_monitor(1920, 1080, None, 1.0, 0));
        assert!(p.needs_flush(), "a monitor needs no Task");
        assert!(matches!(p.next(), Some(Report::Monitor(_))));
        assert!(p.next().is_none());
    }
}
