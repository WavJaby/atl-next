// SPDX-License-Identifier: MIT
//! The `audio` service (Context.AUDIO_SERVICE): the converter's IAudioService stub. View.performClick reaches it on every click: ViewRootImpl.playSoundEffect -> getAudioManager ->
//! AudioManager.areNavigationRepeatSoundEffectsEnabled, which throws NullPointerException without the service (AudioManager.getService does not check the lookup).
//! ATL has no audio policy yet, so the two click calls are answered and no sound plays. Player tracking is served on piids only (`players`): trackPlayer hands out an id, releasePlayer ends it,
//! and the calls in between check the caller and are counted; they keep no state of the player (playerAttributes' usage check and playerEvent's focus logic wait for the audio policy).
//! Stream volumes are the app's own gain (`volume`: AOSP defaults, never forwarded to the host mixer); ringer mode and audio mode are a healthy unlocked device's: RINGER_MODE_NORMAL, MODE_NORMAL
//! (zero would read as RINGER_MODE_SILENT / volume 0 and games mute themselves). Every other method stays counted and answered UNKNOWN_TRANSACTION.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::media::IAudioService::{
    BnAudioService, EnforcePermission, IAudioService,
};
use atl_parcelables::android::media::{AudioAttributes, PlayerBase_PlayerIdCard};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;
use crate::players::{Access, PIID_INVALID, Players};
use crate::volume::{BadStream, STREAM_MUSIC, Volumes};

/// The interface name counters use.
const INTERFACE: &str = "IAudioService";
/// AudioSystem.AUDIO_SESSION_ALLOCATE: playerSessionId rejects a session id at or below it (AudioService.playerSessionId).
const AUDIO_SESSION_ALLOCATE: i32 = 0;
/// AudioManager.RINGER_MODE_NORMAL: AudioService starts there with no saved Settings.Global.MODE_RINGER (readPersistedSettings' default) and nothing on a desktop changes it.
const RINGER_MODE_NORMAL: i32 = 2;
/// AudioSystem.MODE_NORMAL: AudioService.mMode's start; no call or communication session exists here.
const MODE_NORMAL: i32 = 0;
/// AudioManager.USE_DEFAULT_STREAM_TYPE: adjustSuggestedStreamVolume's "pick the active stream".
const USE_DEFAULT_STREAM_TYPE: i32 = i32::MIN;

struct AudioService {
    permissions: Arc<Permissions>,
    players: Mutex<Players>,
    volumes: Mutex<Volumes>,
}

impl AudioService {
    fn players(&self) -> MutexGuard<'_, Players> {
        // a poisoned table still holds plain ids
        self.players.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn volumes(&self) -> MutexGuard<'_, Volumes> {
        // a poisoned table still holds plain indices
        self.volumes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// ensureValidStreamType's IllegalArgumentException.
fn bad_stream(b: BadStream) -> Status {
    Status::new_exception_str(
        ExceptionCode::ILLEGAL_ARGUMENT,
        Some(format!("Bad stream type {}", b.0)),
    )
}

/// Counts a player call by what it found: refused for a player another uid tracks, else answered (an unknown id is AOSP's no-op).
fn count_player_call(method: &str, access: Access) {
    match access {
        Access::Denied => atl_diag::refused(INTERFACE, method),
        Access::Owned | Access::Unknown => atl_diag::answered(INTERFACE, method),
    };
}

impl Interface for AudioService {}

impl IAudioService for AudioService {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    /// AudioService.getStreamVolume:5723: the stream's index, 0 while muted.
    fn r#getStreamVolume(&self, stream: i32) -> binder::Result<i32> {
        self.volumes().volume(stream).map_err(bad_stream)
    }

    /// AudioService.getStreamMaxVolume:5793: MAX_STREAM_VOLUME.
    fn r#getStreamMaxVolume(&self, stream: i32) -> binder::Result<i32> {
        self.volumes().max(stream).map_err(bad_stream)
    }

    /// AudioService.getStreamMinVolume: the unprivileged MIN_STREAM_VOLUME.
    fn r#getStreamMinVolume(&self, stream: i32) -> binder::Result<i32> {
        self.volumes().min(stream).map_err(bad_stream)
    }

    /// AudioService.getLastAudibleStreamVolume (QUERY_AUDIO_STATE, enforced before this): the index regardless of mute.
    fn r#getLastAudibleStreamVolume(&self, stream: i32) -> binder::Result<i32> {
        self.volumes().last_audible(stream).map_err(bad_stream)
    }

    /// AudioService.isStreamMute: the stream's mute flag (set by ADJUST_MUTE or a 0 index).
    fn r#isStreamMute(&self, stream: i32) -> binder::Result<bool> {
        self.volumes().is_muted(stream).map_err(bad_stream)
    }

    /// AudioService.isMasterMute: no master mute exists here (the host mixer's mute is the user's, not the app's).
    fn r#isMasterMute(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// AudioService.setStreamVolume: remembered as the app-visible index (`volume`), not forwarded to the host mixer. No volume UI is shown (FLAG_SHOW_UI has no panel).
    fn r#setStreamVolume(
        &self,
        stream: i32,
        index: i32,
        _flags: i32,
        _calling_package: Option<&str>,
    ) -> binder::Result<()> {
        self.volumes().set(stream, index).map_err(bad_stream)
    }

    /// AudioService.adjustStreamVolume: one step or a mute change on the stream's alias group; an unknown direction is AOSP's IllegalArgumentException.
    fn r#adjustStreamVolume(
        &self,
        stream: i32,
        direction: i32,
        _flags: i32,
        _calling_package: Option<&str>,
    ) -> binder::Result<()> {
        match self
            .volumes()
            .adjust(stream, direction)
            .map_err(bad_stream)?
        {
            true => Ok(()),
            false => Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("Bad direction {direction}")),
            )),
        }
    }

    /// AudioService.adjustSuggestedStreamVolume: USE_DEFAULT_STREAM_TYPE resolves to STREAM_MUSIC (getActiveStreamType on a non-voice platform with nothing in communication), then as adjustStreamVolume.
    fn r#adjustSuggestedStreamVolume(
        &self,
        direction: i32,
        suggested: i32,
        flags: i32,
    ) -> binder::Result<()> {
        let stream = if suggested == USE_DEFAULT_STREAM_TYPE {
            STREAM_MUSIC
        } else {
            suggested
        };
        self.r#adjustStreamVolume(stream, direction, flags, None)
    }

    /// AudioService.getRingerModeExternal:6161: NORMAL (see RINGER_MODE_NORMAL).
    fn r#getRingerModeExternal(&self) -> binder::Result<i32> {
        Ok(RINGER_MODE_NORMAL)
    }

    /// AudioService.getRingerModeInternal: NORMAL, as external (no zen policy moves them apart).
    fn r#getRingerModeInternal(&self) -> binder::Result<i32> {
        Ok(RINGER_MODE_NORMAL)
    }

    /// AudioService.getMode:6820: MODE_NORMAL.
    fn r#getMode(&self) -> binder::Result<i32> {
        Ok(MODE_NORMAL)
    }

    /// AudioService.isMusicActive:3715 asks the audio policy whether STREAM_MUSIC plays. No policy tracks playback state yet (playerEvent keeps none), so false: the answer that lets a game
    /// start its own music (the common use: "is the user's own music playing?").
    fn r#isMusicActive(&self, _remotely: bool) -> binder::Result<bool> {
        Ok(false)
    }

    /// AudioService.isSpeakerphoneOn: whether the communication device is the speaker; MODE_NORMAL has no communication route, so false.
    fn r#isSpeakerphoneOn(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// AudioService.isVolumeFixed: mUseFixedVolume (config_useFixedVolume, false by default): volume calls take effect on the app-visible index.
    fn r#isVolumeFixed(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// Repeat sounds for navigation (fast scroll) stay off: there is no sound effect player.
    fn r#areNavigationRepeatSoundEffectsEnabled(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// The click sound: nothing plays yet. Counted, so a run shows the call.
    fn r#playSoundEffect(&self, _effect_type: i32, _user_id: i32) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "playSoundEffect");
        Ok(())
    }

    /// trackPlayer (AudioService.trackPlayer -> PlaybackActivityMonitor.trackPlayer): a new piid for the caller's player. The PlayerIdCard came through its codec; a refusal never reaches here.
    fn r#trackPlayer(&self, pic: Option<&PlayerBase_PlayerIdCard>) -> binder::Result<i32> {
        if pic.is_none() {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("trackPlayer: null PlayerIdCard".to_owned()),
            ));
        }
        let piid = self.players().track(ThreadState::get_calling_uid());
        if piid == PIID_INVALID {
            atl_diag::refused(INTERFACE, "trackPlayer");
        } else {
            atl_diag::answered(INTERFACE, "trackPlayer");
        }
        Ok(piid)
    }

    /// playerAttributes (AudioService.playerAttributes): AOSP checks a system usage against MODIFY_AUDIO_ROUTING and keeps the attributes. Here: the caller check only.
    fn r#playerAttributes(&self, piid: i32, _attr: Option<&AudioAttributes>) -> binder::Result<()> {
        let access = self.players().access(piid, ThreadState::get_calling_uid());
        count_player_call("playerAttributes", access);
        Ok(())
    }

    /// playerEvent (AudioService.playerEvent): AOSP keeps the state and follows it with focus and ducking. Here: the caller check only.
    fn r#playerEvent(
        &self,
        piid: i32,
        _event: i32,
        _event_id: Option<&[i32]>,
    ) -> binder::Result<()> {
        let access = self.players().access(piid, ThreadState::get_calling_uid());
        count_player_call("playerEvent", access);
        Ok(())
    }

    /// playerSessionId (AudioService.playerSessionId): AOSP throws IllegalArgumentException for a session id at or below AUDIO_SESSION_ALLOCATE (oneway: no caller sees it).
    fn r#playerSessionId(&self, piid: i32, session_id: i32) -> binder::Result<()> {
        if session_id <= AUDIO_SESSION_ALLOCATE {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("invalid session Id {session_id}")),
            ));
        }
        let access = self.players().access(piid, ThreadState::get_calling_uid());
        count_player_call("playerSessionId", access);
        Ok(())
    }

    /// releasePlayer (AudioService.releasePlayer -> PlaybackActivityMonitor.releasePlayer): ends the player. Oneway: no reply reaches the caller.
    fn r#releasePlayer(&self, piid: i32) -> binder::Result<()> {
        let access = self.players().release(piid, ThreadState::get_calling_uid());
        count_player_call("releasePlayer", access);
        Ok(())
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    let service = AudioService {
        permissions,
        players: Mutex::new(Players::default()),
        volumes: Mutex::new(Volumes::default()),
    };
    BnAudioService::new_binder(service, BinderFeatures::default()).as_binder()
}
