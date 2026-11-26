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
use atl_aidl::aidl::android::media::IAudioFocusDispatcher::IAudioFocusDispatcher;
use atl_aidl::aidl::android::media::IAudioService::{
    BnAudioService, EnforcePermission, IAudioService,
};
use atl_aidl::aidl::android::media::audiopolicy::IAudioPolicyCallback::IAudioPolicyCallback;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::media::{
    AudioAttributes, AudioDeviceAttributes, PlayerBase_PlayerIdCard, VolumeInfo,
};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::media::VolumeInfo::INDEX_NOT_SET;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_services_types::{app_ops, permission};
use atl_services_wire::audio_volume::{
    ALIAS, DEVICE_OUT_SPEAKER, DEVICE_VOLUME_BEHAVIOR_ABSOLUTE, ROLE_OUTPUT,
};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
    ThreadState,
};

use crate::appops::AppOps;
use crate::audio_focus::{self, Request, Stack};
use crate::packages::{PackageTable, Permissions};
use crate::players::{Access, PIID_INVALID, Players};
use crate::uid_state::UidViews;
use crate::volume::{Absolute, AppVolumes, BadStream, Changes, Dispatch, STREAM_MUSIC};
use crate::volume_listeners::{self, Shared};

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
/// AppOpsManager.OP_TAKE_AUDIO_FOCUS (AppOpsManager.java): the MODE_FOREGROUND op a focus request needs.
const OP_TAKE_AUDIO_FOCUS: i32 = 32;
/// Build.VERSION_CODES.VANILLA_ICE_CREAM: HardeningEnforcer.blockFocusMethod blocks a denied focus request from this target on (below it only under all-hardening, off by default).
const VANILLA_ICE_CREAM: i32 = 35;
/// AudioAttributes.USAGE_ASSISTANCE_ACCESSIBILITY (AudioAttributes.java:186): the usage a forced accessibility duck names.
const USAGE_ASSISTANCE_ACCESSIBILITY: i32 = 11;
/// The system usages (AudioAttributes.java:220-251: USAGE_CALL_ASSISTANT 17, SYSTEM_USAGE_OFFSET 1000 + 0..3). ATL supports none, so a request naming one is refused.
const SYSTEM_USAGES: [i32; 5] = [17, 1000, 1001, 1002, 1003];
/// AudioFocusRequest.KEY_ACCESSIBILITY_FORCE_FOCUS_DUCKING (AudioFocusRequest.java:226).
const KEY_ACCESSIBILITY_FORCE_FOCUS_DUCKING: &str = "a11y_force_ducking";
/// AudioSystem.IN_VOICE_COMM_FOCUS_ID (AudioSystem.java:2512): the telephony focus owner, which ATL has no telephony to hold.
const IN_VOICE_COMM_FOCUS_ID: &str = "AudioFocus_For_Phone_Ring_And_Calls";
/// The permissions that let a request skip HardeningEnforcer (AudioService.requestAudioFocus:11324-11327).
const MODIFY_AUDIO_SETTINGS_PRIVILEGED: &str =
    "android.permission.MODIFY_AUDIO_SETTINGS_PRIVILEGED";
const MODIFY_AUDIO_ROUTING: &str = "android.permission.MODIFY_AUDIO_ROUTING";

/// The focus stack of the service: owners keyed by their requesting binder; each owner's death link is a [`FocusLink`].
type FocusStack = Stack<SpIBinder, FocusLink>;

/// The death link of one focus owner. Dropping it unlinks: DeathRecipient's own drop does not (binder proxy.rs `Drop for DeathRecipient`).
struct FocusLink {
    binder: SpIBinder,
    recipient: DeathRecipient,
}

impl Drop for FocusLink {
    fn drop(&mut self) {
        // a dead binder refuses the unlink: nothing is left to undo
        let _ = self.binder.unlink_to_death(&mut self.recipient);
    }
}

struct AudioService {
    permissions: Arc<Permissions>,
    players: Mutex<Players>,
    volumes: Mutex<VolumeState>,
    broadcast: Broadcast,
    /// AppOps' policy: the hardening check reads TAKE_AUDIO_FOCUS through it (noteOperation's path)
    appops: AppOps,
    focus: Arc<Mutex<FocusStack>>,
    /// The app processes' volume listeners, told of each write (volume_listeners).
    listeners: Shared,
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

    fn focus(&self) -> MutexGuard<'_, FocusStack> {
        // a poisoned stack still holds plain owners
        self.focus.lock().unwrap_or_else(|e| e.into_inner())
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

/// The arguments AudioService.requestAudioFocus reads before MediaFocusControl sees the request.
struct FocusArgs<'a> {
    aa: Option<&'a AudioAttributes>,
    focus_req_type: i32,
    client_id: Option<&'a str>,
    calling_package_name: Option<&'a str>,
    flags: i32,
    sdk: i32,
}

/// Counts a named refusal (atl-diag keys it `IAudioService.<why>`) and returns its name.
fn refuse(why: &'static str) -> Option<&'static str> {
    atl_diag::refused(INTERFACE, why);
    Some(why)
}

fn is_system_usage(usage: i32) -> bool {
    SYSTEM_USAGES.contains(&usage)
}

/// AudioService.forceFocusDuckingForAccessibility's flag: the bundle names KEY_ACCESSIBILITY_FORCE_FOCUS_DUCKING true.
fn forces_duck(aa: &AudioAttributes) -> bool {
    aa.bundle
        .as_ref()
        .and_then(|b| b.entries().ok())
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|(k, v)| k == KEY_ACCESSIBILITY_FORCE_FOCUS_DUCKING && *v == Value::Bool(true))
        })
}

/// AudioService.requestAudioFocus's checks in its order, for a caller `caller`: a test flag throws (IllegalArgumentException), a refusal returns its counted name, a request that may proceed returns None.
/// The hardening check reads TAKE_AUDIO_FOCUS through `ops` (noteOperation's path), so a request is admitted by the same policy noteOperation applies.
fn admit_request(
    a: &FocusArgs<'_>,
    caller: u32,
    ops: &AppOps,
    permissions: &Permissions,
) -> binder::Result<Option<&'static str>> {
    if a.flags & audio_focus::FLAG_TEST != 0 {
        return Err(Status::new_exception_str(
            ExceptionCode::ILLEGAL_ARGUMENT,
            Some("Invalid test flag".to_owned()),
        ));
    }
    if a.aa.is_some_and(|aa| is_system_usage(aa.usage)) {
        return Ok(refuse("requestAudioFocus (unsupported usage)"));
    }
    if a.flags & audio_focus::FLAG_LOCK == audio_focus::FLAG_LOCK {
        return Ok(refuse(
            "requestAudioFocus (lock: no audio policy, no telephony)",
        ));
    }
    let (Some(aa), Some(client), Some(package)) = (a.aa, a.client_id, a.calling_package_name)
    else {
        return Ok(refuse("requestAudioFocus (null parameter)"));
    };
    if client == IN_VOICE_COMM_FOCUS_ID {
        return Ok(refuse(
            "requestAudioFocus (telephony focus id: no telephony)",
        ));
    }
    // AudioService.java:11324-11331: a privileged caller or a system uid skips HardeningEnforcer. Otherwise blockFocusMethod (:197-247) blocks a denied TAKE_AUDIO_FOCUS from target 35 on.
    let privileged = caller < permission::FIRST_APPLICATION_UID
        || permissions.holds(
            caller,
            &[MODIFY_AUDIO_SETTINGS_PRIVILEGED, MODIFY_AUDIO_ROUTING],
            true,
        );
    if !privileged {
        let uid = i32::try_from(caller).unwrap_or(-1);
        // blockFocusMethod:199-201: an empty package is the uid's first package; none gives "[uid]" (getPackNameForUid), a name no package has, so the op reads ERRORED
        let resolved = match package {
            "" => ops
                .package_of_uid(caller)
                .unwrap_or_else(|| format!("[{uid}]")),
            p => p.to_owned(),
        };
        let denied =
            ops.note_mode(caller, OP_TAKE_AUDIO_FOCUS, uid, &resolved)? != app_ops::ALLOWED;
        if denied && a.sdk >= VANILLA_ICE_CREAM {
            return Ok(refuse(
                "requestAudioFocus (blocked by hardening: TAKE_AUDIO_FOCUS denied)",
            ));
        }
    }
    // forceFocusDuckingForAccessibility (:11198-11223) with no accessibility service: only uid 0 forces the duck
    if caller == 0
        && a.focus_req_type == audio_focus::GAIN_TRANSIENT_MAY_DUCK
        && aa.usage == USAGE_ASSISTANCE_ACCESSIBILITY
        && forces_duck(aa)
    {
        return Ok(refuse(
            "requestAudioFocus (forced accessibility duck: no accessibility service)",
        ));
    }
    Ok(None)
}

/// AudioService.abandonAudioFocus's only check (:11384): the usage. Abandon has no hardening gate in AOSP.
fn abandon_refusal(aa: Option<&AudioAttributes>) -> Option<&'static str> {
    if aa.is_some_and(|aa| is_system_usage(aa.usage)) {
        return refuse("abandonAudioFocus (unsupported usage)");
    }
    None
}

/// The death link of a focus owner on `binder`: when `binder` dies, the stack drops the binder's owners and restores the next top once. Holds the stack weakly (no cycle through the link).
fn link_death(stack: &Arc<Mutex<FocusStack>>, binder: &SpIBinder) -> Option<FocusLink> {
    let stack = Arc::downgrade(stack);
    let owner = binder.clone();
    let mut recipient = DeathRecipient::new(move || {
        if let Some(stack) = stack.upgrade() {
            let gone = stack.lock().unwrap_or_else(|e| e.into_inner()).die(&owner);
            // the links unlink outside the lock (the obituary may be delivering)
            drop(gone);
        }
    });
    binder.clone().link_to_death(&mut recipient).ok()?;
    Some(FocusLink {
        binder: binder.clone(),
        recipient,
    })
}

/// The focus change of one owner, sent to its dispatcher as a oneway call (AudioService's `fd`).
fn dispatch_to(fd: &Strong<dyn IAudioFocusDispatcher>) -> audio_focus::Dispatch {
    let fd = fd.clone();
    Box::new(move |change, client| {
        match fd.r#dispatchAudioFocusChange(change, Some(client)) {
            Ok(()) => atl_diag::sent("IAudioFocusDispatcher", "dispatchAudioFocusChange"),
            Err(e) => {
                crate::activity::log(format_args!(
                    "audio focus change of {client} not dispatched: {e}"
                ));
                atl_diag::refused("IAudioFocusDispatcher", "dispatchAudioFocusChange")
            }
        };
    })
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
        volume_listeners::commit(&self.listeners, uid, stream, flags, &changes);
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
        volume_listeners::commit(&self.listeners, uid, stream, flags, &changes);
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

    /// AudioService.requestAudioFocus:11266-11358 over the ordinary stack: the checks (admit_request), then the caller's binder is pinged and its death link made, then MediaFocusControl's admission (audio_focus::Stack::request).
    /// Lock, telephony, forced duck and the registered-policy path are counted refusals (FAILED), so no owner is made for them.
    fn r#requestAudioFocus(
        &self,
        aa: Option<&AudioAttributes>,
        focus_req_type: i32,
        cb: Option<&SpIBinder>,
        fd: Option<&Strong<dyn IAudioFocusDispatcher>>,
        client_id: Option<&str>,
        calling_package_name: Option<&str>,
        _attribution_tag: Option<&str>,
        flags: i32,
        _pcb: Option<&Strong<dyn IAudioPolicyCallback>>,
        sdk: i32,
    ) -> binder::Result<i32> {
        let caller = ThreadState::get_calling_uid();
        let args = FocusArgs {
            aa,
            focus_req_type,
            client_id,
            calling_package_name,
            flags,
            sdk,
        };
        if admit_request(&args, caller, &self.appops, &self.permissions)?.is_some() {
            return Ok(audio_focus::REQUEST_FAILED);
        }
        let Some(cb) = cb else {
            return Err(Status::new_exception(ExceptionCode::NULL_POINTER, None));
        };
        // pingBinder first: a dead client is refused before any link (MediaFocusControl:1146)
        let alive = cb.clone().ping_binder().is_ok();
        let request = Request {
            client: client_id.unwrap_or_default().to_owned(),
            binder: cb.clone(),
            uid: caller,
            gain: focus_req_type,
            flags,
            sdk,
            dispatch: fd.map(dispatch_to),
        };
        let (code, gone) = self
            .focus()
            .request(request, alive, || link_death(&self.focus, cb));
        // the released links unlink outside the stack lock
        drop(gone);
        Ok(code)
    }

    /// AudioService.abandonAudioFocus:11377-11424: the usage check only (no hardening gate). The client's owners leave the stack silently; a repeat succeeds.
    fn r#abandonAudioFocus(
        &self,
        _fd: Option<&Strong<dyn IAudioFocusDispatcher>>,
        client_id: Option<&str>,
        aa: Option<&AudioAttributes>,
        _calling_package_name: Option<&str>,
    ) -> binder::Result<i32> {
        if abandon_refusal(aa).is_some() {
            return Ok(audio_focus::REQUEST_FAILED);
        }
        let Some(client) = client_id else {
            return Err(Status::new_exception(ExceptionCode::NULL_POINTER, None));
        };
        let gone = self.focus().abandon(client);
        drop(gone);
        Ok(audio_focus::REQUEST_GRANTED)
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

/// The service's binder, for `addService`. `broadcast` delivers the volume broadcasts to one uid. `packages` and `uids` are the AppOps policy's, the same ones the appops service reads.
pub fn new_binder(
    permissions: Arc<Permissions>,
    broadcast: Broadcast,
    packages: Arc<PackageTable>,
    uids: UidViews,
    listeners: Shared,
) -> SpIBinder {
    let appops = AppOps::new(Arc::clone(&permissions), packages, uids);
    let service = AudioService {
        permissions,
        players: Mutex::new(Players::default()),
        volumes: Mutex::new(VolumeState::default()),
        broadcast,
        appops,
        focus: Arc::new(Mutex::new(FocusStack::default())),
        listeners,
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

    use crate::uid_state::{ProcessState, UidView};

    /// A service over `packages` (installed, as records) and `uids` (the uid views the AppOps policy reads).
    fn new_service(packages: Vec<crate::packages::PackageRecord>, uids: UidViews) -> AudioService {
        let table = Arc::new(PackageTable::new(crate::packages::Packages::new(
            packages,
            BTreeMap::new(),
        )));
        let permissions = Arc::new(Permissions::new(&table.snapshot(), &[]));
        AudioService {
            appops: AppOps::new(Arc::clone(&permissions), table, uids),
            permissions,
            players: Mutex::new(Players::default()),
            volumes: Mutex::new(VolumeState::default()),
            broadcast: Arc::new(|_, _| {}),
            focus: Arc::new(Mutex::new(FocusStack::default())),
            listeners: volume_listeners::shared(),
        }
    }

    /// The uid view of a process in `state` (no top or restriction time).
    fn view_of(state: ProcessState) -> UidView {
        UidView {
            state: Some(state),
            top_since: None,
            restricted_since: None,
            now: 0,
        }
    }

    /// A service with one installed app, org.app (uid 10_001), in `state`.
    fn app_service(state: ProcessState) -> AudioService {
        let uids: UidViews = Arc::new(move |_| view_of(state));
        new_service(vec![crate::packages::fixture::record("org.app")], uids)
    }

    /// Media audio: usage 1, no system usage.
    fn media() -> AudioAttributes {
        AudioAttributes {
            usage: 1,
            ..AudioAttributes::default()
        }
    }

    /// org.app's request for a gain at target `sdk`, no flags.
    fn request(aa: &AudioAttributes, sdk: i32) -> FocusArgs<'_> {
        FocusArgs {
            aa: Some(aa),
            focus_req_type: audio_focus::GAIN,
            client_id: Some("org.app#1"),
            calling_package_name: Some("org.app"),
            flags: 0,
            sdk,
        }
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
        let service = new_service(vec![], Arc::new(|_| view_of(ProcessState::Top)));
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

    /// Eligible: a foreground app's media gain passes every check (TAKE_AUDIO_FOCUS allowed, target 35).
    #[test]
    fn foreground_media_gain_is_admitted() {
        let s = app_service(ProcessState::Top);
        let aa = media();
        let admitted = admit_request(&request(&aa, 35), 10_001, &s.appops, &s.permissions);
        assert_eq!(admitted.expect("no exception"), None);
    }

    /// Target 35 with TAKE_AUDIO_FOCUS denied (a cached app): FAILED, counted as the hardening refusal.
    #[test]
    fn denied_take_audio_focus_blocks_target_35() {
        let s = app_service(ProcessState::CachedEmpty);
        let key = "IAudioService.requestAudioFocus (blocked by hardening: TAKE_AUDIO_FOCUS denied)";
        let before = counted(key);
        let aa = media();
        let refused = admit_request(&request(&aa, 35), 10_001, &s.appops, &s.permissions);
        assert_eq!(
            refused.expect("no exception"),
            Some("requestAudioFocus (blocked by hardening: TAKE_AUDIO_FOCUS denied)")
        );
        assert_eq!(counted(key), before + 1);
    }

    /// An empty calling package is the uid's first package (HardeningEnforcer.blockFocusMethod:199-201, getPackNameForUid): org.app, ALLOWED in the foreground, so target 35 is admitted.
    #[test]
    fn empty_package_resolves_to_the_uids_package() {
        let s = app_service(ProcessState::Top);
        let aa = media();
        let mut args = request(&aa, 35);
        args.calling_package_name = Some("");
        let admitted = admit_request(&args, 10_001, &s.appops, &s.permissions);
        assert_eq!(admitted.expect("no exception"), None);
    }

    /// The same empty package with TAKE_AUDIO_FOCUS denied (a cached app, target 35): org.app reads IGNORED, so the request is refused as the hardening refusal.
    #[test]
    fn empty_package_with_denied_take_audio_focus_is_refused() {
        let s = app_service(ProcessState::CachedEmpty);
        let aa = media();
        let mut args = request(&aa, 35);
        args.calling_package_name = Some("");
        let refused = admit_request(&args, 10_001, &s.appops, &s.permissions);
        assert_eq!(
            refused.expect("no exception"),
            Some("requestAudioFocus (blocked by hardening: TAKE_AUDIO_FOCUS denied)")
        );
    }

    /// The same denial from a target 34 is exempt under default hardening (blockFocusMethod: unblockedBySdk).
    #[test]
    fn denied_take_audio_focus_is_exempt_below_target_35() {
        let s = app_service(ProcessState::CachedEmpty);
        let aa = media();
        let admitted = admit_request(&request(&aa, 34), 10_001, &s.appops, &s.permissions);
        assert_eq!(admitted.expect("no exception"), None);
    }

    /// AudioService.java:11269: a test flag throws IllegalArgumentException before any other check.
    #[test]
    fn test_flag_throws_illegal_argument() {
        let s = app_service(ProcessState::Top);
        let aa = media();
        let args = FocusArgs {
            flags: audio_focus::FLAG_TEST,
            ..request(&aa, 35)
        };
        let err = admit_request(&args, 10_001, &s.appops, &s.permissions)
            .expect_err("a test flag throws");
        assert_eq!(err.exception_code(), ExceptionCode::ILLEGAL_ARGUMENT);
    }

    /// The lock needs an audio policy or telephony, both absent here: refused and counted.
    #[test]
    fn lock_is_refused_and_counted() {
        let s = app_service(ProcessState::Top);
        let key = "IAudioService.requestAudioFocus (lock: no audio policy, no telephony)";
        let before = counted(key);
        let aa = media();
        let args = FocusArgs {
            flags: audio_focus::FLAG_LOCK,
            ..request(&aa, 35)
        };
        let refused = admit_request(&args, 10_001, &s.appops, &s.permissions);
        assert_eq!(
            refused.expect("no exception"),
            Some("requestAudioFocus (lock: no audio policy, no telephony)")
        );
        assert_eq!(counted(key), before + 1);
    }

    /// Named refusals that need no binder: a system usage, a null parameter, the telephony focus id.
    #[test]
    fn unsupported_requests_are_refused_by_name() {
        let s = app_service(ProcessState::Top);
        let aa = media();
        let system = AudioAttributes {
            usage: 1000,
            ..AudioAttributes::default()
        };
        let check = |args: FocusArgs<'_>| {
            admit_request(&args, 10_001, &s.appops, &s.permissions).expect("no exception")
        };
        assert_eq!(
            check(request(&system, 35)),
            Some("requestAudioFocus (unsupported usage)")
        );
        let no_client = FocusArgs {
            client_id: None,
            ..request(&aa, 35)
        };
        assert_eq!(check(no_client), Some("requestAudioFocus (null parameter)"));
        let telephony = FocusArgs {
            client_id: Some(IN_VOICE_COMM_FOCUS_ID),
            ..request(&aa, 35)
        };
        assert_eq!(
            check(telephony),
            Some("requestAudioFocus (telephony focus id: no telephony)")
        );
    }

    /// Forced accessibility duck: only uid 0 forces it (no accessibility service exists), and only with the bundle flag.
    #[test]
    fn forced_accessibility_duck_is_refused_for_uid_0() {
        let s = app_service(ProcessState::Top);
        let forced = AudioAttributes {
            usage: USAGE_ASSISTANCE_ACCESSIBILITY,
            bundle: Some(Bundle::from_entries([(
                KEY_ACCESSIBILITY_FORCE_FOCUS_DUCKING.to_owned(),
                Value::Bool(true),
            )])),
            ..AudioAttributes::default()
        };
        let plain = AudioAttributes {
            usage: USAGE_ASSISTANCE_ACCESSIBILITY,
            ..AudioAttributes::default()
        };
        let root = |aa: &AudioAttributes| {
            let args = FocusArgs {
                focus_req_type: audio_focus::GAIN_TRANSIENT_MAY_DUCK,
                ..request(aa, 35)
            };
            admit_request(&args, 0, &s.appops, &s.permissions).expect("no exception")
        };
        assert_eq!(
            root(&forced),
            Some("requestAudioFocus (forced accessibility duck: no accessibility service)")
        );
        assert_eq!(root(&plain), None);
    }

    /// Abandon checks the usage only: a denied TAKE_AUDIO_FOCUS (cached app, target 35) does not stop it (AOSP abandon has no hardening gate).
    #[test]
    fn abandon_checks_only_the_usage() {
        assert_eq!(abandon_refusal(Some(&media())), None);
        let system = AudioAttributes {
            usage: 1000,
            ..AudioAttributes::default()
        };
        assert_eq!(
            abandon_refusal(Some(&system)),
            Some("abandonAudioFocus (unsupported usage)")
        );
        assert_eq!(abandon_refusal(None), None);
    }
}
