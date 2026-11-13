// SPDX-License-Identifier: MIT
//! The `audio` service (Context.AUDIO_SERVICE): the converter's IAudioService stub. View.performClick reaches it on every click: ViewRootImpl.playSoundEffect -> getAudioManager ->
//! AudioManager.areNavigationRepeatSoundEffectsEnabled, which throws NullPointerException without the service (AudioManager.getService does not check the lookup).
//! ATL has no audio policy yet, so the two click calls are answered and no sound plays. Player tracking is served on piids only (`players`): trackPlayer hands out an id, releasePlayer ends it,
//! and the calls in between check the caller and are counted; they keep no state of the player (playerAttributes' usage check and playerEvent's focus logic wait for the audio policy).
//! Every other method stays counted and answered UNKNOWN_TRANSACTION.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::media::IAudioService::{
    BnAudioService, EnforcePermission, IAudioService,
};
use atl_parcelables::android::media::{AudioAttributes, PlayerBase_PlayerIdCard};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;
use crate::players::{Access, PIID_INVALID, Players};
use atl_services_types::counters;

/// The interface name counters use.
const INTERFACE: &str = "IAudioService";
/// AudioSystem.AUDIO_SESSION_ALLOCATE: playerSessionId rejects a session id at or below it (AudioService.playerSessionId).
const AUDIO_SESSION_ALLOCATE: i32 = 0;

struct AudioService {
    permissions: Arc<Permissions>,
    players: Mutex<Players>,
}

impl AudioService {
    fn players(&self) -> MutexGuard<'_, Players> {
        // a poisoned table still holds plain ids
        self.players.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Counts a player call by what it found: refused for a player another uid tracks, else answered (an unknown id is AOSP's no-op).
fn count_player_call(method: &str, access: Access) {
    match access {
        Access::Denied => counters::refused(INTERFACE, method),
        Access::Owned | Access::Unknown => counters::answered(INTERFACE, method),
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

    /// Repeat sounds for navigation (fast scroll) stay off: there is no sound effect player.
    fn r#areNavigationRepeatSoundEffectsEnabled(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// The click sound: nothing plays yet. Counted, so a run shows the call.
    fn r#playSoundEffect(&self, _effect_type: i32, _user_id: i32) -> binder::Result<()> {
        counters::answered(INTERFACE, "playSoundEffect");
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
            counters::refused(INTERFACE, "trackPlayer");
        } else {
            counters::answered(INTERFACE, "trackPlayer");
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
    };
    BnAudioService::new_binder(service, BinderFeatures::default()).as_binder()
}
