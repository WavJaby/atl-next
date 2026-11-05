// SPDX-License-Identifier: MIT
//! The Tasks of the one display (plan `Lifecycle / windows`: freeform, one Task = one toplevel window). A Task's size is its toplevel's content size in physical pixels (`Task.setBounds`, Task.java:2786; the compositor's configure plays the Shell's `wct.setBounds`, VeiledResizeTaskPositioner.java:188). The display ([`DisplayModel`]) is the monitor: a window's size never changes it.
//! A Task is keyed by its [`TaskId`], which the services assign when the launch that makes the Task attaches its process (activity.rs). An activity's token names its Task (ActivityRecord.task), and a window names its Task by its `attrs.token` (window.rs). The owner uid is kept for the checks the registry makes (registry_service.rs), never as a key.
//! The table also keeps the IWindow client of each window of a Task, so a size change reaches them (window.rs `notify_resized`).
//! The table is generic over the handle `C` (the services use SpIBinder): handles are cloned and compared by equality, so the table and its tests build without the converter feature, which the test step needs.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use crate::display_model::DisplayModel;

/// A Task's identity, assigned by the services (positive). The wire carries it as an i32: compositor_registry SET_BOUNDS and the IUiAgent TASK transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(i32);

impl TaskId {
    /// The number the wire carries.
    pub fn raw(self) -> i32 {
        self.0
    }

    /// The Task a wire number names; None for a number no Task has (zero or negative).
    pub fn from_raw(raw: i32) -> Option<Self> {
        (raw > 0).then_some(TaskId(raw))
    }
}

/// A Task's size: the toplevel's content size in physical pixels. The origin is not kept: the compositor places the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSize {
    pub width: i32,
    pub height: i32,
}

/// What the services keep per Task: the uid of the app it belongs to (checked by the registry), and its size once one was set (only the sizes that were set are kept: a Task without one has the display's size).
struct TaskRecord {
    owner: u32,
    size: Option<TaskSize>,
    /// The seq of the override this size makes (config_seq).
    config_seq: i32,
}

/// One window of a Task: the client handle (what `remove` finds the window by) and the Task it belongs to.
struct WindowEntry<C> {
    task: TaskId,
    client: C,
}

struct State<C> {
    /// the monitor the app windows are on (one display, plan `Lifecycle / windows`)
    display: DisplayModel,
    tasks: BTreeMap<TaskId, TaskRecord>,
    /// the last TaskId handed out
    last_id: i32,
    /// WindowToken -> Task (an activity token: ActivityRecord.task; a toast token: the caller's Task). A token is an identity: compared by equality.
    tokens: Vec<(C, TaskId)>,
    windows: Vec<WindowEntry<C>>,
    /// The last `ClientWindowFrames.seq` handed out, global across windows.
    seq: i32,
}

impl<C> State<C> {
    fn new(display: DisplayModel) -> Self {
        State {
            display,
            tasks: BTreeMap::new(),
            last_id: 0,
            tokens: Vec::new(),
            windows: Vec::new(),
            seq: 0,
        }
    }
}

/// The Tasks of the display: their owners and sizes, the window tokens bound to each, and the windows of each. `C` is the handle an activity or window is known by.
pub struct Tasks<C> {
    state: Mutex<State<C>>,
}

/// A new Task's size on `display` ([`DisplayModel::opening_size`]).
fn opening(display: &DisplayModel) -> TaskSize {
    let (width, height) = display.opening_size();
    TaskSize { width, height }
}

/// The config_seq after `seq`: positive, as a numbered override must be (0 = never changed).
fn next_config_seq(seq: i32) -> i32 {
    seq.wrapping_add(1).max(1)
}

impl<C: Clone + PartialEq> Tasks<C> {
    /// `display` is the monitor at start. A Task with no size yet has the display's opening size: its window opens at it.
    pub fn new(display: DisplayModel) -> Self {
        Tasks {
            state: Mutex::new(State::new(display)),
        }
    }

    /// The display now (the monitor's values, [`Tasks::set_display`]): unchanged by any window's size. The one owner: every service reads it here, none keeps a copy.
    pub fn display(&self) -> DisplayModel {
        self.lock().display
    }

    fn lock(&self) -> MutexGuard<'_, State<C>> {
        // a poisoned table still holds the tasks and windows
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new Task of the app `owner` (its uid). The launch that starts a Task calls it once.
    pub fn new_task(&self, owner: u32) -> TaskId {
        let mut st = self.lock();
        st.last_id = st.last_id.checked_add(1).expect("TaskId space exhausted");
        let id = TaskId(st.last_id);
        st.tasks.insert(
            id,
            TaskRecord {
                owner,
                size: None,
                config_seq: 0,
            },
        );
        id
    }

    /// The uid of the app `task` belongs to; None for no Task.
    pub fn owner(&self, task: TaskId) -> Option<u32> {
        self.lock().tasks.get(&task).map(|t| t.owner)
    }

    /// Whether the app `owner` has a Task: only such an app reports the monitor (registry SET_DISPLAY).
    pub fn owns_any(&self, owner: u32) -> bool {
        self.lock().tasks.values().any(|t| t.owner == owner)
    }

    /// The size of `task`: the one it was set to, else the initial size (a Task that does not exist has it too).
    pub fn size(&self, task: TaskId) -> TaskSize {
        let st = self.lock();
        st.tasks
            .get(&task)
            .and_then(|t| t.size)
            .unwrap_or_else(|| opening(&st.display))
    }

    /// Sets `task`'s size; true when it changed, so its windows need `resized`. False for the size it already has and for no Task (the registry checks the owner first).
    pub fn set_size(&self, task: TaskId, size: TaskSize) -> bool {
        let mut st = self.lock();
        let initial = opening(&st.display);
        let Some(record) = st.tasks.get_mut(&task) else {
            return false;
        };
        if record.size.unwrap_or(initial) == size {
            return false;
        }
        record.size = Some(size);
        record.config_seq = next_config_seq(record.config_seq);
        true
    }

    /// The window's monitor changed (size, density or refresh rate: DisplayModel::from_monitor): it becomes the display. Every Task's configuration follows it, so each gets a new config_seq; returns those Tasks, empty when nothing changed.
    /// A Task with no size set keeps its opening size from the old display: its window is already open at that size, and only a SET_BOUNDS changes it.
    pub fn set_display(&self, display: DisplayModel) -> Vec<TaskId> {
        let mut st = self.lock();
        if st.display == display {
            return Vec::new();
        }
        let old = opening(&st.display);
        st.display = display;
        st.tasks
            .iter_mut()
            .map(|(id, record)| {
                record.size.get_or_insert(old);
                record.config_seq = next_config_seq(record.config_seq);
                *id
            })
            .collect()
    }

    /// The Configuration.seq of `task`'s override (ActivityRecord.java:7697): numbered per size, 0 until the first change. Every copy of one override carries it: the item, IWindow.resized, relayout. They race on the app's main thread (AOSP orders them), and a seq-0 copy reads as older than a pending numbered one, so ActivityThread drops it and a layout runs on the old configuration; an item at seq 0 fares no better: handleActivityConfigurationChangedInner:7112 takes its own pending copy for newer and drops it.
    pub fn config_seq(&self, task: TaskId) -> i32 {
        self.lock().tasks.get(&task).map_or(0, |r| r.config_seq)
    }

    /// Names `task` as the Task of the WindowToken `token`: an activity's (ActivityRecord.task; ActivityRecord extends WindowToken) or a toast's (the notification service's
    /// WindowManagerInternal.addWindowToken(TYPE_TOAST), placed in the caller's Task). A token bound again takes the new Task.
    pub fn bind_token(&self, token: C, task: TaskId) {
        let mut st = self.lock();
        match st.tokens.iter_mut().find(|(t, _)| *t == token) {
            Some(entry) => entry.1 = task,
            None => st.tokens.push((token, task)),
        }
    }

    /// Forgets the WindowToken `token` (WindowManagerInternal.removeWindowToken): a window added with it later is refused.
    pub fn unbind_token(&self, token: &C) {
        self.lock().tokens.retain(|(t, _)| t != token);
    }

    /// The newest Task of the app `owner` (its uid): where a toast of that app shows. None when it has none.
    pub fn newest_task_of(&self, owner: u32) -> Option<TaskId> {
        self.lock()
            .tasks
            .iter()
            .rev()
            .find(|(_, t)| t.owner == owner)
            .map(|(id, _)| *id)
    }

    /// The Task of the WindowToken `token`; None for no token, or one never bound (a window with it is refused).
    pub fn task_of_token(&self, token: Option<&C>) -> Option<TaskId> {
        let token = token?;
        self.lock()
            .tokens
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, task)| *task)
    }

    /// Records a window of `task`; a client already recorded is not recorded twice.
    pub fn add_window(&self, task: TaskId, client: C) {
        let mut st = self.lock();
        if !st.windows.iter().any(|w| w.client == client) {
            st.windows.push(WindowEntry { task, client });
        }
    }

    /// Forgets the window whose client is `client` (removeWindow).
    pub fn remove_window(&self, client: &C) {
        self.lock().windows.retain(|w| w.client != *client);
    }

    /// The clients of `task`'s windows, in the order they were added.
    pub fn windows_of(&self, task: TaskId) -> Vec<C> {
        self.lock()
            .windows
            .iter()
            .filter(|w| w.task == task)
            .map(|w| w.client.clone())
            .collect()
    }

    /// The next frame sequence number: each `resized` carries a new one.
    pub fn next_seq(&self) -> i32 {
        let mut st = self.lock();
        st.seq = st.seq.wrapping_add(1);
        st.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in handle: the table only compares and clones them.
    type Handle = u32;

    /// The display the tests use: DisplayModel's default, 1280x800.
    fn tasks() -> Tasks<Handle> {
        Tasks::new(DisplayModel::default())
    }

    /// What a Task without a size has: 3/4 of each side of the 1280x800 display (AOSP desktop default bounds, DesktopModeUtils.kt:49-56).
    fn initial_size() -> TaskSize {
        TaskSize {
            width: 960,
            height: 600,
        }
    }

    /// AOSP truncates each scaled side (`.toInt()`): 1366 x 0.75 = 1024.5 -> 1024, 2560x1600 -> 1920x1200.
    #[test]
    fn initial_size_is_three_quarters_truncated() {
        let at = |width, height| {
            Tasks::<Handle>::new(DisplayModel {
                width,
                height,
                ..DisplayModel::default()
            })
            .size(TaskId(1))
        };
        assert_eq!(at(1366, 768), size(1024, 576));
        assert_eq!(at(2560, 1600), size(1920, 1200));
    }

    fn size(width: i32, height: i32) -> TaskSize {
        TaskSize { width, height }
    }

    /// Two Tasks of one app hold their own bounds: both start at the initial size, each keeps the size set for it, and no resize changes the display.
    #[test]
    fn tasks_of_one_uid_hold_their_own_bounds() {
        let t = tasks();
        let (main, second) = (t.new_task(10_123), t.new_task(10_123));
        assert_ne!(main, second);
        assert_eq!(t.owner(main), Some(10_123));
        assert_eq!(t.owner(second), Some(10_123));
        assert_eq!(t.size(main), initial_size());
        assert_eq!(t.size(second), initial_size());
        assert!(t.set_size(main, size(352, 272)));
        assert!(t.set_size(second, size(288, 208)));
        assert_eq!(t.size(main), size(352, 272));
        assert_eq!(t.size(second), size(288, 208));
        assert_eq!(t.display(), DisplayModel::default());
    }

    /// Setting the size a Task already has is no change; each change replaces the size, so the last one set is the one kept. Back to the initial size is a change: the entry is kept, not dropped.
    #[test]
    fn set_size_reports_changes_and_keeps_the_last() {
        let t = tasks();
        let task = t.new_task(7);
        assert!(t.set_size(task, size(1000, 700)));
        assert!(t.set_size(task, size(900, 650)));
        assert!(t.set_size(task, size(800, 600)));
        assert!(!t.set_size(task, size(800, 600)));
        assert_eq!(t.size(task), size(800, 600));
        assert!(t.set_size(task, initial_size()));
        assert_eq!(t.size(task), initial_size());
        assert!(!t.set_size(task, initial_size()));
    }

    /// One number per size: 0 before the first change, up by one at each change, not at a size it already has; none for no Task.
    #[test]
    fn config_seq_follows_the_size_changes() {
        let t = tasks();
        let task = t.new_task(7);
        assert_eq!(t.config_seq(task), 0);
        t.set_size(task, size(1000, 700));
        assert_eq!(t.config_seq(task), 1);
        t.set_size(task, size(1000, 700));
        assert_eq!(t.config_seq(task), 1);
        t.set_size(task, size(900, 650));
        assert_eq!(t.config_seq(task), 2);
    }

    /// A new monitor becomes the display and renumbers every Task's override; the same monitor again changes nothing. A Task never sized keeps the opening size it opened at (960x600: 3/4 of the default 1280x800), not the new display's.
    #[test]
    fn set_display_renumbers_every_task_and_keeps_open_sizes() {
        let t = tasks();
        let sized = t.new_task(7);
        let fresh = t.new_task(8);
        t.set_size(sized, size(1000, 700));
        let monitor = DisplayModel::from_monitor(1920, 1080, Some(60.0), 2.0);
        assert_eq!(t.set_display(monitor), vec![sized, fresh]);
        assert_eq!(t.display(), monitor);
        assert_eq!((t.config_seq(sized), t.config_seq(fresh)), (2, 1));
        assert_eq!(t.size(sized), size(1000, 700));
        assert_eq!(t.size(fresh), size(960, 600));
        assert_eq!(t.set_display(monitor), Vec::new());
        assert_eq!(t.config_seq(sized), 2);
    }

    /// A Task that does not exist has no owner, takes no size, and has the display's size.
    #[test]
    fn unknown_task_is_refused() {
        let t = tasks();
        let ghost = TaskId::from_raw(99).expect("positive");
        assert_eq!(t.owner(ghost), None);
        assert!(!t.set_size(ghost, size(352, 272)));
        assert_eq!(t.size(ghost), initial_size());
    }

    /// The wire form: zero and negative numbers name no Task; a Task's number round-trips.
    #[test]
    fn task_numbers_round_trip_and_refuse_non_positive() {
        let t = tasks();
        let task = t.new_task(7);
        assert_eq!(TaskId::from_raw(0), None);
        assert_eq!(TaskId::from_raw(-1), None);
        assert_eq!(TaskId::from_raw(task.raw()), Some(task));
    }

    /// An activity's token names its Task: the bound token finds it, no token and an unbound token find none; a token bound again takes the new Task.
    #[test]
    fn activity_tokens_name_their_task() {
        let t = tasks();
        let (first, second) = (t.new_task(7), t.new_task(7));
        t.bind_token(1, first);
        assert_eq!(t.task_of_token(Some(&1)), Some(first));
        assert_eq!(t.task_of_token(None), None);
        assert_eq!(t.task_of_token(Some(&2)), None);
        t.bind_token(1, second);
        assert_eq!(t.task_of_token(Some(&1)), Some(second));
    }

    /// A window belongs to the Task it was added under: two Tasks of one app list their own windows; a window added twice is kept once; a removed one is gone.
    #[test]
    fn windows_belong_to_their_task() {
        let t = tasks();
        let (main, second) = (t.new_task(7), t.new_task(7));
        t.add_window(main, 100);
        t.add_window(second, 200);
        t.add_window(main, 100);
        assert_eq!(t.windows_of(main), vec![100]);
        assert_eq!(t.windows_of(second), vec![200]);
        t.remove_window(&100);
        assert!(t.windows_of(main).is_empty());
        assert_eq!(t.windows_of(second), vec![200]);
    }

    /// Frame sequence numbers only go up.
    #[test]
    fn sequence_numbers_increase() {
        let t = tasks();
        assert_eq!(t.next_seq(), 1);
        assert_eq!(t.next_seq(), 2);
    }
}
