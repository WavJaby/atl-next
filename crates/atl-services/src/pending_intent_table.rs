// SPDX-License-Identifier: MIT
//! The PendingIntent records the activity service keeps (PendingIntentController.java): one per PendingIntentRecord.Key, handed out as its IIntentSender binder (pending_intent.rs).
//! Same key -> same record -> same binder: PendingIntent.equals compares the binder, and AlarmManager matches an alarm by it (alarm_table.rs).
//! Key.equals (PendingIntentRecord.java:191-238): type, user, package, feature, activity token (binder identity), who, requestCode, the request intent by filterEquals (broadcast::IntentKey), its resolved type, and the flags left once the lookup flags are cleared. Options and the other intents of an array are not part of it.
//! Generic over the binder type, so the tests run without binder.

use crate::broadcast::IntentKey;

/// PendingIntentRecord.Key. `flags` are the sent flags less the lookup flags (FLAG_NO_CREATE, CANCEL_CURRENT, UPDATE_CURRENT), which getIntentSender reads and clears (PendingIntentController.java:152-156).
#[derive(Clone)]
pub struct Key<B> {
    /// the PendingIntent kind: ActivityManager.INTENT_SENDER_*
    pub r#type: i32,
    pub package: String,
    pub feature_id: Option<String>,
    /// the activity token, compared by binder identity
    pub activity: Option<B>,
    pub who: Option<String>,
    pub request_code: i32,
    /// the request intent: the last of the sent array (PendingIntentRecord.java:160)
    pub intent: Option<IntentKey>,
    pub resolved_type: Option<String>,
    pub flags: i32,
    pub user_id: i32,
}

/// A record: its key, the creator's uid (PendingIntentRecord.uid) and the binder it is handed out as.
pub struct Record<B> {
    pub key: Key<B>,
    pub uid: u32,
    pub binder: B,
}

/// What getIntentSender does with an equal record (PendingIntentController.java:165-188).
#[derive(Clone, Copy, Default)]
pub struct Lookup {
    /// FLAG_NO_CREATE: no record is created
    pub no_create: bool,
    /// FLAG_CANCEL_CURRENT: an equal record is cancelled, and a new one replaces it
    pub cancel_current: bool,
}

/// The records in creation order. An equal key never holds two.
pub struct PendingIntentTable<B> {
    records: Vec<Record<B>>,
}

impl<B> Default for PendingIntentTable<B> {
    fn default() -> Self {
        PendingIntentTable {
            records: Vec::new(),
        }
    }
}

impl<B: Clone + PartialEq> PendingIntentTable<B> {
    /// Records held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// No record held.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// getIntentSender (PendingIntentController.java:162-193). An equal record's binder comes back unchanged: FLAG_UPDATE_CURRENT's extras are send-time data, not modelled (:169-179). FLAG_CANCEL_CURRENT cancels the equal record and `make` gives the new one. With FLAG_NO_CREATE and no record, None; with FLAG_NO_CREATE and a record it cancels, AOSP returns that cancelled record (:187-188), and so does this.
    pub fn get_or_create(
        &mut self,
        key: Key<B>,
        uid: u32,
        lookup: Lookup,
        make: impl FnOnce() -> B,
    ) -> Option<B> {
        let mut cancelled = None;
        if let Some(i) = self.records.iter().position(|r| r.key.same_as(&key)) {
            if !lookup.cancel_current {
                return Some(self.records[i].binder.clone());
            }
            cancelled = Some(self.records.remove(i).binder);
        }
        if lookup.no_create {
            return cancelled;
        }
        let binder = make();
        self.records.push(Record {
            key,
            uid,
            binder: binder.clone(),
        });
        Some(binder)
    }

    /// The record whose binder is `binder`.
    pub fn find(&self, binder: &B) -> Option<&Record<B>> {
        self.records.iter().find(|r| &r.binder == binder)
    }

    /// Drops the record whose binder is `binder` (cancelIntentSender, PendingIntentController.java:285-289), and returns it.
    pub fn remove(&mut self, binder: &B) -> Option<Record<B>> {
        let i = self.records.iter().position(|r| &r.binder == binder)?;
        Some(self.records.remove(i))
    }
}

impl<B: PartialEq> Key<B> {
    /// Key.equals (PendingIntentRecord.java:191-238): the intent by filterEquals, the activity by binder identity, the rest by value.
    fn same_as(&self, other: &Key<B>) -> bool {
        self.r#type == other.r#type
            && self.user_id == other.user_id
            && self.package == other.package
            && self.feature_id == other.feature_id
            && self.activity == other.activity
            && self.who == other.who
            && self.request_code == other.request_code
            && match (&self.intent, &other.intent) {
                (Some(a), Some(b)) => a.filter_equals(b),
                (None, None) => true,
                _ => false,
            }
            && self.resolved_type == other.resolved_type
            && self.flags == other.flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A key of package `com.example` for request `code`, with the request intent `action`. Binders are numbers here.
    fn key(code: i32, action: &str) -> Key<u32> {
        Key {
            r#type: 1,
            package: "com.example".to_owned(),
            feature_id: None,
            activity: None,
            who: None,
            request_code: code,
            intent: Some(IntentKey {
                action: Some(action.to_owned()),
                ..IntentKey::default()
            }),
            resolved_type: None,
            flags: 0,
            user_id: 0,
        }
    }

    /// A binder for a new record: the number of records made so far.
    fn next(made: &Cell<u32>) -> u32 {
        made.set(made.get() + 1);
        made.get()
    }

    /// The same key twice: one record, the second call returns its binder and makes none (PendingIntentController.java:165-181).
    #[test]
    fn same_key_gives_the_same_binder() {
        let mut t = PendingIntentTable::default();
        let made = Cell::new(0);
        assert_eq!(
            t.get_or_create(key(1, "A"), 10123, Lookup::default(), || next(&made)),
            Some(1)
        );
        assert_eq!(
            t.get_or_create(key(1, "A"), 10123, Lookup::default(), || next(&made)),
            Some(1)
        );
        assert_eq!((made.get(), t.len()), (1, 1));
    }

    /// The request code is part of the key (Key.equals :216-218): another code is another record.
    #[test]
    fn request_code_is_part_of_the_key() {
        let mut t = PendingIntentTable::default();
        assert_eq!(
            t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1),
            Some(1)
        );
        assert_eq!(
            t.get_or_create(key(2, "A"), 10123, Lookup::default(), || 2),
            Some(2)
        );
        assert_eq!(t.len(), 2);
    }

    /// The request intent compares by filterEquals (:219-227): another action, or no intent, is another record.
    #[test]
    fn intent_is_part_of_the_key() {
        let mut t = PendingIntentTable::default();
        t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1);
        assert_eq!(
            t.get_or_create(key(1, "B"), 10123, Lookup::default(), || 2),
            Some(2)
        );
        let mut none = key(1, "A");
        none.intent = None;
        assert_eq!(
            t.get_or_create(none, 10123, Lookup::default(), || 3),
            Some(3)
        );
        assert_eq!(t.len(), 3);
    }

    /// FLAG_NO_CREATE gives nothing until a record exists, then that record's binder (:187-188 before creation, :181 after).
    #[test]
    fn no_create_is_none_until_a_record_exists() {
        let mut t = PendingIntentTable::default();
        let no_create = Lookup {
            no_create: true,
            cancel_current: false,
        };
        assert_eq!(t.get_or_create(key(1, "A"), 10123, no_create, || 1), None);
        assert!(t.is_empty());
        assert_eq!(
            t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1),
            Some(1)
        );
        assert_eq!(
            t.get_or_create(key(1, "A"), 10123, no_create, || 2),
            Some(1)
        );
        assert_eq!(t.len(), 1);
    }

    /// FLAG_CANCEL_CURRENT: the old record goes and the key gets a new binder; the old binder is no longer held (:183-186).
    #[test]
    fn cancel_current_replaces_the_binder() {
        let mut t = PendingIntentTable::default();
        t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1);
        let cancel = Lookup {
            no_create: false,
            cancel_current: true,
        };
        assert_eq!(t.get_or_create(key(1, "A"), 10123, cancel, || 2), Some(2));
        assert!(t.find(&1).is_none());
        assert_eq!(t.find(&2).map(|r| r.uid), Some(10123));
        assert_eq!(t.len(), 1);
    }

    /// FLAG_NO_CREATE with FLAG_CANCEL_CURRENT cancels the record and still returns its binder (:183-188).
    #[test]
    fn cancel_current_with_no_create_returns_the_cancelled_binder() {
        let mut t = PendingIntentTable::default();
        t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1);
        let both = Lookup {
            no_create: true,
            cancel_current: true,
        };
        assert_eq!(t.get_or_create(key(1, "A"), 10123, both, || 2), Some(1));
        assert!(t.is_empty());
    }

    /// cancelIntentSender removes the record by its binder, once: the second removal finds none (:285-289).
    #[test]
    fn remove_drops_the_record_once() {
        let mut t = PendingIntentTable::default();
        t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1);
        assert_eq!(t.remove(&1).map(|r| r.uid), Some(10123));
        assert!(t.remove(&1).is_none());
        assert!(t.is_empty());
    }

    /// The activity token (binder identity), the kind, the resolved type, the flags left after masking, and the user are each part of the key (Key.equals :198-233).
    #[test]
    fn token_kind_type_flags_and_user_are_part_of_the_key() {
        let mut t = PendingIntentTable::default();
        t.get_or_create(key(1, "A"), 10123, Lookup::default(), || 1);
        let mut token = key(1, "A");
        token.activity = Some(7);
        let mut kind = key(1, "A");
        kind.r#type = 2;
        let mut typed = key(1, "A");
        typed.resolved_type = Some("text/plain".to_owned());
        let mut one_shot = key(1, "A");
        one_shot.flags = 1 << 30; // FLAG_ONE_SHOT, PendingIntent.java:218
        let mut user = key(1, "A");
        user.user_id = 10;
        assert_eq!(
            t.get_or_create(token, 10123, Lookup::default(), || 2),
            Some(2)
        );
        assert_eq!(
            t.get_or_create(kind, 10123, Lookup::default(), || 3),
            Some(3)
        );
        assert_eq!(
            t.get_or_create(typed, 10123, Lookup::default(), || 4),
            Some(4)
        );
        assert_eq!(
            t.get_or_create(one_shot, 10123, Lookup::default(), || 5),
            Some(5)
        );
        assert_eq!(
            t.get_or_create(user, 10123, Lookup::default(), || 6),
            Some(6)
        );
        assert_eq!(t.len(), 6);
    }
}
