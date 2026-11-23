// SPDX-License-Identifier: MIT
//! The PendingIntent records of the activity service: the table (pending_intent_table.rs) under a lock, and the IIntentSender binder each record is handed out as.
//! Same key -> same record -> same binder: PendingIntent.equals compares the binder, and AlarmManager matches an alarm by it (alarm_table.rs).
//! IIntentSender.send (PendingIntentRecord.sendInner) of a BROADCAST record sends the request intent through [`IntentSink`] as the creator uid; a SERVICE or FOREGROUND_SERVICE record starts its service the same way (startServiceInPackage).
//! The send's BroadcastOptions are read here ([`SendOptions`]): the temp allowlist and the alarm flag, under the caller's permission rules (BroadcastController :965-988, :2158-2168; PendingIntentRecord :579-580, :604-613).
//! The alarm service makes the same send in-process when an alarm is due ([`send_in_process`]): its caller is SYSTEM_UID.
//! The activity kind and the activity result kind are not modelled: the call is answered UNKNOWN_TRANSACTION and counted.
//! Not modelled: the send-time intent (AOSP fills it over the request intent; here the request intent is sent, counted as `fill-in`), the other BroadcastOptions (BAL, delivery groups, ...),
//! the finishedReceiver's onSendFinished, requiredPermission, cancel listeners, the sender's pid (this process), the start's result (AOSP ignores it too: PendingIntentRecord :696-700 logs and swallows the exceptions),
//! the allowlist manager process flag (tempAllowlistForPendingIntentLocked :15716-15733: a caller process holding no permission is refused silently).
//! Records last until cancelled. AOSP lets a record go with its last remote reference (WeakReference, PendingIntentController.java:163-164); here it is held, so a FLAG_NO_CREATE lookup can find a record AOSP would have dropped.
//! userId is part of the key, and a broadcast goes to that user.

use std::cell::Cell;
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::content::IIntentSender::{BnIntentSender, IIntentSender};
use atl_aidl::mangled::_7_android_7_content_15_IIntentReceiver as IIntentReceiver;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_services_types::permission::SYSTEM_UID;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::pending_intent_table::{Key, Lookup, PendingIntentTable};
use crate::uid_state::{self, TempGrant};

/// The interface name counters use.
const INTERFACE: &str = "IIntentSender";

/// PendingIntent.FLAG_NO_CREATE, FLAG_CANCEL_CURRENT, FLAG_UPDATE_CURRENT (PendingIntent.java:225, 237, 253).
const FLAG_NO_CREATE: i32 = 1 << 29;
const FLAG_CANCEL_CURRENT: i32 = 1 << 28;
const FLAG_UPDATE_CURRENT: i32 = 1 << 27;
/// Read by getIntentSender and cleared from the key (PendingIntentController.java:155-156).
const LOOKUP_FLAGS: i32 = FLAG_NO_CREATE | FLAG_CANCEL_CURRENT | FLAG_UPDATE_CURRENT;

/// ActivityManager.INTENT_SENDER_BROADCAST, _ACTIVITY_RESULT, _SERVICE, _FOREGROUND_SERVICE (ActivityManager.java:666, 681, 688, 695).
pub const INTENT_SENDER_BROADCAST: i32 = 1;
pub const INTENT_SENDER_ACTIVITY_RESULT: i32 = 3;
pub const INTENT_SENDER_SERVICE: i32 = 4;
pub const INTENT_SENDER_FOREGROUND_SERVICE: i32 = 5;
/// ActivityManager.INTENT_SENDER_UNKNOWN (ActivityManager.java:659): the type getInfoForIntentSender reports for a sender that is not a record of this service.
pub const INTENT_SENDER_UNKNOWN: i32 = 0;
/// PendingIntent.FLAG_IMMUTABLE (PendingIntent.java:266): not a lookup flag, so it stays in the key, where getInfoForIntentSender reads it.
pub const FLAG_IMMUTABLE: i32 = 1 << 26;

/// BroadcastOptions Bundle keys (BroadcastOptions.java :123-142) and BroadcastOptions.FLAG_IS_ALARM_BROADCAST (:88): set on every alarm send, and only SYSTEM_UID may send it.
pub const KEY_TEMP_DURATION: &str = "android:broadcast.temporaryAppAllowlistDuration";
pub const KEY_TEMP_TYPE: &str = "android:broadcast.temporaryAppAllowlistType";
pub const KEY_TEMP_REASON_CODE: &str = "android:broadcast.temporaryAppAllowlistReasonCode";
pub const KEY_TEMP_REASON: &str = "android:broadcast.temporaryAppAllowlistReason";
pub const KEY_FLAGS: &str = "android:broadcast.flags";
pub const FLAG_IS_ALARM_BROADCAST: i32 = 1 << 3;

/// The permissions that let a sender request a temp allowlist (BroadcastController :965-988; tempAllowlistForPendingIntentLocked :15723-15733).
const TEMP_ALLOWLIST_PERMISSIONS: [&str; 3] = [
    "android.permission.CHANGE_DEVICE_IDLE_TEMP_WHITELIST",
    "android.permission.START_ACTIVITIES_FROM_BACKGROUND",
    "android.permission.START_FOREGROUND_SERVICES_FROM_BACKGROUND",
];

thread_local! {
    /// Set by [`send_in_process`] for its own send: the caller is SYSTEM_UID, not this thread's calling uid. The only thread-local identity in this crate: every other caller is
    /// a parameter (`caller`, [`IntentSink::send_as`]); this one enters through the binder `send`, which reads the calling uid there.
    static OWN_CALL: Cell<bool> = const { Cell::new(false) };
}

/// The BroadcastOptions of a send (PendingIntentRecord.sendInner :579-580, :604-613), read from its Bundle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendOptions {
    /// a temp allowlist was requested (duration > 0): the caller must hold one of the permissions, whatever the grant
    pub temp_requested: bool,
    /// the grant the request makes (uid_state::grant_of); None when it grants nothing
    pub grant: Option<TempGrant>,
    /// FLAG_IS_ALARM_BROADCAST: only SYSTEM_UID may send it
    pub alarm: bool,
}

impl SendOptions {
    /// The options in `options` (None = no Bundle, nothing requested). An unreadable Bundle reads as nothing requested, and counted.
    pub fn read(options: Option<&Bundle>) -> SendOptions {
        let Some(bundle) = options else {
            return SendOptions::default();
        };
        let entries = match bundle.entries() {
            Ok(entries) => entries,
            Err(_) => {
                atl_diag::refused(INTERFACE, "send options (unreadable Bundle)");
                return SendOptions::default();
            }
        };
        let get = |key: &str| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let duration = match get(KEY_TEMP_DURATION) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let int = |key: &str, default: i32| match get(key) {
            Some(Value::Int(v)) => *v,
            _ => default,
        };
        let kind = int(KEY_TEMP_TYPE, uid_state::TEMP_TYPE_NONE);
        if kind == uid_state::TEMP_TYPE_APP_FREEZING_DELAYED {
            atl_diag::answered(
                INTERFACE,
                "temp allowlist type not modelled: APP_FREEZING_DELAYED",
            );
        }
        let grant = uid_state::grant_of(duration, kind, int(KEY_TEMP_REASON_CODE, 0));
        if duration > 0 && grant.is_none() && kind != uid_state::TEMP_TYPE_APP_FREEZING_DELAYED {
            atl_diag::answered(
                INTERFACE,
                "temp allowlist requested, no grant (REASON_DENIED or type)",
            );
        }
        SendOptions {
            temp_requested: duration > 0,
            grant,
            alarm: int(KEY_FLAGS, 0) & FLAG_IS_ALARM_BROADCAST != 0,
        }
    }
}

/// What a sent record goes through: a BROADCAST record's intent to `user_id` as the creator `uid` (activity.rs's broadcast path), and a SERVICE record's start as that uid
/// (`foreground`: FOREGROUND_SERVICE, whose foreground rules are S7's). `caller` is the sender's uid, for the permission rules of [`SendOptions`].
pub trait IntentSink: Send + Sync {
    /// `finished` = the sender's finishedReceiver: the broadcast is then ordered, its final result goes back to the calling process (PendingIntentRecord.sendInner :674-677).
    fn send_as(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        finished: Option<&Strong<dyn IIntentReceiver>>,
        options: &SendOptions,
        caller: u32,
    ) -> binder::Result<()>;
    /// `options.grant` goes to `uid` for a foreground start only (PendingIntentRecord :604-613).
    fn start_service_as(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        foreground: bool,
        options: &SendOptions,
        caller: u32,
    ) -> binder::Result<()>;
}

/// The permissions that let `caller` request a temp allowlist (any of them; a system caller holds all).
pub fn may_request_temp_allowlist(holds: impl Fn(&str) -> bool) -> bool {
    TEMP_ALLOWLIST_PERMISSIONS.iter().any(|p| holds(p))
}

/// A record as [`PendingIntents::info`] reports it.
pub struct RecordInfo {
    pub package: String,
    pub uid: u32,
    pub flags: i32,
    pub r#type: i32,
}

/// The records, under one lock, and the broadcast path a sent record goes through.
pub struct PendingIntents {
    table: Mutex<PendingIntentTable<SpIBinder>>,
    sink: Arc<dyn IntentSink>,
}

impl PendingIntents {
    pub fn new(sink: Arc<dyn IntentSink>) -> Self {
        PendingIntents {
            table: Mutex::new(PendingIntentTable::default()),
            sink,
        }
    }

    fn lock(&self) -> MutexGuard<'_, PendingIntentTable<SpIBinder>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// getIntentSender (PendingIntentController.java:120-195) for `key`, with the flags as the caller sent them. `intent` is the request intent (the last of the sent array, PendingIntentRecord.java:160); an equal record keeps the one it was created with.
    pub fn get_or_create(
        &self,
        mut key: Key<SpIBinder>,
        uid: u32,
        flags: i32,
        intent: Option<Intent>,
    ) -> binder::Result<Option<Strong<dyn IIntentSender>>> {
        key.flags = flags & !LOOKUP_FLAGS;
        let lookup = Lookup {
            no_create: flags & FLAG_NO_CREATE != 0,
            cancel_current: flags & FLAG_CANCEL_CURRENT != 0,
        };
        let sent = Sent {
            r#type: key.r#type,
            uid,
            user_id: key.user_id,
            intent,
            sink: Arc::clone(&self.sink),
        };
        let binder = self
            .lock()
            .get_or_create(key, uid, lookup, || new_binder(sent));
        Ok(binder
            .map(|b| b.into_interface::<dyn IIntentSender>())
            .transpose()?)
    }

    /// The record whose binder is `binder` (its key's package, flags and type, and its creator uid); None when this service holds no such record.
    pub fn info(&self, binder: &SpIBinder) -> Option<RecordInfo> {
        self.lock().find(binder).map(|r| RecordInfo {
            package: r.key.package.clone(),
            uid: r.uid,
            flags: r.key.flags,
            r#type: r.key.r#type,
        })
    }

    /// Cancels the record whose binder is `binder`; no such record, nothing.
    pub fn cancel(&self, binder: &SpIBinder) {
        self.lock().remove(binder);
    }
}

/// PendingIntent.send from the services themselves (AlarmManagerService's mPendingIntent.send) on the record's `operation` binder, with `options` as the Bundle.
/// The caller is SYSTEM_UID: the services' own thread runs no transaction, and ATL's process uid is the desktop user's, not necessarily SYSTEM_UID.
pub fn send_in_process(operation: &SpIBinder, options: &Bundle) -> binder::Result<()> {
    let sender = operation.clone().into_interface::<dyn IIntentSender>()?;
    let _own = OwnCall::enter();
    sender.r#send(0, None, None, None, None, None, Some(options))
}

/// Holds OWN_CALL for one send. Drop restores the previous value, so an error return and an unwinding panic clear it as well.
struct OwnCall(bool);

impl OwnCall {
    fn enter() -> Self {
        Self(OWN_CALL.replace(true))
    }
}

impl Drop for OwnCall {
    fn drop(&mut self) {
        OWN_CALL.set(self.0);
    }
}

/// What a record sends: its kind, creator uid, user, request intent and the broadcast path.
struct Sent {
    r#type: i32,
    uid: u32,
    user_id: i32,
    intent: Option<Intent>,
    sink: Arc<dyn IntentSink>,
}

/// The IIntentSender a record is handed out as: send (module doc).
struct Sender(Sent);

impl Interface for Sender {}

impl IIntentSender for Sender {
    fn r#send(
        &self,
        _code: i32,
        intent: Option<&Intent>,
        _resolved_type: Option<&str>,
        _whitelist_token: Option<&SpIBinder>,
        finished_receiver: Option<&Strong<dyn IIntentReceiver>>,
        _required_permission: Option<&str>,
        options: Option<&Bundle>,
    ) -> binder::Result<()> {
        let sent = &self.0;
        let kind = sent.r#type;
        if !matches!(
            kind,
            INTENT_SENDER_BROADCAST | INTENT_SENDER_SERVICE | INTENT_SENDER_FOREGROUND_SERVICE
        ) {
            atl_diag::refused(INTERFACE, "send (kind not modelled)");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        if intent.is_some() {
            atl_diag::answered(INTERFACE, "send (sent intent not filled in)");
        }
        let caller = if OWN_CALL.get() {
            SYSTEM_UID
        } else {
            ThreadState::get_calling_uid()
        };
        let options = SendOptions::read(options);
        let empty = Intent::default();
        let request = sent.intent.as_ref().or(intent).unwrap_or(&empty);
        if kind == INTENT_SENDER_BROADCAST {
            // BroadcastController :2158-2168, via PendingIntentRecord :579-580
            if options.alarm && caller != SYSTEM_UID {
                atl_diag::refused(INTERFACE, "send (alarm flag from a non-system caller)");
                return Err(Status::new_exception_str(
                    ExceptionCode::SECURITY,
                    Some("Non-system callers may not flag broadcasts as alarm".to_owned()),
                ));
            }
            sent.sink.send_as(
                request,
                sent.uid,
                sent.user_id,
                finished_receiver,
                &options,
                caller,
            )
        } else {
            let foreground = kind == INTENT_SENDER_FOREGROUND_SERVICE;
            sent.sink.start_service_as(
                request,
                sent.uid,
                sent.user_id,
                foreground,
                &options,
                caller,
            )
        }
    }
}

/// A new record's binder.
fn new_binder(sent: Sent) -> SpIBinder {
    BnIntentSender::new_binder(Sender(sent), BinderFeatures::default()).as_binder()
}
