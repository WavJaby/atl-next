// SPDX-License-Identifier: MIT
//! The one appId allocator: AOSP's AppIdSettingMap (frameworks/base services/core/java/com/android/server/pm/AppIdSettingMap.java) for application ids. System and /data packages share one
//! pool; an id is handed out as `acquireAndRegisterNewAppId` does -- the lowest unregistered id >= `mFirstAvailableAppId` (FIRST_APPLICATION_UID at start), none past LAST_APPLICATION_UID.
//! `removeSetting` frees an id but raises `mFirstAvailableAppId` to id + 1, so within one run a freed id below that mark is not handed out again (a later boot, rebuilding from packages.xml, may).
//! Our installer's uid ([`atl_broker::INSTALLER_UID`]) is registered at construction like any taken id, not skipped by a second rule.

use atl_services_types::permission::{FIRST_APPLICATION_UID, LAST_APPLICATION_UID};

/// Registered application ids (AppIdSettingMap's mNonSystemSettings, index = id - FIRST_APPLICATION_UID) and the first id a new acquire may take.
pub struct AppIds {
    taken: Vec<bool>,
    first_available: u32,
}

impl Default for AppIds {
    fn default() -> Self {
        Self::new()
    }
}

impl AppIds {
    /// The pool at start: only the installer's id taken.
    pub fn new() -> Self {
        let mut ids = Self {
            taken: Vec::new(),
            first_available: FIRST_APPLICATION_UID,
        };
        let installer = ids.register(atl_broker::INSTALLER_UID);
        debug_assert!(installer, "fresh pool");
        ids
    }

    /// registerExistingAppId: false when `id` is not an application id or is already registered.
    pub fn register(&mut self, id: u32) -> bool {
        let Some(i) = Self::index(id) else {
            return false;
        };
        if i >= self.taken.len() {
            self.taken.resize(i + 1, false);
        }
        !std::mem::replace(&mut self.taken[i], true)
    }

    /// acquireAndRegisterNewAppId: the new id, or None when the pool is exhausted.
    pub fn acquire(&mut self) -> Option<u32> {
        let from = Self::index(self.first_available)?;
        let i = match self.taken.iter().skip(from).position(|t| !t) {
            Some(free) => from + free,
            None => self.taken.len().max(from),
        };
        let id = FIRST_APPLICATION_UID + u32::try_from(i).ok()?;
        self.register(id).then_some(id)
    }

    /// removeSetting: frees `id`; the next acquire starts above it (setFirstAvailableAppId).
    pub fn release(&mut self, id: u32) {
        if let Some(t) = Self::index(id).and_then(|i| self.taken.get_mut(i)) {
            *t = false;
        }
        self.first_available = self.first_available.max(id + 1);
    }

    fn index(id: u32) -> Option<usize> {
        (FIRST_APPLICATION_UID..=LAST_APPLICATION_UID)
            .contains(&id)
            .then(|| (id - FIRST_APPLICATION_UID) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected ids follow from the rule alone: pool 10000..=19999 (Process.java), 10000 = the installer's (atl_broker::INSTALLER_UID), lowest free first.

    #[test]
    fn first_ids_skip_only_the_installer() {
        let mut ids = AppIds::new();
        assert_eq!(ids.acquire(), Some(10_001));
        assert_eq!(ids.acquire(), Some(10_002));
    }

    #[test]
    fn a_registered_id_is_taken_and_a_gap_below_it_is_filled() {
        let mut ids = AppIds::new();
        assert!(ids.register(10_003));
        assert!(!ids.register(10_003), "duplicate refused");
        assert!(!ids.register(1_000), "not an application id");
        assert_eq!(ids.acquire(), Some(10_001));
        assert_eq!(ids.acquire(), Some(10_002));
        assert_eq!(ids.acquire(), Some(10_004));
    }

    /// AOSP removeSetting: the freed id is not reused in the same run; ids above it are.
    #[test]
    fn released_id_is_not_reused_within_a_run() {
        let mut ids = AppIds::new();
        let a = ids.acquire().unwrap(); // 10001
        let b = ids.acquire().unwrap(); // 10002
        ids.release(a);
        assert_eq!(ids.acquire(), Some(10_003));
        ids.release(b);
        assert_eq!(ids.acquire(), Some(10_004));
    }

    #[test]
    fn exhausted_pool_gives_none() {
        let mut ids = AppIds::new();
        // 10000 taken + 9999 acquires fill 10000..=19999
        for want in 10_001..=19_999 {
            assert_eq!(ids.acquire(), Some(want));
        }
        assert_eq!(ids.acquire(), None);
    }

    /// Break check: an allocator that hands out a taken id (here the installer's) fails here, since `register` refuses it and `acquire` would return None or a duplicate.
    #[test]
    fn never_hands_out_a_taken_id() {
        let mut ids = AppIds::new();
        let mut seen = std::collections::BTreeSet::from([atl_broker::INSTALLER_UID]);
        for _ in 0..100 {
            let id = ids.acquire().unwrap();
            assert!(seen.insert(id), "id {id} handed out twice");
        }
    }
}
