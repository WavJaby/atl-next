// SPDX-License-Identifier: MIT
//! Player ids (piids) of the `audio` service: trackPlayer hands out a new id per player, every later player call names it, releasePlayer ends it.
//! AOSP takes the id from AudioSystem.newAudioPlayerId; the service numbers its own, counting up from 1 and never reusing one. PLAYER_PIID_INVALID (-1) is what runs out, never an id.
//! The table keeps only the uid that tracked each live id, which PlaybackActivityMonitor.checkConfigurationCaller checks: another uid's call is refused, root (0) passes. Nothing else of a player is kept (owner: piids only).
//! Plain Rust, no binder: `cargo test -p atl-services` covers it without the converter.

use std::collections::BTreeMap;

/// AudioPlaybackConfiguration.PLAYER_PIID_INVALID: trackPlayer's answer once no id is left.
pub const PIID_INVALID: i32 = -1;
/// The caller uid that may act on any player (PlaybackActivityMonitor.checkConfigurationCaller: binderUid 0 is not checked).
pub const ROOT_UID: u32 = 0;

/// What a call on a piid finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Tracked by the caller (or the caller is root): the call applies.
    Owned,
    /// No live player under this id: AOSP does nothing, and no error reaches the caller.
    Unknown,
    /// Tracked by another uid: refused.
    Denied,
}

/// The live players. `next` is None once the last id is handed out.
#[derive(Debug)]
pub struct Players {
    next: Option<i32>,
    /// Live ids: piid -> the uid that tracked it.
    owners: BTreeMap<i32, u32>,
}

impl Default for Players {
    fn default() -> Self {
        Players {
            next: Some(1),
            owners: BTreeMap::new(),
        }
    }
}

impl Players {
    /// A new id for a player tracked by `uid`; PIID_INVALID once the ids run out.
    pub fn track(&mut self, uid: u32) -> i32 {
        let Some(piid) = self.next else {
            return PIID_INVALID;
        };
        self.next = piid.checked_add(1);
        self.owners.insert(piid, uid);
        piid
    }

    /// What a call by `uid` on `piid` finds.
    pub fn access(&self, piid: i32, uid: u32) -> Access {
        match self.owners.get(&piid) {
            None => Access::Unknown,
            Some(&owner) if uid == ROOT_UID || owner == uid => Access::Owned,
            Some(_) => Access::Denied,
        }
    }

    /// releasePlayer: ends the player when the caller may act on it; returns what the call found (Owned: released).
    pub fn release(&mut self, piid: i32, uid: u32) -> Access {
        let access = self.access(piid, uid);
        if access == Access::Owned {
            self.owners.remove(&piid);
        }
        access
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are unique and count up: each track gets the previous one plus one, and a released id is not handed out again.
    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let mut p = Players::default();
        let a = p.track(10_123);
        let b = p.track(10_123);
        assert_eq!(p.release(a, 10_123), Access::Owned);
        let c = p.track(10_123);
        assert!(a > 0, "first id {a}");
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
    }

    /// The last id is still handed out; the one after it is PIID_INVALID, and stays so.
    #[test]
    fn ids_run_out_without_wrapping() {
        let mut p = Players {
            next: Some(i32::MAX),
            owners: BTreeMap::new(),
        };
        assert_eq!(p.track(10_123), i32::MAX);
        assert_eq!(p.track(10_123), PIID_INVALID);
        assert_eq!(p.track(10_123), PIID_INVALID);
    }

    /// Another uid is refused and keeps the player; the tracking uid and root act; an unknown id is a no-op for everyone.
    #[test]
    fn only_the_tracking_uid_or_root_acts() {
        let mut p = Players::default();
        let piid = p.track(10_123);
        assert_eq!(p.access(piid, 10_124), Access::Denied);
        assert_eq!(p.release(piid, 10_124), Access::Denied);
        assert_eq!(p.access(piid, 10_123), Access::Owned);
        assert_eq!(p.access(piid, ROOT_UID), Access::Owned);
        assert_eq!(p.access(PIID_INVALID, 10_123), Access::Unknown);
        assert_eq!(p.release(piid, 10_123), Access::Owned);
        assert_eq!(p.access(piid, 10_123), Access::Unknown);
        assert_eq!(p.release(piid, ROOT_UID), Access::Unknown);
    }
}
