// SPDX-License-Identifier: MIT
//! The alarms the `alarm` service holds (alarm.rs): what AlarmManagerService keeps per set alarm, for matching and removal. Nothing is delivered yet, so no clock is converted and no quota, idle or batching rule applies.
//! Matching is AlarmManagerService's: an alarm with an operation matches that operation (PendingIntent identity), one without matches its listener (Alarm.java:171-175). A set replaces the alarm already held for its key (setImplLocked:2271).
//! Generic over the key types, so the tests run without the binder; alarm.rs keys it by the operation's and the listener's binders.

/// One set alarm: the arguments of AlarmManagerService's set (:2684-2688), the creator uid and package, and the keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Alarm<O, L> {
    /// the creator uid (`creatorUid`) and the package it was set for (`sourcePackage`)
    pub uid: u32,
    pub package: String,
    /// AlarmManager's type, trigger and window in its own clock (unconverted), the interval (0 = one-shot) and the flags, as set
    pub kind: i32,
    pub trigger: i64,
    pub window: i64,
    pub interval: i64,
    pub flags: i32,
    pub operation: Option<O>,
    pub listener: Option<L>,
}

/// The alarms in set order. A key holds at most one alarm: a set replaces the one it matches.
#[derive(Debug)]
pub struct AlarmTable<O, L> {
    alarms: Vec<Alarm<O, L>>,
}

impl<O, L> Default for AlarmTable<O, L> {
    fn default() -> Self {
        AlarmTable { alarms: Vec::new() }
    }
}

impl<O: PartialEq, L: PartialEq> AlarmTable<O, L> {
    /// Alarms held.
    pub fn len(&self) -> usize {
        self.alarms.len()
    }

    /// No alarm held.
    pub fn is_empty(&self) -> bool {
        self.alarms.is_empty()
    }

    /// setImplLocked (:2257-2271): the alarm held for this operation or listener goes first, then this one is added.
    pub fn set(&mut self, alarm: Alarm<O, L>) {
        self.remove(alarm.operation.as_ref(), alarm.listener.as_ref());
        self.alarms.push(alarm);
    }

    /// removeLocked (:3989-3997): drops each alarm the operation or the listener matches; with neither given, none. Returns how many were dropped.
    pub fn remove(&mut self, operation: Option<&O>, listener: Option<&L>) -> usize {
        let before = self.alarms.len();
        self.alarms.retain(|a| !a.matches(operation, listener));
        before - self.alarms.len()
    }

    /// removeAll (:2911-2927): drops the alarms `package` set under `uid`. Returns how many were dropped.
    pub fn remove_all(&mut self, uid: u32, package: &str) -> usize {
        let before = self.alarms.len();
        self.alarms.retain(|a| a.uid != uid || a.package != package);
        before - self.alarms.len()
    }
}

impl<O: PartialEq, L: PartialEq> Alarm<O, L> {
    /// Alarm.matches (Alarm.java:171-175): an alarm with an operation matches only that operation; one without matches its listener. An alarm with neither matches nothing (AOSP would throw).
    fn matches(&self, operation: Option<&O>, listener: Option<&L>) -> bool {
        match (&self.operation, &self.listener) {
            (Some(op), _) => operation == Some(op),
            (None, Some(l)) => listener == Some(l),
            (None, None) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys are plain numbers here: the table never looks inside them.
    fn alarm(uid: u32, package: &str, op: Option<u32>, listener: Option<u32>) -> Alarm<u32, u32> {
        Alarm {
            uid,
            package: package.to_owned(),
            kind: 0,
            trigger: 0,
            window: 0,
            interval: 0,
            flags: 0,
            operation: op,
            listener,
        }
    }

    /// set, then remove by the same operation: one alarm dropped, the table empty.
    #[test]
    fn set_then_remove_empties_the_table() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.len(), 1);
        assert_eq!(t.remove(Some(&1), None), 1);
        assert!(t.is_empty());
    }

    /// A second set for the same operation replaces the first (setImplLocked removes the match before it adds).
    #[test]
    fn same_operation_replaces_the_alarm() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.len(), 1);
    }

    /// A listener alarm matches its own listener only: another operation or another listener drops nothing.
    #[test]
    fn listener_alarm_matches_its_listener_only() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", None, Some(7)));
        assert_eq!(t.remove(Some(&1), None), 0);
        assert_eq!(t.remove(None, Some(&8)), 0);
        assert_eq!(t.remove(None, Some(&7)), 1);
        assert!(t.is_empty());
    }

    /// Neither key given: nothing matches (removeLocked returns early).
    #[test]
    fn remove_without_a_key_drops_nothing() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        assert_eq!(t.remove(None, None), 0);
        assert_eq!(t.len(), 1);
    }

    /// removeAll drops the package's alarms set under the caller's uid only: two of three here.
    #[test]
    fn remove_all_is_per_package_and_uid() {
        let mut t = AlarmTable::default();
        t.set(alarm(10123, "com.example", Some(1), None));
        t.set(alarm(10123, "com.example", Some(2), None));
        t.set(alarm(10124, "com.example", Some(3), None));
        t.set(alarm(10123, "com.other", Some(4), None));
        assert_eq!(t.remove_all(10123, "com.example"), 2);
        assert_eq!(t.len(), 2);
    }
}
