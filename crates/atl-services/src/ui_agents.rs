// SPDX-License-Identifier: MIT
//! The registry's table of app UI agents (the IUiAgent binders REGISTER hands in, compositor_registry.rs); the interface itself is atl-services-wire `ui_agent`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use binder::SpIBinder;

/// The UI agents the registry holds, by uid. One app per uid: a process that registers again replaces its uid's entry (a restarted app).
#[derive(Default)]
pub struct UiAgents {
    agents: Mutex<HashMap<u32, SpIBinder>>,
}

impl UiAgents {
    pub fn insert(&self, uid: u32, agent: SpIBinder) {
        self.lock().insert(uid, agent);
    }

    /// The agent registered for `uid`.
    pub fn get(&self, uid: u32) -> Option<SpIBinder> {
        self.lock().get(&uid).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u32, SpIBinder>> {
        // a poisoned table still holds the agents
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }
}
