// SPDX-License-Identifier: MIT
//! The compositor registry service (compositor_registry.rs is the protocol). REGISTER takes the app's UI agent (ui_agent.rs), kept under the caller's uid in the table the window service reads:
//! constructor injection into [`Registry`], so the table is not part of the interface.

use std::io::Write;
use std::sync::Arc;

use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{
    BinderFeatures, Interface, SpIBinder, StatusCode, ThreadState, declare_binder_interface,
};

use crate::activity::ActivityManager;
use crate::compositor_registry::{DESCRIPTOR, REGISTER, SET_BOUNDS, SET_DISPLAY, TASK_INFO};
use crate::counters;
use crate::desktop_bounds::ActivityInput;
use crate::display::DisplayCallbacks;
use crate::display_model::DisplayModel;
use crate::tasks::{TaskId, TaskSize, Tasks, initial_size};
use crate::ui_agent::UiAgents;

/// The interface name counters use.
const INTERFACE: &str = "ICompositorRegistry";

/// TASK_INFO's reply: the window's opening size and its Task's package (empty for a process with no Task).
pub struct TaskInfo {
    pub size: TaskSize,
    pub package: String,
}

pub trait ICompositorRegistry: Interface {
    /// REGISTER once its argument is read: the agent is kept under `caller_uid`.
    fn register(&self, caller_uid: u32, ui_agent: SpIBinder) -> Result<(), StatusCode>;

    /// SET_BOUNDS once its arguments are read: Task `task` (its number, tasks.rs) is `width` x `height` physical pixels. The caller's uid must own the Task; a non-positive size is refused; a size change notifies the Task's windows.
    fn set_bounds(
        &self,
        caller_uid: u32,
        task: i32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode>;

    /// SET_DISPLAY once its argument is read: the monitor in the `--display` form. The caller's uid must own a Task; a malformed form is refused; a change reaches every app (registry `display_changed`).
    fn set_display(&self, caller_uid: u32, display: &str) -> Result<(), StatusCode>;

    /// TASK_INFO once the caller's pid is read: the size the process's window opens at and its Task's package (compositor_registry.rs TASK_INFO).
    fn task_info(&self, caller_pid: i32) -> Result<TaskInfo, StatusCode>;
}

declare_binder_interface! {
    ICompositorRegistry[DESCRIPTOR] {
        native: BnCompositorRegistry(on_transact),
        proxy: BpCompositorRegistry,
    }
}

/// REGISTER: the UI agent; the calling uid is taken from the transaction.
fn on_transact(
    service: &dyn ICompositorRegistry,
    code: TransactionCode,
    data: &BorrowedParcel<'_>,
    reply: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    if code == SET_BOUNDS {
        let task: i32 = data.read()?;
        let width: i32 = data.read()?;
        let height: i32 = data.read()?;
        return service.set_bounds(ThreadState::get_calling_uid(), task, width, height);
    }
    if code == SET_DISPLAY {
        let display: String = data.read()?;
        return service.set_display(ThreadState::get_calling_uid(), &display);
    }
    if code == TASK_INFO {
        let info = service.task_info(ThreadState::get_calling_pid())?;
        reply.write(&info.size.width)?;
        reply.write(&info.size.height)?;
        return reply.write(&info.package);
    }
    if code != REGISTER {
        counters::count_unimplemented(INTERFACE, &format!("code {code}"));
        return Err(StatusCode::UNKNOWN_TRANSACTION);
    }
    let ui_agent: SpIBinder = data.read()?;
    service.register(ThreadState::get_calling_uid(), ui_agent)
}

impl ICompositorRegistry for BpCompositorRegistry {
    fn set_bounds(&self, _: u32, _: i32, _: i32, _: i32) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }

    fn set_display(&self, _: u32, _: &str) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }

    fn task_info(&self, _: i32) -> Result<TaskInfo, StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }

    fn register(&self, _: u32, _: SpIBinder) -> Result<(), StatusCode> {
        Err(StatusCode::INVALID_OPERATION)
    }
}

impl ICompositorRegistry for Binder<BnCompositorRegistry> {
    fn set_bounds(
        &self,
        caller_uid: u32,
        task: i32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode> {
        self.0.set_bounds(caller_uid, task, width, height)
    }

    fn set_display(&self, caller_uid: u32, display: &str) -> Result<(), StatusCode> {
        self.0.set_display(caller_uid, display)
    }

    fn task_info(&self, caller_pid: i32) -> Result<TaskInfo, StatusCode> {
        self.0.task_info(caller_pid)
    }

    fn register(&self, caller_uid: u32, ui_agent: SpIBinder) -> Result<(), StatusCode> {
        self.0.register(caller_uid, ui_agent)
    }
}

struct Registry {
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
    /// the activities of the Tasks: a size change reaches their app threads (activity.rs task_resized)
    activities: ActivityManager,
    /// the display service's callbacks: a display change reaches them (display.rs)
    display_callbacks: Arc<DisplayCallbacks>,
}

impl Interface for Registry {}

impl Registry {
    /// `task`'s configuration changed (its size or the display): its activities get the change (activity.rs task_resized), then its windows `resized`, unless a relaunch holds them.
    fn task_changed(&self, task: TaskId) {
        self.activities.task_resized(task);
        if self.activities.task_relaunching(task) {
            let _ = writeln!(
                std::io::stderr(),
                "services: resize of task {} held for the relaunch",
                task.raw()
            );
            return;
        }
        let windows = crate::window::notify_resized(&self.tasks, task);
        let _ = writeln!(
            std::io::stderr(),
            "services: resized {windows} windows of task {}",
            task.raw()
        );
    }
}

impl ICompositorRegistry for Registry {
    fn task_info(&self, caller_pid: i32) -> Result<TaskInfo, StatusCode> {
        Ok(match self.activities.task_of_process(caller_pid) {
            Some(task) => TaskInfo {
                size: self.tasks.size(task).ok_or(StatusCode::BAD_VALUE)?,
                package: self.tasks.package(task).ok_or(StatusCode::BAD_VALUE)?,
            },
            None => TaskInfo {
                size: initial_size(&self.tasks.display(), &ActivityInput::no_activity()),
                package: String::new(),
            },
        })
    }

    fn set_bounds(
        &self,
        caller_uid: u32,
        task: i32,
        width: i32,
        height: i32,
    ) -> Result<(), StatusCode> {
        // an app sizes its own Tasks only: the caller's uid must own the one it names
        let Some(task) =
            TaskId::from_raw(task).filter(|t| self.tasks.owner(*t) == Some(caller_uid))
        else {
            let _ = writeln!(
                std::io::stderr(),
                "services: bounds refused uid {caller_uid}: task {task} is not its"
            );
            return Err(StatusCode::PERMISSION_DENIED);
        };
        if width <= 0 || height <= 0 {
            let _ = writeln!(
                std::io::stderr(),
                "services: bounds refused task {}: {width}x{height}",
                task.raw()
            );
            return Err(StatusCode::BAD_VALUE);
        }
        if self.tasks.set_size(task, TaskSize { width, height }) {
            let _ = writeln!(
                std::io::stderr(),
                "services: task {} of uid {caller_uid} bounds {width}x{height}",
                task.raw()
            );
            self.task_changed(task);
        }
        Ok(())
    }

    fn set_display(&self, caller_uid: u32, display: &str) -> Result<(), StatusCode> {
        // only an app with a window reports its monitor
        if !self.tasks.owns_any(caller_uid) {
            let _ = writeln!(
                std::io::stderr(),
                "services: display refused uid {caller_uid}: owns no task"
            );
            return Err(StatusCode::PERMISSION_DENIED);
        }
        let model = DisplayModel::parse_arg(display).map_err(|why| {
            let _ = writeln!(std::io::stderr(), "services: display refused: {why}");
            StatusCode::BAD_VALUE
        })?;
        let tasks = self.tasks.set_display(model);
        if tasks.is_empty() {
            return Ok(());
        }
        let _ = writeln!(
            std::io::stderr(),
            "services: display {display} from uid {caller_uid}"
        );
        // AOSP's order: DisplayManagerService delivers the display event, then WindowManager's display change updates the global configuration (ActivityTaskManagerService.updateGlobalConfigurationLocked:4813: ConfigurationChangeItem to each process), then each activity's
        let callbacks = self.display_callbacks.display_changed();
        let processes = self.activities.global_configuration_changed();
        let _ = writeln!(
            std::io::stderr(),
            "services: display change told {callbacks} display callbacks, {processes} processes"
        );
        for task in tasks {
            self.task_changed(task);
        }
        Ok(())
    }

    fn register(&self, caller_uid: u32, ui_agent: SpIBinder) -> Result<(), StatusCode> {
        self.agents.insert(caller_uid, ui_agent);
        let _ = writeln!(
            std::io::stderr(),
            "services: registered the UI agent of uid {caller_uid}"
        );
        Ok(())
    }
}

/// A new registry binder. `agents` is the table the window service reads (system_services.rs gives both the same one); `activities` is the service a Task's size change reaches.
pub fn new_binder(
    agents: Arc<UiAgents>,
    tasks: Arc<Tasks<SpIBinder>>,
    activities: ActivityManager,
    display_callbacks: Arc<DisplayCallbacks>,
) -> SpIBinder {
    BnCompositorRegistry::new_binder(
        Registry {
            agents,
            tasks,
            activities,
            display_callbacks,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
