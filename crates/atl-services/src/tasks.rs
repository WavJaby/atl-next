// SPDX-License-Identifier: MIT
//! The Tasks of the one display (plan `Lifecycle / windows`: freeform, one Task = one toplevel window). A Task's size is its toplevel's content size in physical pixels
//! (`Task.setBounds`, Task.java:2786; the compositor's configure plays the Shell's `wct.setBounds`, VeiledResizeTaskPositioner.java:188). The display ([`DisplayModel`]) is the monitor:
//! a window's size never changes it. The table also keeps the IWindow client of each window of a Task, so a size change reaches them (window.rs `notify_resized`).
//! A Task is keyed by the caller's uid, as the UI agents are (ui_agent.rs): one app process per uid until activity tokens key Tasks.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use binder::SpIBinder;

use crate::display_model::DisplayModel;

/// A Task's size: the toplevel's content size in physical pixels. The origin is not kept: the compositor places the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSize {
    pub width: i32,
    pub height: i32,
}

/// One window of a Task: the client binder (what `remove` finds the window by) and the Task it belongs to.
struct WindowEntry {
    uid: u32,
    client: SpIBinder,
}

#[derive(Default)]
struct State {
    /// Only the sizes that were set: a Task without one has the display's size.
    sizes: BTreeMap<u32, TaskSize>,
    windows: Vec<WindowEntry>,
    /// The last `ClientWindowFrames.seq` handed out, global across windows.
    seq: i32,
}

/// The Tasks of the display: sizes by uid and the windows of each.
pub struct Tasks {
    display: DisplayModel,
    state: Mutex<State>,
}

impl Tasks {
    /// `display` is the monitor. A Task with no size yet has the display's size: its window opens at it.
    pub fn new(display: DisplayModel) -> Self {
        Tasks {
            display,
            state: Mutex::new(State::default()),
        }
    }

    /// The display (monitor values): unchanged by any window's size.
    pub fn display(&self) -> DisplayModel {
        self.display
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // a poisoned table still holds the sizes and windows
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn display_size(&self) -> TaskSize {
        TaskSize {
            width: self.display.width,
            height: self.display.height,
        }
    }

    /// The size of `uid`'s Task: the one it was set to, else the display's.
    pub fn size(&self, uid: u32) -> TaskSize {
        self.lock()
            .sizes
            .get(&uid)
            .copied()
            .unwrap_or_else(|| self.display_size())
    }

    /// Sets `uid`'s Task size; true when it changed, so its windows need `resized`. The last size set is the one kept.
    pub fn set_size(&self, uid: u32, size: TaskSize) -> bool {
        let mut st = self.lock();
        let current = st
            .sizes
            .get(&uid)
            .copied()
            .unwrap_or_else(|| self.display_size());
        if current == size {
            return false;
        }
        st.sizes.insert(uid, size);
        true
    }

    /// Records a window of `uid`'s Task; a client already recorded is not recorded twice.
    pub fn add_window(&self, uid: u32, client: SpIBinder) {
        let mut st = self.lock();
        if !st.windows.iter().any(|w| w.client == client) {
            st.windows.push(WindowEntry { uid, client });
        }
    }

    /// Forgets the window whose client is `client` (removeWindow).
    pub fn remove_window(&self, client: &SpIBinder) {
        self.lock().windows.retain(|w| w.client != *client);
    }

    /// The clients of `uid`'s windows, in the order they were added.
    pub fn windows_of(&self, uid: u32) -> Vec<SpIBinder> {
        self.lock()
            .windows
            .iter()
            .filter(|w| w.uid == uid)
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

    /// The display the tests use: DisplayModel's default, 1280x800.
    fn tasks() -> Tasks {
        Tasks::new(DisplayModel::default())
    }

    /// A Task with no size has the display's size. A set size is kept per uid, and no resize changes the display.
    #[test]
    fn sizes_are_per_uid_and_default_to_the_display() {
        let t = tasks();
        assert_eq!(
            t.size(10_123),
            TaskSize {
                width: 1280,
                height: 800
            }
        );
        assert!(t.set_size(
            10_123,
            TaskSize {
                width: 352,
                height: 272
            }
        ));
        assert!(t.set_size(
            10_124,
            TaskSize {
                width: 288,
                height: 208
            }
        ));
        assert_eq!(
            t.size(10_123),
            TaskSize {
                width: 352,
                height: 272
            }
        );
        assert_eq!(
            t.size(10_124),
            TaskSize {
                width: 288,
                height: 208
            }
        );
        assert_eq!(t.display(), DisplayModel::default());
    }

    /// Setting the size a Task already has is no change; each change replaces the size, so the last one set is the one kept.
    #[test]
    fn set_size_reports_changes_and_keeps_the_last() {
        let t = tasks();
        assert!(t.set_size(
            7,
            TaskSize {
                width: 1000,
                height: 700
            }
        ));
        assert!(t.set_size(
            7,
            TaskSize {
                width: 900,
                height: 650
            }
        ));
        assert!(t.set_size(
            7,
            TaskSize {
                width: 800,
                height: 600
            }
        ));
        assert!(!t.set_size(
            7,
            TaskSize {
                width: 800,
                height: 600
            }
        ));
        assert_eq!(
            t.size(7),
            TaskSize {
                width: 800,
                height: 600
            }
        );
        // back to the display's size is a change: the entry is kept, not dropped
        assert!(t.set_size(
            7,
            TaskSize {
                width: 1280,
                height: 800
            }
        ));
        assert_eq!(
            t.size(7),
            TaskSize {
                width: 1280,
                height: 800
            }
        );
    }

    /// Frame sequence numbers only go up.
    #[test]
    fn sequence_numbers_increase() {
        let t = tasks();
        assert_eq!(t.next_seq(), 1);
        assert_eq!(t.next_seq(), 2);
    }
}
