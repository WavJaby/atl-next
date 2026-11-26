// SPDX-License-Identifier: MIT
//! AudioService's side of the volume-change seam (atl-services-wire `audio_listener`, A23 slice 5): the app processes' listeners, keyed per process ((uid, pid)) as AudioPolicyService.mNotificationClients is,
//! and the commit that tells them a stream was written.
//! A commit goes to the listeners of the written uid only. A listener whose delivery fails is dropped: its process is gone. Each listener is linked to its binder's death; a death removes its own registration only,
//! so a reused pid registering again is not removed by the old process's death (ui_agents precedent: registration numbers).
//! Which commits publish: the written stream's own step or mute change (AudioPolicyManager.cpp:3875 publishes the group after a write). Alias members move with it inside AudioService and publish nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use atl_services_wire::audio_listener::{
    BnAudioVolumeRegistry, IAudioVolumeListener, IAudioVolumeRegistry,
};
use binder::{
    BinderFeatures, DeathRecipient, IBinder, Interface, SpIBinder, StatusCode, Strong, ThreadState,
};

use crate::volume::Changes;

/// The interface name counters use.
const INTERFACE: &str = "IAudioVolumeRegistry";

/// One listener's delivery: false when the listener is gone (its process died or its binder refused).
pub trait Deliver {
    fn deliver(&self, stream: i32, flags: i32) -> bool;
}

/// A registered process: its listener and the death link that removes it with the process.
pub struct Registered {
    listener: Strong<dyn IAudioVolumeListener>,
    _death: Option<DeathRecipient>,
}

impl Deliver for Registered {
    fn deliver(&self, stream: i32, flags: i32) -> bool {
        self.listener.volume_committed(stream, flags).is_ok()
    }
}

/// The listeners of every process that registered, keyed by (uid, pid).
pub struct Listeners<L> {
    entries: BTreeMap<(u32, i32), Entry<L>>,
    registrations: u64,
}

struct Entry<L> {
    registration: u64,
    listener: L,
}

impl<L> Default for Listeners<L> {
    fn default() -> Self {
        Listeners {
            entries: BTreeMap::new(),
            registrations: 0,
        }
    }
}

impl<L: Deliver> Listeners<L> {
    /// A new registration number; death removes only the entry with its number.
    pub fn next_registration(&mut self) -> u64 {
        self.registrations += 1;
        self.registrations
    }

    /// The first registration of `key` wins (AudioPolicyService.registerClient: `indexOfKey(token) < 0`). Returns whether the listener was kept.
    pub fn insert(&mut self, key: (u32, i32), registration: u64, listener: L) -> bool {
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(
            key,
            Entry {
                registration,
                listener,
            },
        );
        true
    }

    /// A death: removes `key`'s entry if it is the registration `registration`.
    pub fn remove(&mut self, key: (u32, i32), registration: u64) {
        if self
            .entries
            .get(&key)
            .is_some_and(|e| e.registration == registration)
        {
            self.entries.remove(&key);
        }
    }

    /// Delivers a write of `stream` with `flags` to every listener of `uid`; a listener that fails is dropped. Returns the deliveries made.
    pub fn commit(&mut self, uid: u32, stream: i32, flags: i32) -> usize {
        let mut delivered = 0;
        let mut dead = Vec::new();
        for (&key, entry) in self.entries.iter().filter(|(key, _)| key.0 == uid) {
            if entry.listener.deliver(stream, flags) {
                delivered += 1;
            } else {
                dead.push((key, entry.registration));
            }
        }
        for (key, registration) in dead {
            self.remove(key, registration);
        }
        delivered
    }
}

/// The table AudioService and the registry share.
pub type Shared = Arc<Mutex<Listeners<Registered>>>;

/// An empty table.
pub fn shared() -> Shared {
    Arc::new(Mutex::new(Listeners::default()))
}

fn table(shared: &Shared) -> MutexGuard<'_, Listeners<Registered>> {
    // a poisoned table still holds plain registrations
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The commit rule: `stream` is the one written, and its own step or mute changed in `changes` (Volumes::set / adjust). An alias member's change alone is not a publish of its own.
pub fn written(stream: i32, changes: &Changes) -> bool {
    changes.volume.iter().any(|&(s, _, _)| s == stream)
        || changes.mute.iter().any(|&(s, _)| s == stream)
}

/// AudioService's commit: the listeners of `uid` learn of `stream` when [`written`] holds.
pub fn commit(shared: &Shared, uid: u32, stream: i32, flags: i32, changes: &Changes) {
    if written(stream, changes) {
        table(shared).commit(uid, stream, flags);
    }
}

/// The registry service: the calling process's listener is kept under its (uid, pid) until its death.
struct Registry {
    shared: Shared,
}

impl Interface for Registry {}

impl IAudioVolumeRegistry for Registry {
    fn register(&self, listener: Strong<dyn IAudioVolumeListener>) -> Result<(), StatusCode> {
        let key = (
            ThreadState::get_calling_uid(),
            ThreadState::get_calling_pid(),
        );
        let registration = table(&self.shared).next_registration();
        let mut binder = listener.as_binder();
        let death = if binder.is_remote() {
            let weak: Weak<Mutex<Listeners<Registered>>> = Arc::downgrade(&self.shared);
            let mut death = DeathRecipient::new(move || {
                if let Some(shared) = weak.upgrade() {
                    table(&shared).remove(key, registration);
                }
            });
            if let Err(e) = binder.link_to_death(&mut death) {
                atl_diag::refused(INTERFACE, "register (dead listener)");
                return Err(e);
            }
            Some(death)
        } else {
            None
        };
        table(&self.shared).insert(
            key,
            registration,
            Registered {
                listener,
                _death: death,
            },
        );
        atl_diag::answered(INTERFACE, "register");
        Ok(())
    }
}

/// The binder served as [`atl_services_wire::audio_listener::SERVICE`], over the table `shared`.
pub fn new_binder(shared: Shared) -> SpIBinder {
    BnAudioVolumeRegistry::new_binder(Registry { shared }, BinderFeatures::default()).as_binder()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::volume::{AppVolumes, Dispatch, FLAG_ABSOLUTE_VOLUME, STREAM_MUSIC, Volumes};

    /// AudioSystem's stream number of the alias member that follows MUSIC (STREAM_TTS, ALIAS table: TTS 9 -> MUSIC 3).
    const STREAM_TTS: i32 = 9;
    const APP: u32 = 10_001;
    const OTHER_UID: u32 = 10_002;
    const PID: i32 = 42;

    /// A listener that logs what it was delivered; `alive` false: each delivery fails, as to a dead process.
    struct Recorder {
        got: Rc<RefCell<Vec<(i32, i32)>>>,
        alive: bool,
    }

    impl Deliver for Recorder {
        fn deliver(&self, stream: i32, flags: i32) -> bool {
            self.got.borrow_mut().push((stream, flags));
            self.alive
        }
    }

    /// A recorder and its log, kept by the test.
    fn recorder(alive: bool) -> (Recorder, Rc<RefCell<Vec<(i32, i32)>>>) {
        let got = Rc::new(RefCell::new(Vec::new()));
        (
            Recorder {
                got: Rc::clone(&got),
                alive,
            },
            got,
        )
    }

    #[test]
    fn a_real_write_publishes_its_stream() {
        let mut v = Volumes::default();
        let changes = v.set(STREAM_MUSIC, 2).expect("MUSIC is a stream");
        assert!(written(STREAM_MUSIC, &changes), "{changes:?}");
    }

    #[test]
    fn a_duplicate_write_publishes_nothing() {
        let mut v = Volumes::default();
        v.set(STREAM_MUSIC, 2).expect("MUSIC is a stream");
        let again = v.set(STREAM_MUSIC, 2).expect("MUSIC is a stream");
        assert!(!written(STREAM_MUSIC, &again), "{again:?}");
    }

    #[test]
    fn alias_members_move_but_only_the_written_stream_is_delivered() {
        let mut v = Volumes::default();
        let changes = v.set(STREAM_MUSIC, 2).expect("MUSIC is a stream");
        assert!(changes.volume.iter().any(|&(s, _, _)| s == STREAM_TTS));
        let mut listeners = Listeners::default();
        let (rec, got) = recorder(true);
        assert!(listeners.insert((APP, PID), 1, rec));
        let delivered = listeners.commit(APP, STREAM_MUSIC, 0);
        assert_eq!(delivered, 1);
        assert_eq!(*got.borrow(), vec![(STREAM_MUSIC, 0)]);
    }

    #[test]
    fn another_uid_gets_nothing() {
        let mut listeners = Listeners::default();
        let (rec, got) = recorder(true);
        assert!(listeners.insert((APP, PID), 1, rec));
        assert_eq!(listeners.commit(OTHER_UID, STREAM_MUSIC, 0), 0);
        assert!(got.borrow().is_empty());
    }

    #[test]
    fn a_dead_listener_is_dropped_and_gets_nothing_more() {
        let mut listeners = Listeners::default();
        let (rec, got) = recorder(false);
        assert!(listeners.insert((APP, PID), 1, rec));
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, 0), 0);
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, 0), 0);
        assert_eq!(*got.borrow(), vec![(STREAM_MUSIC, 0)]);
    }

    #[test]
    fn a_host_change_is_delivered_once_with_its_flags_and_not_dispatched_back() {
        let mut app = AppVolumes::default();
        let (changes, dispatch) = app
            .set(APP, STREAM_MUSIC, 2, FLAG_ABSOLUTE_VOLUME)
            .expect("MUSIC is a stream");
        assert_eq!(dispatch, Dispatch::None);
        assert!(written(STREAM_MUSIC, &changes), "{changes:?}");
        let mut listeners = Listeners::default();
        let (rec, got) = recorder(true);
        assert!(listeners.insert((APP, PID), 1, rec));
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, FLAG_ABSOLUTE_VOLUME), 1);
        assert_eq!(*got.borrow(), vec![(STREAM_MUSIC, FLAG_ABSOLUTE_VOLUME)]);
    }

    #[test]
    fn the_first_registration_of_a_process_is_kept() {
        let mut listeners = Listeners::default();
        let (first, got) = recorder(true);
        let (second, second_got) = recorder(true);
        assert!(listeners.insert((APP, PID), 1, first));
        assert!(!listeners.insert((APP, PID), 2, second));
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, 0), 1);
        assert_eq!(*got.borrow(), vec![(STREAM_MUSIC, 0)]);
        assert!(second_got.borrow().is_empty());
    }

    #[test]
    fn a_death_removes_only_its_own_registration() {
        let mut listeners = Listeners::default();
        let (rec, got) = recorder(true);
        assert!(listeners.insert((APP, PID), 1, rec));
        listeners.remove((APP, PID), 2);
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, 0), 1);
        listeners.remove((APP, PID), 1);
        assert_eq!(listeners.commit(APP, STREAM_MUSIC, 0), 0);
        assert_eq!(*got.borrow(), vec![(STREAM_MUSIC, 0)]);
    }
}
