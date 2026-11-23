// SPDX-License-Identifier: MIT
//! The PendingIntent records of the activity service: the table (pending_intent_table.rs) under a lock, and the IIntentSender binder each record is handed out as.
//! Same key -> same record -> same binder: PendingIntent.equals compares the binder, and AlarmManager matches an alarm by it (alarm_table.rs).
//! IIntentSender.send (PendingIntentRecord.sendInner) of a BROADCAST record sends the request intent through [`IntentSink`] as the creator uid; a SERVICE or FOREGROUND_SERVICE record starts its service the same way (startServiceInPackage).
//! The alarm service makes the same broadcast call in-process when an alarm is due. The activity kind and the activity result kind are not modelled: the call is answered UNKNOWN_TRANSACTION and counted.
//! Not modelled: the send-time intent (AOSP fills it over the request intent; here the request intent is sent, counted as `fill-in`), the send-time options, the finishedReceiver's onSendFinished, requiredPermission, cancel listeners, the sender's pid (this process), the start's result (AOSP ignores it too: PendingIntentRecord :696-700 logs and swallows the exceptions).
//! Records last until cancelled. AOSP lets a record go with its last remote reference (WeakReference, PendingIntentController.java:163-164); here it is held, so a FLAG_NO_CREATE lookup can find a record AOSP would have dropped.
//! userId is part of the key, and a broadcast goes to that user.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::content::IIntentSender::{BnIntentSender, IIntentSender};
use atl_aidl::mangled::_7_android_7_content_15_IIntentReceiver as IIntentReceiver;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::os::Bundle;
use binder::{BinderFeatures, Interface, SpIBinder, Strong};

use crate::pending_intent_table::{Key, Lookup, PendingIntentTable};

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

/// What a sent record goes through: a BROADCAST record's intent to `user_id` as the creator `uid` (activity.rs's broadcast path), and a SERVICE record's start as that uid
/// (`foreground`: FOREGROUND_SERVICE, whose foreground rules are S7's).
pub trait IntentSink: Send + Sync {
    /// `finished` = the sender's finishedReceiver: the broadcast is then ordered, its final result goes back to the calling process (PendingIntentRecord.sendInner :674-677).
    fn send_as(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        finished: Option<&Strong<dyn IIntentReceiver>>,
    ) -> binder::Result<()>;
    fn start_service_as(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        foreground: bool,
    ) -> binder::Result<()>;
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
        _options: Option<&Bundle>,
    ) -> binder::Result<()> {
        let sent = &self.0;
        let kind = sent.r#type;
        if !matches!(
            kind,
            INTENT_SENDER_BROADCAST | INTENT_SENDER_SERVICE | INTENT_SENDER_FOREGROUND_SERVICE
        ) {
            atl_diag::refused("IIntentSender", "send (kind not modelled)");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        if intent.is_some() {
            atl_diag::answered("IIntentSender", "send (sent intent not filled in)");
        }
        let empty = Intent::default();
        let request = sent.intent.as_ref().or(intent).unwrap_or(&empty);
        if kind == INTENT_SENDER_BROADCAST {
            sent.sink
                .send_as(request, sent.uid, sent.user_id, finished_receiver)
        } else {
            let foreground = kind == INTENT_SENDER_FOREGROUND_SERVICE;
            sent.sink
                .start_service_as(request, sent.uid, sent.user_id, foreground)
        }
    }
}

/// A new record's binder.
fn new_binder(sent: Sent) -> SpIBinder {
    BnIntentSender::new_binder(Sender(sent), BinderFeatures::default()).as_binder()
}
