// SPDX-License-Identifier: MIT
//! The registry's table of app UI: each app process's UI agent (the IUiAgent binder REGISTER hands in, compositor_registry.rs; the interface is atl-services-wire `ui_agent`) and its compositor.
//!
//! Keyed per process, never per uid: every process of an app runs its own compositor (launcher step 5b'), and a uid's processes (`android:process`, e.g. `:push`) register each their own. AOSP keys
//! the same state by the calling process: WindowManagerService.openSession makes one Session per caller recording `Binder.getCallingPid()` and the uid (Session.java:130-141), and that session's
//! windows and surfaces belong to that process; InputMethodManagerService keeps one ClientState per client binder, with its uid and pid. Per-uid state (permissions, app-ops, Task ownership) stays
//! per uid elsewhere.
//!
//! Where the pid comes from: binder's calling pid ([`AppProcess::calling`]), never a pid a caller passes. The services run in the host's pid namespace (atl-binderd `PidNs::Host`), so each app
//! sandbox's process is seen by its host pid (the broker's tag, binderd `Tag::host_pid`): one pid namespace per sandbox, so two processes of one uid are two sandboxes with two host pids, distinct
//! while both live. A host pid can be reused after its process died; the death link on each entry ([`UiAgents::insert`]) removes the dead process's entry, so a reused pid never reaches a dead agent.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use binder::{DeathRecipient, IBinder, SpIBinder, StatusCode, ThreadState};

/// A binder caller's process: its uid and the pid binder reports for it (see the module doc for why that pid identifies one process).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AppProcess {
    pub uid: u32,
    pub pid: i32,
}

impl AppProcess {
    /// The process of the binder call being served. Only for a synchronous call: a oneway call carries pid 0 (binder.c sets sender_pid only with a reply thread, binderd likewise), so a oneway
    /// method finds its process another way (activity.rs: onBackPressed by the Task).
    pub fn calling() -> Self {
        Self {
            uid: ThreadState::get_calling_uid(),
            pid: ThreadState::get_calling_pid(),
        }
    }
}

impl std::fmt::Display for AppProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uid {} pid {}", self.uid, self.pid)
    }
}

/// One registered process: its agent and compositor, the registration's number (a death of an older registration of the same process leaves a newer one) and the death link.
struct Entry {
    agent: SpIBinder,
    composer: SpIBinder,
    registration: u64,
    /// Held only so the link lives as long as the entry. None for an agent in the services' own process (not remote: it cannot die before the table).
    _death: Option<DeathRecipient>,
}

#[derive(Default)]
struct Table {
    entries: HashMap<AppProcess, Entry>,
    registrations: u64,
}

/// The registered app processes' UI agents and compositors, by [`AppProcess`]. A process that registers again replaces its own entry only.
#[derive(Default)]
pub struct UiAgents {
    table: Mutex<Table>,
}

impl UiAgents {
    /// Keeps `process`'s agent and compositor, linked to the agent's death (the entry goes with the process). Fails when the link cannot be made (the agent is already dead).
    pub fn insert(
        self: &Arc<Self>,
        process: AppProcess,
        mut agent: SpIBinder,
        composer: SpIBinder,
    ) -> Result<(), StatusCode> {
        let registration = {
            let mut t = self.lock();
            t.registrations += 1;
            t.registrations
        };
        let death = if agent.is_remote() {
            let table: Weak<Self> = Arc::downgrade(self);
            let mut death = DeathRecipient::new(move || {
                if let Some(table) = table.upgrade() {
                    table.died(process, registration);
                }
            });
            agent.link_to_death(&mut death)?;
            Some(death)
        } else {
            None
        };
        self.lock().entries.insert(
            process,
            Entry {
                agent,
                composer,
                registration,
                _death: death,
            },
        );
        Ok(())
    }

    /// The agent `process` registered.
    pub fn agent(&self, process: AppProcess) -> Option<SpIBinder> {
        self.lock().entries.get(&process).map(|e| e.agent.clone())
    }

    /// The compositor `process` registered.
    pub fn composer(&self, process: AppProcess) -> Option<SpIBinder> {
        self.lock()
            .entries
            .get(&process)
            .map(|e| e.composer.clone())
    }

    /// The agent of registration `registration` of `process` died: its entry goes, unless the process registered again since. Logged with what is left.
    fn died(&self, process: AppProcess, registration: u64) {
        let mut t = self.lock();
        if !t
            .entries
            .get(&process)
            .is_some_and(|e| e.registration == registration)
        {
            return;
        }
        t.entries.remove(&process);
        let left = t.entries.len();
        drop(t);
        let _ = writeln!(
            std::io::stderr(),
            "services: UI agent of {process} died; entry removed, {left} left"
        );
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        // a poisoned table still holds the agents
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local binder standing for an agent or a compositor: any object with its own identity.
    fn binder() -> SpIBinder {
        crate::ui_agents::tests_support::local_binder()
    }

    const UID: u32 = 10_003;
    const A: AppProcess = AppProcess { uid: UID, pid: 200 };
    const B: AppProcess = AppProcess { uid: UID, pid: 300 };

    #[test]
    fn each_process_of_a_uid_keeps_its_own_entry_in_either_order() {
        for a_first in [true, false] {
            let t = Arc::new(UiAgents::default());
            let (agent_a, comp_a, agent_b, comp_b) = (binder(), binder(), binder(), binder());
            let mut order = vec![(A, &agent_a, &comp_a), (B, &agent_b, &comp_b)];
            if !a_first {
                order.reverse();
            }
            for (p, agent, comp) in order {
                t.insert(p, agent.clone(), comp.clone()).expect("insert");
            }
            assert_eq!(t.agent(A), Some(agent_a.clone()), "a_first={a_first}");
            assert_eq!(t.composer(A), Some(comp_a.clone()), "a_first={a_first}");
            assert_eq!(t.agent(B), Some(agent_b.clone()), "a_first={a_first}");
            assert_eq!(t.composer(B), Some(comp_b.clone()), "a_first={a_first}");
            // a third process of the uid that never registered gets nothing, not a sibling's agent
            assert_eq!(t.agent(AppProcess { uid: UID, pid: 400 }), None);
        }
    }

    #[test]
    fn a_death_removes_only_its_process_and_only_its_registration() {
        let t = Arc::new(UiAgents::default());
        let agent_a = binder();
        t.insert(A, agent_a.clone(), binder()).expect("insert");
        t.insert(B, binder(), binder()).expect("insert");
        // registrations count from 1 in insert order: A is 1, B is 2
        t.died(B, 2);
        assert_eq!(t.agent(A), Some(agent_a));
        assert_eq!(t.agent(B), None);
        // A registers again (registration 3): the death of its first registration leaves the new one
        let again = binder();
        t.insert(A, again.clone(), binder()).expect("insert");
        t.died(A, 1);
        assert_eq!(t.agent(A), Some(again));
        t.died(A, 3);
        assert_eq!(t.agent(A), None);
    }
}

/// Local binders for the services' unit tests: objects with their own identity, in this process.
#[cfg(test)]
pub(crate) mod tests_support {
    use atl_services_wire::ui_agent::{BnUiAgent, IUiAgent};
    use binder::{BinderFeatures, Interface, SpIBinder, StatusCode};
    use std::os::fd::OwnedFd;
    use std::sync::{Arc, Mutex};

    use atl_parcelables::android::view::{SurfaceControl, SurfaceControl_Transaction};

    /// An IUiAgent that records the window surfaces asked of it and refuses them (no compositor behind it).
    #[derive(Default)]
    pub(crate) struct RecordingAgent {
        pub(crate) surfaces: Arc<Mutex<Vec<String>>>,
    }

    impl Interface for RecordingAgent {}

    impl IUiAgent for RecordingAgent {
        fn window_surface(&self, layer_name: &str) -> Result<SurfaceControl, StatusCode> {
            self.surfaces
                .lock()
                .expect("surfaces")
                .push(layer_name.to_owned());
            Err(StatusCode::INVALID_OPERATION)
        }
        fn input_server(&self, _: SpIBinder, _: OwnedFd, _: i32) -> Result<(), StatusCode> {
            Ok(())
        }
        fn remove_input(&self, _: SpIBinder) -> Result<(), StatusCode> {
            Ok(())
        }
        fn input_frame(&self, _: SpIBinder, _: [i32; 4]) -> Result<(), StatusCode> {
            Ok(())
        }
        fn task(&self, _: i32) -> Result<(), StatusCode> {
            Ok(())
        }
        fn pointer_icon(&self, _: i32) -> Result<(), StatusCode> {
            Ok(())
        }
        fn minimize(&self) -> Result<(), StatusCode> {
            Ok(())
        }
        fn task_title(&self, _: Option<&str>, _: &str, _: &str) -> Result<(), StatusCode> {
            Ok(())
        }
        fn window_position(&self, _: i32, _: i32, _: i32) -> Result<(), StatusCode> {
            Ok(())
        }
        fn input_method(&self) -> Result<Option<SpIBinder>, StatusCode> {
            Ok(None)
        }
        fn post_draw_transaction(&self, _: &SurfaceControl_Transaction) -> Result<(), StatusCode> {
            Ok(())
        }
        fn dim_layer(&self, _: i32, _: f32, _: i32, _: i32) -> Result<(), StatusCode> {
            Ok(())
        }
    }

    /// A recording agent as a local binder, with its record.
    pub(crate) fn recording_agent() -> (SpIBinder, Arc<Mutex<Vec<String>>>) {
        let agent = RecordingAgent::default();
        let surfaces = Arc::clone(&agent.surfaces);
        (
            BnUiAgent::new_binder(agent, BinderFeatures::default()).as_binder(),
            surfaces,
        )
    }

    pub(crate) fn local_binder() -> SpIBinder {
        recording_agent().0
    }
}
