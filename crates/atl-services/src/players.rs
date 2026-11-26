// SPDX-License-Identifier: MIT
//! The players of the `audio` service (PlaybackActivityMonitor's mPlayers): trackPlayer hands out a new id (piid) per player, every later player call names it, releasePlayer ends it.
//! AOSP takes the id from AudioSystem.newAudioPlayerId; the service numbers its own, counting up from 1 and never reusing one. PLAYER_PIID_INVALID (-1) is what runs out, never an id.
//! Each live player keeps what the duck policy reads (AudioPlaybackConfiguration): the tracking uid (checkConfigurationCaller: another uid's call is refused, root (0) passes), the player type, the content type, the state and the player's handle `P` (its IPlayer).
//! Plain Rust, no binder: `cargo test -p atl-services` covers it without the converter.

use std::collections::BTreeMap;

/// AudioPlaybackConfiguration.PLAYER_PIID_INVALID: trackPlayer's answer once no id is left.
pub const PIID_INVALID: i32 = -1;
/// The caller uid that may act on any player (PlaybackActivityMonitor.checkConfigurationCaller: binderUid 0 is not checked).
pub const ROOT_UID: u32 = 0;
/// AudioPlaybackConfiguration.PLAYER_STATE_RELEASED (:151).
pub const STATE_RELEASED: i32 = 0;
/// AudioPlaybackConfiguration.PLAYER_STATE_IDLE (:157): a new player's state (constructor :391).
pub const STATE_IDLE: i32 = 1;
/// AudioPlaybackConfiguration.PLAYER_STATE_STARTED (:163).
pub const STATE_STARTED: i32 = 2;
/// AudioPlaybackConfiguration.PLAYER_UPDATE_DEVICE_ID (:180): updates the device ids, not the state.
pub const UPDATE_DEVICE_ID: i32 = 5;
/// AudioPlaybackConfiguration.PLAYER_UPDATE_PORT_ID (:185): recorded apart, no state change (PlaybackActivityMonitor.playerEvent:384-386).
pub const UPDATE_PORT_ID: i32 = 6;
/// AudioPlaybackConfiguration.PLAYER_TYPE_JAM_SOUNDPOOL (:90).
pub const TYPE_JAM_SOUNDPOOL: i32 = 3;
/// AudioPlaybackConfiguration.PLAYER_TYPE_AAUDIO (:109).
pub const TYPE_AAUDIO: i32 = 13;

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

/// One live player (AudioPlaybackConfiguration): what trackPlayer, playerAttributes and playerEvent told the service.
#[derive(Debug)]
pub struct Player<P> {
    pub uid: u32,
    pub player_type: i32,
    pub content_type: i32,
    pub state: i32,
    device_ids: Vec<i32>,
    /// The player's handle (PlayerIdCard's IPlayer); None when the card carried none.
    pub proxy: Option<P>,
}

/// The live players. `next` is None once the last id is handed out.
#[derive(Debug)]
pub struct Players<P> {
    next: Option<i32>,
    live: BTreeMap<i32, Player<P>>,
}

impl<P> Default for Players<P> {
    fn default() -> Self {
        Players {
            next: Some(1),
            live: BTreeMap::new(),
        }
    }
}

impl<P> Players<P> {
    /// trackPlayer: a new id for a player tracked by `uid`, IDLE; PIID_INVALID once the ids run out.
    pub fn track(
        &mut self,
        uid: u32,
        player_type: i32,
        content_type: i32,
        proxy: Option<P>,
    ) -> i32 {
        let Some(piid) = self.next else {
            return PIID_INVALID;
        };
        self.next = piid.checked_add(1);
        self.live.insert(
            piid,
            Player {
                uid,
                player_type,
                content_type,
                state: STATE_IDLE,
                device_ids: Vec::new(),
                proxy,
            },
        );
        piid
    }

    /// What a call by `uid` on `piid` finds.
    pub fn access(&self, piid: i32, uid: u32) -> Access {
        match self.live.get(&piid) {
            None => Access::Unknown,
            Some(p) if uid == ROOT_UID || p.uid == uid => Access::Owned,
            Some(_) => Access::Denied,
        }
    }

    /// The live player `piid`.
    pub fn get(&self, piid: i32) -> Option<&Player<P>> {
        self.live.get(&piid)
    }

    /// Every live player, by piid.
    pub fn iter(&self) -> impl Iterator<Item = (i32, &Player<P>)> {
        self.live.iter().map(|(&piid, p)| (piid, p))
    }

    /// playerAttributes -> AudioPlaybackConfiguration.handleAudioAttributesEvent: the content type the duck policy reads. Returns what the call found.
    pub fn set_content_type(&mut self, piid: i32, uid: u32, content_type: i32) -> Access {
        let access = self.access(piid, uid);
        if access == Access::Owned
            && let Some(p) = self.live.get_mut(&piid)
        {
            p.content_type = content_type;
        }
        access
    }

    /// playerEvent (PlaybackActivityMonitor.playerEvent:364-418): what the call found, and whether the state changed to STARTED (the duck check's trigger, :410-413).
    /// A port id is recorded apart (:384-386), a SoundPool player reports no state (:396-400), and handleStateEvent (AudioPlaybackConfiguration:744-764) sets the state to the event (a device id update excepted) and counts new device ids of a start as a change.
    pub fn event(&mut self, piid: i32, uid: u32, event: i32, device_ids: &[i32]) -> (Access, bool) {
        let Some(p) = self.live.get(&piid) else {
            return (Access::Unknown, false);
        };
        if event == UPDATE_PORT_ID
            || (p.player_type == TYPE_JAM_SOUNDPOOL && event != STATE_RELEASED)
        {
            return (Access::Owned, false);
        }
        let access = self.access(piid, uid);
        let Some(p) = self.live.get_mut(&piid).filter(|_| access == Access::Owned) else {
            return (access, false);
        };
        let mut changed = false;
        if event != UPDATE_DEVICE_ID {
            changed = p.state != event;
            p.state = event;
        }
        if event == STATE_STARTED || event == UPDATE_DEVICE_ID {
            changed |= p.device_ids != device_ids;
            p.device_ids = device_ids.to_vec();
        }
        (access, changed && event == STATE_STARTED)
    }

    /// releasePlayer: ends the player when the caller may act on it; returns what the call found (Owned: released) and the released player.
    pub fn release(&mut self, piid: i32, uid: u32) -> (Access, Option<Player<P>>) {
        let access = self.access(piid, uid);
        let gone = (access == Access::Owned)
            .then(|| self.live.remove(&piid))
            .flatten();
        (access, gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(p: &mut Players<()>, uid: u32) -> i32 {
        p.track(uid, 2, 0, None)
    }

    /// Ids are unique and count up: each track gets the previous one plus one, and a released id is not handed out again.
    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let mut p = Players::default();
        let a = track(&mut p, 10_123);
        let b = track(&mut p, 10_123);
        assert_eq!(p.release(a, 10_123).0, Access::Owned);
        let c = track(&mut p, 10_123);
        assert!(a > 0, "first id {a}");
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
    }

    /// The last id is still handed out; the one after it is PIID_INVALID, and stays so.
    #[test]
    fn ids_run_out_without_wrapping() {
        let mut p = Players {
            next: Some(i32::MAX),
            live: BTreeMap::new(),
        };
        assert_eq!(track(&mut p, 10_123), i32::MAX);
        assert_eq!(track(&mut p, 10_123), PIID_INVALID);
        assert_eq!(track(&mut p, 10_123), PIID_INVALID);
    }

    /// Another uid is refused and keeps the player; the tracking uid and root act; an unknown id is a no-op for everyone.
    #[test]
    fn only_the_tracking_uid_or_root_acts() {
        let mut p = Players::default();
        let piid = track(&mut p, 10_123);
        assert_eq!(p.access(piid, 10_124), Access::Denied);
        assert_eq!(p.release(piid, 10_124).0, Access::Denied);
        assert_eq!(p.access(piid, 10_123), Access::Owned);
        assert_eq!(p.access(piid, ROOT_UID), Access::Owned);
        assert_eq!(p.access(PIID_INVALID, 10_123), Access::Unknown);
        assert_eq!(p.release(piid, 10_123).0, Access::Owned);
        assert_eq!(p.access(piid, 10_123), Access::Unknown);
        assert_eq!(p.release(piid, ROOT_UID).0, Access::Unknown);
    }

    /// A start from IDLE is a change; a repeat with the same device ids is not, with new ones it is (handleStateEvent:754-757); pause then start again is.
    /// Another uid's event leaves the state; a port id update and a SoundPool player's events change nothing.
    #[test]
    fn started_changes_follow_handle_state_event() {
        let mut p = Players::default();
        let piid = track(&mut p, 10_123);
        assert_eq!(p.get(piid).map(|x| x.state), Some(STATE_IDLE));
        assert_eq!(
            p.event(piid, 10_124, STATE_STARTED, &[2]),
            (Access::Denied, false)
        );
        assert_eq!(
            p.event(piid, 10_123, STATE_STARTED, &[2]),
            (Access::Owned, true)
        );
        assert_eq!(
            p.event(piid, 10_123, STATE_STARTED, &[2]),
            (Access::Owned, false)
        );
        assert_eq!(
            p.event(piid, 10_123, STATE_STARTED, &[3]),
            (Access::Owned, true)
        );
        assert_eq!(
            p.event(piid, 10_123, UPDATE_PORT_ID, &[9]),
            (Access::Owned, false)
        );
        assert_eq!(p.event(piid, 10_123, 3, &[]), (Access::Owned, false));
        assert_eq!(
            p.event(piid, 10_123, STATE_STARTED, &[3]),
            (Access::Owned, true)
        );
        let pool = p.track(10_123, TYPE_JAM_SOUNDPOOL, 0, None);
        assert_eq!(
            p.event(pool, 10_123, STATE_STARTED, &[2]),
            (Access::Owned, false)
        );
        assert_eq!(p.get(pool).map(|x| x.state), Some(STATE_IDLE));
    }
}
