// SPDX-License-Identifier: MIT
//! The framework duck of the `audio` service: PlaybackActivityMonitor.duckPlayers and its DuckingManager / DuckedApp (PlaybackActivityMonitor.java:815-879, :1190-1312), the policy AOSP runs in system_server.
//! A ducked uid's started players get the duck VolumeShaper through their IPlayer; restoring the uid reverses it. The gain itself is computed in the player's process by AOSP's VolumeHandler (atl-audioflinger), never here, and never on the host volume.
//! Plain Rust over [`Players`]: the shaper goes out through a caller's `apply`, which returns whether the player took the call (AOSP's `applyVolumeShaper` not throwing).

use std::collections::BTreeMap;

use crate::players::{Players, STATE_STARTED, TYPE_AAUDIO, TYPE_JAM_SOUNDPOOL};

/// PlaybackActivityMonitor.VOLUME_SHAPER_SYSTEM_DUCK_ID (:85).
pub const DUCK_ID: i32 = 1;
/// PlaybackActivityMonitor.VOLUME_SHAPER_SYSTEM_STRONG_DUCK_ID (:88).
pub const STRONG_DUCK_ID: i32 = 4;
/// DUCK_VSHAPE's end volume (:97), a "normal duck" at -14 dB.
pub const DUCK_VOLUME: f32 = 0.2;
/// STRONG_DUCK_VSHAPE's end volume (:112), -35 dB.
pub const STRONG_DUCK_VOLUME: f32 = 0.017783;
/// Both duck curves' duration: MediaFocusControl.getFocusRampTimeMs(GAIN_TRANSIENT_MAY_DUCK, USAGE_NOTIFICATION) (MediaFocusControl.java:1082-1088 -> 500).
pub const DUCK_DURATION_MS: f64 = 500.0;
/// AudioAttributes.CONTENT_TYPE_SPEECH (AudioAttributes.java:92): a speaking player is never ducked (duckPlayers:836-843).
pub const CONTENT_TYPE_SPEECH: i32 = 1;
/// UNDUCKABLE_PLAYER_TYPES (:140-143).
const UNDUCKABLE: [i32; 2] = [TYPE_AAUDIO, TYPE_JAM_SOUNDPOOL];

/// VolumeShaperConfigurationType (frameworks/av aidl): ID names an existing shaper, SCALE carries a curve.
pub const CONFIG_TYPE_ID: i32 = 0;
pub const CONFIG_TYPE_SCALE: i32 = 1;
/// VolumeShaper.Configuration.toParcelable's optionFlags for OPTION_FLAG_CLOCK_TIME (VolumeShaper.java:632-641): 1 << VolumeShaperConfigurationOptionFlag.CLOCK_TIME (1).
pub const OPTION_CLOCK_TIME: i32 = 1 << 1;
/// InterpolatorType.CUBIC (2): the Builder's default interpolator (VolumeShaper.java:870), which the duck curves keep.
pub const INTERPOLATOR_CUBIC: i32 = 2;
/// Operation.toParcelable's flags (VolumeShaper.java:1326-1345): REVERSE -> 1 << 0, CREATE_IF_NEEDED -> 1 << VolumeShaperOperationFlag.CREATE_IF_NECESSARY (4).
pub const OP_REVERSE: i32 = 1 << 0;
pub const OP_CREATE_IF_NECESSARY: i32 = 1 << 4;

/// A VolumeShaper.Configuration as its parcelable carries it (toParcelable, VolumeShaper.java:491-511). `curve` is None for a TYPE_ID configuration, which carries only its id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub config_type: i32,
    pub id: i32,
    pub option_flags: i32,
    pub duration_ms: f64,
    pub interpolator: i32,
    /// (time, volume) pairs, the parcelable's xy.
    pub curve: Option<[(f32, f32); 2]>,
}

/// A VolumeShaper.Operation as its parcelable carries it (toParcelable :1277-1283). The Builder's defaults are replaceId -1, xOffset NaN (:1369-1370).
/// No PartialEq: an unset xOffset is NaN, which never equals itself.
#[derive(Debug, Clone, Copy)]
pub struct Operation {
    pub flags: i32,
    pub replace_id: i32,
    pub x_offset: f32,
}

/// The configuration and operation one duck call sends.
#[derive(Debug, Clone, Copy)]
pub struct Shaper {
    pub config: Config,
    pub op: Operation,
}

/// DUCK_VSHAPE or STRONG_DUCK_VSHAPE (:93-118): 1.0 -> the duck volume over the ramp, clock time.
fn duck_config(strong: bool) -> Config {
    let (id, end) = if strong {
        (STRONG_DUCK_ID, STRONG_DUCK_VOLUME)
    } else {
        (DUCK_ID, DUCK_VOLUME)
    };
    Config {
        config_type: CONFIG_TYPE_SCALE,
        id,
        option_flags: OPTION_CLOCK_TIME,
        duration_ms: DUCK_DURATION_MS,
        interpolator: INTERPOLATOR_CUBIC,
        curve: Some([(0.0, 1.0), (1.0, end)]),
    }
}

/// DuckedApp.addDuck's call (:1264-1281): the duck curve with PLAY_CREATE_IF_NEEDED, or PLAY_SKIP_RAMP (xOffset 1.0, :146-147) for a player that starts under the duck.
pub fn duck(strong: bool, skip_ramp: bool) -> Shaper {
    Shaper {
        config: duck_config(strong),
        op: Operation {
            flags: OP_CREATE_IF_NECESSARY,
            replace_id: -1,
            x_offset: if skip_ramp { 1.0 } else { f32::NAN },
        },
    }
}

/// DuckedApp.removeUnduckAll's call (:1283-1305): DUCK_ID or STRONG_DUCK_ID (a TYPE_ID configuration, :104-105) with Operation.REVERSE (:1172-1175).
pub fn unduck(strong: bool) -> Shaper {
    Shaper {
        config: Config {
            config_type: CONFIG_TYPE_ID,
            id: if strong { STRONG_DUCK_ID } else { DUCK_ID },
            option_flags: 0,
            duration_ms: 0.0,
            interpolator: 0,
            curve: None,
        },
        op: Operation {
            flags: OP_REVERSE,
            replace_id: -1,
            x_offset: f32::NAN,
        },
    }
}

/// One ducked uid (DuckedApp): the duck strength fixed when it was ducked, and the players that took the duck.
#[derive(Debug)]
struct DuckedApp {
    strong: bool,
    ducked: Vec<i32>,
}

impl DuckedApp {
    /// addDuck: a player already ducked is left; otherwise it is ducked and recorded when the call went out.
    fn add<P>(
        &mut self,
        piid: i32,
        proxy: Option<&P>,
        skip_ramp: bool,
        apply: &mut dyn FnMut(&P, Shaper) -> bool,
    ) {
        if self.ducked.contains(&piid) {
            return;
        }
        // a player without an IPlayer throws in getPlayerProxy().applyVolumeShaper: not recorded (:1276-1280)
        if proxy.is_some_and(|p| apply(p, duck(self.strong, skip_ramp))) {
            self.ducked.push(piid);
        }
    }
}

/// DuckingManager: the ducked uids.
#[derive(Debug, Default)]
pub struct Ducking {
    duckers: BTreeMap<u32, DuckedApp>,
}

impl Ducking {
    /// Whether `uid` is ducked.
    pub fn is_ducked(&self, uid: u32) -> bool {
        self.duckers.contains_key(&uid)
    }

    /// The piids ducked for `uid`.
    pub fn ducked(&self, uid: u32) -> &[i32] {
        self.duckers.get(&uid).map_or(&[], |d| d.ducked.as_slice())
    }

    /// PlaybackActivityMonitor.duckPlayers (:815-859) with forceDuck false (the only forced duck, accessibility's, is refused before the stack: audio.rs admit_request): whether the framework handles the loser's CAN_DUCK.
    /// No player at all: handled, nothing ducked (:821-823). A started loser player that speaks or cannot be shaped: not handled (:836-851). Otherwise the uid is ducked (even with no started player, so a later start is ducked) and each started player gets the curve.
    pub fn duck_players<P>(
        &mut self,
        players: &Players<P>,
        winner_uid: u32,
        loser_uid: u32,
        strong: bool,
        apply: &mut dyn FnMut(&P, Shaper) -> bool,
    ) -> bool {
        if players.iter().next().is_none() {
            return true;
        }
        let mut to_duck = Vec::new();
        for (piid, p) in players.iter() {
            if p.uid != winner_uid && p.uid == loser_uid && p.state == STATE_STARTED {
                if p.content_type == CONTENT_TYPE_SPEECH || UNDUCKABLE.contains(&p.player_type) {
                    return false;
                }
                to_duck.push((piid, p.proxy.as_ref()));
            }
        }
        // duckUid (:1193-1203): the strength is the first duck's; a ducked uid keeps it
        let da = self.duckers.entry(loser_uid).or_insert(DuckedApp {
            strong,
            ducked: Vec::new(),
        });
        for (piid, proxy) in to_duck {
            da.add(piid, proxy, false, apply);
        }
        true
    }

    /// unduckUid (:1205-1212) -> removeUnduckAll: each ducked player still live gets the reverse; the uid is no longer ducked.
    pub fn unduck_uid<P>(
        &mut self,
        uid: u32,
        players: &Players<P>,
        apply: &mut dyn FnMut(&P, Shaper) -> bool,
    ) {
        let Some(da) = self.duckers.remove(&uid) else {
            return;
        };
        for piid in da.ducked {
            if let Some(proxy) = players.get(piid).and_then(|p| p.proxy.as_ref()) {
                apply(proxy, unduck(da.strong));
            }
        }
    }

    /// checkDuck (:1215-1224): a player that started while its uid is ducked is ducked at once, skipping the ramp.
    pub fn check_duck<P>(
        &mut self,
        piid: i32,
        players: &Players<P>,
        apply: &mut dyn FnMut(&P, Shaper) -> bool,
    ) {
        let Some(p) = players.get(piid) else {
            return;
        };
        if let Some(da) = self.duckers.get_mut(&p.uid) {
            da.add(piid, p.proxy.as_ref(), true, apply);
        }
    }

    /// removeReleased (:1231-1240): a released player leaves its uid's ducked list.
    pub fn remove_released(&mut self, piid: i32, uid: u32) {
        if let Some(da) = self.duckers.get_mut(&uid) {
            da.ducked.retain(|&d| d != piid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::players::STATE_IDLE;

    const WINNER: u32 = 10_001;
    const LOSER: u32 = 10_002;
    /// AudioAttributes.CONTENT_TYPE_MUSIC (AudioAttributes.java:98).
    const MUSIC: i32 = 2;
    /// AudioPlaybackConfiguration.PLAYER_TYPE_JAM_AUDIOTRACK (:82).
    const AUDIOTRACK: i32 = 1;

    /// A shaper with xOffset as its bits: NaN (the Builder's "unset") then compares equal to itself.
    type Key = (Config, i32, i32, u32);

    fn key(s: Shaper) -> Key {
        (
            s.config,
            s.op.flags,
            s.op.replace_id,
            s.op.x_offset.to_bits(),
        )
    }

    /// Each call as (player tag, shaper key); the proxy is the tag.
    type Calls = Vec<(u32, Key)>;

    fn started(p: &mut Players<u32>, uid: u32, ty: i32, content: i32, tag: u32) -> i32 {
        let piid = p.track(uid, ty, content, Some(tag));
        p.event(piid, uid, STATE_STARTED, &[2]);
        piid
    }

    fn recorder(calls: &mut Calls) -> impl FnMut(&u32, Shaper) -> bool + '_ {
        |tag, s| {
            calls.push((*tag, key(s)));
            true
        }
    }

    /// The curves as PlaybackActivityMonitor builds them (:93-118, :124-147) and the parcel encoding of VolumeShaper.toParcelable: ids 1 and 4, 1.0 -> 0.2 / 0.017783 over 500 ms, clock time, cubic; CREATE_IF_NEEDED = 16 (1 << 4); SKIP_RAMP's xOffset 1.0; REVERSE = 1 on a TYPE_ID configuration.
    #[test]
    fn shapers_match_playback_activity_monitor() {
        let d = duck(false, false);
        assert_eq!(
            (
                d.config.config_type,
                d.config.id,
                d.config.option_flags,
                d.config.interpolator
            ),
            (1, 1, 2, 2)
        );
        assert_eq!(d.config.duration_ms, 500.0);
        assert_eq!(d.config.curve, Some([(0.0, 1.0), (1.0, 0.2)]));
        assert_eq!((d.op.flags, d.op.replace_id), (16, -1));
        assert!(d.op.x_offset.is_nan());
        let s = duck(true, true);
        assert_eq!(s.config.id, 4);
        assert_eq!(s.config.curve, Some([(0.0, 1.0), (1.0, 0.017783)]));
        assert_eq!(s.op.x_offset, 1.0);
        let u = unduck(false);
        assert_eq!(
            (u.config.config_type, u.config.id, u.config.curve),
            (0, 1, None)
        );
        assert_eq!((u.op.flags, u.op.replace_id), (1, -1));
        assert!(u.op.x_offset.is_nan());
        assert_eq!(unduck(true).config.id, 4);
    }

    /// A started cross-uid music player is ducked with the ramp and handled; restore reverses it once and forgets the uid.
    #[test]
    fn eligible_started_player_is_ducked_and_restored() {
        let mut p = Players::default();
        let piid = started(&mut p, LOSER, AUDIOTRACK, MUSIC, 7);
        let mut d = Ducking::default();
        let mut calls = Calls::new();
        assert!(d.duck_players(&p, WINNER, LOSER, false, &mut recorder(&mut calls)));
        assert_eq!(calls, vec![(7, key(duck(false, false)))]);
        assert_eq!(d.ducked(LOSER), &[piid]);
        calls.clear();
        d.unduck_uid(LOSER, &p, &mut recorder(&mut calls));
        assert_eq!(calls, vec![(7, key(unduck(false)))]);
        assert!(!d.is_ducked(LOSER));
    }

    /// A started speech player, AAudio or SoundPool player of the loser: not handled, nothing ducked, the uid not ducked (:836-851).
    #[test]
    fn speech_and_unduckable_players_are_not_handled() {
        for (ty, content) in [(AUDIOTRACK, CONTENT_TYPE_SPEECH), (TYPE_AAUDIO, MUSIC)] {
            let mut p = Players::default();
            started(&mut p, LOSER, AUDIOTRACK, MUSIC, 1);
            started(&mut p, LOSER, ty, content, 2);
            let mut d = Ducking::default();
            let mut calls = Calls::new();
            assert!(!d.duck_players(&p, WINNER, LOSER, false, &mut recorder(&mut calls)));
            assert!(calls.is_empty(), "{calls:?}");
            assert!(!d.is_ducked(LOSER));
        }
    }

    /// No player at all: handled, uid not ducked (:821-823). Players but none started: handled and the uid ducked, so a later start is ducked skipping the ramp (checkDuck).
    #[test]
    fn no_started_player_marks_the_uid_and_a_later_start_skips_the_ramp() {
        let mut d = Ducking::default();
        let mut calls = Calls::new();
        assert!(d.duck_players(
            &Players::<u32>::default(),
            WINNER,
            LOSER,
            false,
            &mut recorder(&mut calls)
        ));
        assert!(!d.is_ducked(LOSER));
        let mut p = Players::default();
        let piid = p.track(LOSER, AUDIOTRACK, MUSIC, Some(3));
        assert_eq!(p.get(piid).map(|x| x.state), Some(STATE_IDLE));
        assert!(d.duck_players(&p, WINNER, LOSER, true, &mut recorder(&mut calls)));
        assert!(calls.is_empty());
        assert!(d.is_ducked(LOSER));
        p.event(piid, LOSER, STATE_STARTED, &[2]);
        d.check_duck(piid, &p, &mut recorder(&mut calls));
        assert_eq!(calls, vec![(3, key(duck(true, true)))]);
        // a second start of a ducked player is left (addDuck :1266-1269)
        d.check_duck(piid, &p, &mut recorder(&mut calls));
        assert_eq!(calls.len(), 1);
    }

    /// Players of the winner's uid and of third uids are left; a player without an IPlayer is not recorded; a released player is not reversed.
    #[test]
    fn only_the_losers_players_with_a_handle_are_ducked() {
        let mut p = Players::default();
        started(&mut p, WINNER, AUDIOTRACK, MUSIC, 1);
        started(&mut p, 10_003, AUDIOTRACK, MUSIC, 2);
        let kept = started(&mut p, LOSER, AUDIOTRACK, MUSIC, 3);
        let bare = p.track(LOSER, AUDIOTRACK, MUSIC, None);
        p.event(bare, LOSER, STATE_STARTED, &[2]);
        let mut d = Ducking::default();
        let mut calls = Calls::new();
        assert!(d.duck_players(&p, WINNER, LOSER, false, &mut recorder(&mut calls)));
        assert_eq!(calls, vec![(3, key(duck(false, false)))]);
        assert_eq!(d.ducked(LOSER), &[kept]);
        d.remove_released(kept, LOSER);
        calls.clear();
        d.unduck_uid(LOSER, &p, &mut recorder(&mut calls));
        assert!(calls.is_empty());
    }
}
