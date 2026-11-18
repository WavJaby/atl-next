// SPDX-License-Identifier: MIT
//! The `audio` service (Context.AUDIO_SERVICE): the converter's IAudioService stub. View.performClick reaches it on every click: ViewRootImpl.playSoundEffect -> getAudioManager ->
//! AudioManager.areNavigationRepeatSoundEffectsEnabled, which throws NullPointerException without the service (AudioManager.getService does not check the lookup).
//! ATL has no audio policy yet, so the two click calls are answered and no sound plays. Player tracking is served on piids only (`players`): trackPlayer hands out an id, releasePlayer ends it,
//! and the calls in between check the caller and are counted; they keep no state of the player (playerAttributes' usage check and playerEvent's focus logic wait for the audio policy).
//! Stream volumes are kept per app uid (`volume`: the stated deviation from AOSP's device-wide volume); STREAM_MUSIC's alias group is the app's host volume through the absolute-volume device
//! its process registers (registerDeviceVolumeDispatcherForAbsoluteVolume, atl-audioflinger), other groups stay the app's own remembered gain. Each change is broadcast to the app's uid
//! (VOLUME_CHANGED_ACTION, STREAM_MUTE_CHANGED_ACTION) through the activity manager's delivery. Ringer mode and audio mode are a healthy unlocked device's: RINGER_MODE_NORMAL, MODE_NORMAL
//! (zero would read as RINGER_MODE_SILENT / volume 0 and games mute themselves). Every other method stays counted and answered UNKNOWN_TRANSACTION.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::media::IAudioDeviceVolumeDispatcher::IAudioDeviceVolumeDispatcher;
use atl_aidl::aidl::android::media::IAudioService::{
    BnAudioService, EnforcePermission, IAudioService,
};
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::media::{
    AudioAttributes, AudioDeviceAttributes, PlayerBase_PlayerIdCard, VolumeInfo,
};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::media::VolumeInfo::INDEX_NOT_SET;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_services_wire::audio_volume::{
    ALIAS, DEVICE_OUT_SPEAKER, DEVICE_VOLUME_BEHAVIOR_ABSOLUTE, ROLE_OUTPUT,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::packages::Permissions;
use crate::players::{Access, PIID_INVALID, Players};
use crate::volume::{Absolute, AppVolumes, BadStream, Changes, Dispatch, STREAM_MUSIC};

/// The interface name counters use.
const INTERFACE: &str = "IAudioService";
/// AudioManager broadcast actions and extras (AudioManager.java:204, :246, :288, :298, :306, :312, :338).
const VOLUME_CHANGED_ACTION: &str = "android.media.VOLUME_CHANGED_ACTION";
const STREAM_MUTE_CHANGED_ACTION: &str = "android.media.STREAM_MUTE_CHANGED_ACTION";
const EXTRA_VOLUME_STREAM_TYPE: &str = "android.media.EXTRA_VOLUME_STREAM_TYPE";
const EXTRA_VOLUME_STREAM_TYPE_ALIAS: &str = "android.media.EXTRA_VOLUME_STREAM_TYPE_ALIAS";
const EXTRA_VOLUME_STREAM_VALUE: &str = "android.media.EXTRA_VOLUME_STREAM_VALUE";
const EXTRA_PREV_VOLUME_STREAM_VALUE: &str = "android.media.EXTRA_PREV_VOLUME_STREAM_VALUE";
const EXTRA_STREAM_VOLUME_MUTED: &str = "android.media.EXTRA_STREAM_VOLUME_MUTED";

/// Sends a system broadcast to one uid's registered receivers (wired to the activity manager by atl-system-services).
pub type Broadcast = Arc<dyn Fn(u32, &Intent) + Send + Sync>;

/// A uid's registered absolute-volume device: its dispatcher and the device it named.
struct Registered {
    dispatcher: Strong<dyn IAudioDeviceVolumeDispatcher>,
    device: AudioDeviceAttributes,
}

/// The volume state: each uid's volumes and the dispatcher of its device, changed together.
#[derive(Default)]
struct VolumeState {
    volumes: AppVolumes,
    dispatchers: BTreeMap<u32, Registered>,
}

/// Why a registration is refused: the device is not the one STREAM_MUSIC plays to here, or the behaviour is not the absolute one this service models. Each refusal is counted under its name.
fn refuse_device(device: &AudioDeviceAttributes, behavior: i32) -> Option<&'static str> {
    if device.role != ROLE_OUTPUT || device.native_type != DEVICE_OUT_SPEAKER {
        return Some("registerDeviceVolumeDispatcherForAbsoluteVolume (unknown device)");
    }
    if behavior != DEVICE_VOLUME_BEHAVIOR_ABSOLUTE {
        return Some("registerDeviceVolumeDispatcherForAbsoluteVolume (behavior not absolute)");
    }
    None
}

/// The VolumeInfo of the list that names a MUSIC-group stream with index and range set (AbsoluteVolumeDeviceInfo.getMatchingVolumeInfoForStream for STREAM_MUSIC) as the device and its index.
fn music_volume(volumes: &[Option<VolumeInfo>]) -> Option<(Absolute, i32)> {
    let music = ALIAS[STREAM_MUSIC as usize];
    volumes.iter().flatten().find_map(|v| {
        let set = [v.index, v.min_index, v.max_index]
            .iter()
            .all(|&i| i != INDEX_NOT_SET);
        let in_group = usize::try_from(v.stream_type)
            .ok()
            .and_then(|s| ALIAS.get(s))
            .is_some_and(|&a| a == music);
        (set && in_group).then_some((
            Absolute {
                stream: v.stream_type,
                min: v.min_index,
                max: v.max_index,
            },
            v.index,
        ))
    })
}

/// The broadcasts of `changes`, as VolumeStreamState.setIndex:9822-9869 (VOLUME_CHANGED_ACTION: stream, value, previous value, alias) and broadcastMuteSetting:4307-4318 (STREAM_MUTE_CHANGED_ACTION:
/// muted, stream) build them.
fn intents(changes: &Changes) -> Vec<Intent> {
    let volume = changes.volume.iter().map(|&(s, prev, value)| {
        let alias = i32::try_from(ALIAS[s as usize]).unwrap_or(s);
        (
            VOLUME_CHANGED_ACTION,
            vec![
                (EXTRA_VOLUME_STREAM_VALUE, Value::Int(value)),
                (EXTRA_PREV_VOLUME_STREAM_VALUE, Value::Int(prev)),
                (EXTRA_VOLUME_STREAM_TYPE, Value::Int(s)),
                (EXTRA_VOLUME_STREAM_TYPE_ALIAS, Value::Int(alias)),
            ],
        )
    });
    let mute = changes.mute.iter().map(|&(s, muted)| {
        (
            STREAM_MUTE_CHANGED_ACTION,
            vec![
                (EXTRA_STREAM_VOLUME_MUTED, Value::Bool(muted)),
                (EXTRA_VOLUME_STREAM_TYPE, Value::Int(s)),
            ],
        )
    });
    volume
        .chain(mute)
        .map(|(action, extras)| Intent {
            action: Some(action.to_owned()),
            extras: Some(Bundle::from_entries(
                extras.into_iter().map(|(k, v)| (k.to_owned(), v)),
            )),
            ..Intent::default()
        })
        .collect()
}
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
    volumes: Mutex<VolumeState>,
    broadcast: Broadcast,
}

impl AudioService {
    fn players(&self) -> MutexGuard<'_, Players> {
        // a poisoned table still holds plain ids
        self.players.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn volumes(&self) -> MutexGuard<'_, VolumeState> {
        // a poisoned table still holds plain indices
        self.volumes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The calling uid's volume of `stream`, read by `f`.
    fn read<T>(
        &self,
        stream: i32,
        f: impl FnOnce(&crate::volume::Volumes, i32) -> Result<T, BadStream>,
    ) -> binder::Result<T> {
        let uid = ThreadState::get_calling_uid();
        f(self.volumes().volumes.of(uid), stream).map_err(bad_stream)
    }

    /// After a change of `uid`'s volumes (taken under the lock, sent outside it): its broadcasts to `uid`, then what it owes the device.
    fn changed(&self, uid: u32, changes: &Changes, dispatch: Dispatch) {
        for intent in intents(changes) {
            (self.broadcast)(uid, &intent);
        }
        let Dispatch::Changed {
            device,
            index,
            muted,
        } = dispatch
        else {
            if dispatch == Dispatch::NoDevice {
                no_device();
            }
            return;
        };
        let target = self
            .volumes()
            .dispatchers
            .get(&uid)
            .map(|r| (r.dispatcher.clone(), r.device.clone()));
        let Some((dispatcher, attributes)) = target else {
            no_device();
            return;
        };
        let info = VolumeInfo {
            stream_type: device.stream,
            has_mute_command: true,
            muted,
            index,
            min_index: device.min,
            max_index: device.max,
        };
        // oneway: a dead controller fails here (AOSP logs the RemoteException and keeps the device)
        match dispatcher.r#dispatchDeviceVolumeChanged(Some(&attributes), Some(&info)) {
            Ok(()) => atl_diag::sent(
                "IAudioDeviceVolumeDispatcher",
                "dispatchDeviceVolumeChanged",
            ),
            Err(e) => {
                crate::activity::log(format_args!(
                    "absolute volume change of uid {uid} not dispatched: {e}"
                ));
                atl_diag::refused(
                    "IAudioDeviceVolumeDispatcher",
                    "dispatchDeviceVolumeChanged",
                )
            }
        };
    }
}

/// An app-side MUSIC change of a uid with no absolute-volume device (no host stream: a process without audio, or a host without an audio server): it stays the app-visible index only.
fn no_device() -> u64 {
    atl_diag::refused(
        INTERFACE,
        "dispatch (no absolute-volume device for the uid)",
    )
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
        self.read(stream, |v, s| v.volume(s))
    }

    /// AudioService.getStreamMaxVolume:5793: MAX_STREAM_VOLUME.
    fn r#getStreamMaxVolume(&self, stream: i32) -> binder::Result<i32> {
        self.read(stream, |v, s| v.max(s))
    }

    /// AudioService.getStreamMinVolume: the unprivileged MIN_STREAM_VOLUME.
    fn r#getStreamMinVolume(&self, stream: i32) -> binder::Result<i32> {
        self.read(stream, |v, s| v.min(s))
    }

    /// AudioService.getLastAudibleStreamVolume (QUERY_AUDIO_STATE, enforced before this): the index regardless of mute.
    fn r#getLastAudibleStreamVolume(&self, stream: i32) -> binder::Result<i32> {
        self.read(stream, |v, s| v.last_audible(s))
    }

    /// AudioService.isStreamMute: the stream's mute flag (set by ADJUST_MUTE or a 0 index).
    fn r#isStreamMute(&self, stream: i32) -> binder::Result<bool> {
        self.read(stream, |v, s| v.is_muted(s))
    }

    /// AudioService.isMasterMute: no master mute exists here (the host mixer's mute is the user's, not the app's).
    fn r#isMasterMute(&self) -> binder::Result<bool> {
        Ok(false)
    }

    /// AudioService.setStreamVolume on the caller's own volumes: the index, then the mute change (onSetStreamVolume), the broadcasts, and for the MUSIC group the dispatch to the app's
    /// absolute-volume device unless FLAG_ABSOLUTE_VOLUME says the device sent it (:5267-5271). No volume UI is shown (FLAG_SHOW_UI has no panel).
    fn r#setStreamVolume(
        &self,
        stream: i32,
        index: i32,
        flags: i32,
        _calling_package: Option<&str>,
    ) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        let (changes, dispatch) = self
            .volumes()
            .volumes
            .set(uid, stream, index, flags)
            .map_err(bad_stream)?;
        self.changed(uid, &changes, dispatch);
        Ok(())
    }

    /// AudioService.adjustStreamVolume: one step or a mute change on the stream's alias group of the caller, then as setStreamVolume (:4103-4108 / :4166-4170: the device does not handle
    /// adjustments itself, so the result is dispatched); an unknown direction is AOSP's IllegalArgumentException.
    fn r#adjustStreamVolume(
        &self,
        stream: i32,
        direction: i32,
        flags: i32,
        _calling_package: Option<&str>,
    ) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        let adjusted = self
            .volumes()
            .volumes
            .adjust(uid, stream, direction, flags)
            .map_err(bad_stream)?;
        let Some((changes, dispatch)) = adjusted else {
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("Bad direction {direction}")),
            ));
        };
        self.changed(uid, &changes, dispatch);
        Ok(())
    }

    /// AudioService.registerDeviceVolumeDispatcherForAbsoluteVolume:8208-8250: a null device or list is NullPointerException (Objects.requireNonNull); register keeps the dispatcher and sets the
    /// stream to its VolumeInfo's index, unregister drops it. Deviations: AOSP requires MODIFY_AUDIO_ROUTING or BLUETOOTH_PRIVILEGED; here any caller registers, for its own uid's volumes only
    /// (the app's controller runs under the app's uid and can move only its own host volume, which setStreamVolume already allows). Only the device STREAM_MUSIC plays to here
    /// (DEVICE_OUT_SPEAKER) with absolute behaviour and a MUSIC-group VolumeInfo is kept; anything else is counted refused and ignored (AOSP would keep a device no stream is routed to).
    fn r#registerDeviceVolumeDispatcherForAbsoluteVolume(
        &self,
        register: bool,
        cb: Option<&Strong<dyn IAudioDeviceVolumeDispatcher>>,
        _package_name: Option<&str>,
        device: Option<&AudioDeviceAttributes>,
        volumes: Option<&[Option<VolumeInfo>]>,
        _handles_volume_adjustment: bool,
        volume_behavior: i32,
    ) -> binder::Result<()> {
        const METHOD: &str = "registerDeviceVolumeDispatcherForAbsoluteVolume";
        let (Some(device), Some(volumes)) = (device, volumes) else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some(format!("{METHOD}: null device or volumes")),
            ));
        };
        let uid = ThreadState::get_calling_uid();
        if !register {
            let mut st = self.volumes();
            st.volumes.unregister(uid);
            st.dispatchers.remove(&uid);
            atl_diag::answered(INTERFACE, &format!("{METHOD} (unregister)"));
            return Ok(());
        }
        if let Some(why) = refuse_device(device, volume_behavior) {
            atl_diag::refused(INTERFACE, why);
            return Ok(());
        }
        let (Some(cb), Some((absolute, index))) = (cb, music_volume(volumes)) else {
            atl_diag::refused(
                INTERFACE,
                &format!("{METHOD} (no dispatcher or no MUSIC VolumeInfo)"),
            );
            return Ok(());
        };
        let changes = {
            let mut st = self.volumes();
            st.dispatchers.insert(
                uid,
                Registered {
                    dispatcher: cb.clone(),
                    device: device.clone(),
                },
            );
            st.volumes
                .register(uid, absolute, index)
                .map_err(bad_stream)?
        };
        atl_diag::answered(INTERFACE, METHOD);
        self.changed(uid, &changes, Dispatch::None);
        Ok(())
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

    /// AudioService.setStreamVolumeWithAttribution:4719 (what AudioManager.setStreamVolume calls): HardeningEnforcer.blockVolumeMethod blocks nothing on a non-automotive device unless all hardening
    /// is enabled (off by default), so as setStreamVolume.
    fn r#setStreamVolumeWithAttribution(
        &self,
        stream: i32,
        index: i32,
        flags: i32,
        calling_package: Option<&str>,
        _attribution_tag: Option<&str>,
    ) -> binder::Result<()> {
        self.r#setStreamVolume(stream, index, flags, calling_package)
    }

    /// AudioService.adjustStreamVolumeWithAttribution:3935 (AudioManager.adjustStreamVolume): as setStreamVolumeWithAttribution, then adjustStreamVolume. Not modelled: the
    /// STREAM_ACCESSIBILITY check (CHANGE_ACCESSIBILITY_VOLUME).
    fn r#adjustStreamVolumeWithAttribution(
        &self,
        stream: i32,
        direction: i32,
        flags: i32,
        calling_package: Option<&str>,
        _attribution_tag: Option<&str>,
    ) -> binder::Result<()> {
        self.r#adjustStreamVolume(stream, direction, flags, calling_package)
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

/// The service's binder, for `addService`. `broadcast` delivers the volume broadcasts to one uid.
pub fn new_binder(permissions: Arc<Permissions>, broadcast: Broadcast) -> SpIBinder {
    let service = AudioService {
        permissions,
        players: Mutex::new(Players::default()),
        volumes: Mutex::new(VolumeState::default()),
        broadcast,
    };
    BnAudioService::new_binder(service, BinderFeatures::default()).as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(key: &str) -> u64 {
        atl_diag::counts()
            .into_iter()
            .find(|(o, a, _)| o == "refused" && a == key)
            .map_or(0, |(_, _, n)| n)
    }

    /// The device the app's controller registers (atl-audioflinger dispatcher.rs): output, speaker.
    fn speaker() -> AudioDeviceAttributes {
        AudioDeviceAttributes {
            role: ROLE_OUTPUT,
            device_type: atl_services_wire::audio_volume::TYPE_BUILTIN_SPEAKER,
            native_type: DEVICE_OUT_SPEAKER,
            ..AudioDeviceAttributes::default()
        }
    }

    /// Refusal: a device other than the speaker (an input, a headset 0x4) or a non-absolute behaviour (full volume, 1) is refused, each under its own counter.
    #[test]
    fn unknown_devices_are_refused() {
        assert_eq!(
            refuse_device(&speaker(), DEVICE_VOLUME_BEHAVIOR_ABSOLUTE),
            None
        );
        let headset = AudioDeviceAttributes {
            native_type: 0x4,
            ..speaker()
        };
        let input = AudioDeviceAttributes {
            role: 1,
            ..speaker()
        };
        for d in [&headset, &input] {
            assert_eq!(
                refuse_device(d, DEVICE_VOLUME_BEHAVIOR_ABSOLUTE),
                Some("registerDeviceVolumeDispatcherForAbsoluteVolume (unknown device)")
            );
        }
        assert_eq!(
            refuse_device(&speaker(), 1),
            Some("registerDeviceVolumeDispatcherForAbsoluteVolume (behavior not absolute)")
        );
    }

    /// Refusal: a MUSIC change for a uid with no device is counted (the services' `refused` line a run prints).
    #[test]
    fn a_uid_without_a_device_is_counted() {
        let key = format!("{INTERFACE}.dispatch (no absolute-volume device for the uid)");
        let before = counted(&key);
        let service = AudioService {
            permissions: Arc::new(Permissions::new(
                &crate::packages::Packages::new(vec![], BTreeMap::new()),
                &[],
            )),
            players: Mutex::new(Players::default()),
            volumes: Mutex::new(VolumeState::default()),
            broadcast: Arc::new(|_, _| {}),
        };
        let (changes, dispatch) = service
            .volumes()
            .volumes
            .set(10_001, STREAM_MUSIC, 4, 0)
            .expect("valid stream");
        assert_eq!(dispatch, Dispatch::NoDevice);
        service.changed(10_001, &changes, dispatch);
        assert_eq!(counted(&key), before + 1);
    }

    /// The broadcasts carry AOSP's extras: a MUSIC change 5 -> 9 is one VOLUME_CHANGED per group stream with value, previous value, stream and alias; a mute one STREAM_MUTE_CHANGED with muted and stream.
    #[test]
    fn intents_carry_aosps_extras() {
        let changes = Changes {
            volume: vec![(STREAM_MUSIC, 5, 9)],
            mute: vec![(9, true)],
        };
        let i = intents(&changes);
        assert_eq!(i.len(), 2);
        assert_eq!(i[0].action.as_deref(), Some(VOLUME_CHANGED_ACTION));
        let extras = i[0]
            .extras
            .as_ref()
            .expect("extras")
            .entries()
            .expect("entries");
        assert!(extras.contains(&(EXTRA_VOLUME_STREAM_VALUE.to_owned(), Value::Int(9))));
        assert!(extras.contains(&(EXTRA_PREV_VOLUME_STREAM_VALUE.to_owned(), Value::Int(5))));
        assert!(extras.contains(&(EXTRA_VOLUME_STREAM_TYPE.to_owned(), Value::Int(3))));
        assert!(extras.contains(&(EXTRA_VOLUME_STREAM_TYPE_ALIAS.to_owned(), Value::Int(3))));
        assert_eq!(i[1].action.as_deref(), Some(STREAM_MUTE_CHANGED_ACTION));
        let extras = i[1]
            .extras
            .as_ref()
            .expect("extras")
            .entries()
            .expect("entries");
        assert!(extras.contains(&(EXTRA_STREAM_VOLUME_MUTED.to_owned(), Value::Bool(true))));
        assert!(extras.contains(&(EXTRA_VOLUME_STREAM_TYPE.to_owned(), Value::Int(9))));
    }
}
