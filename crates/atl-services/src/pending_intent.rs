// SPDX-License-Identifier: MIT
//! The PendingIntent records of the activity service: the table (pending_intent_table.rs) under a lock, and the IIntentSender binder each record is handed out as.
//! send is not modelled: the stub default, counted UNKNOWN (counters.rs), so a PendingIntent that would fire shows in the log. Not modelled: the send-time data (intent extras, options), cancel listeners, and the activity-result kind, which AOSP gives ActivityTaskManager (activity.rs refuses it).
//! Records last until cancelled. AOSP lets a record go with its last remote reference (WeakReference, PendingIntentController.java:163-164); here it is held, so a FLAG_NO_CREATE lookup can find a record AOSP would have dropped.
//! userId is part of the key only: one user.

use std::sync::{Mutex, MutexGuard};

use atl_aidl::aidl::android::content::IIntentSender::{BnIntentSender, IIntentSender};
use binder::{BinderFeatures, Interface, SpIBinder, Status, Strong};

use crate::pending_intent_table::{Key, Lookup, PendingIntentTable};

/// The interface name counters use.
const INTERFACE: &str = "IIntentSender";

/// PendingIntent.FLAG_NO_CREATE, FLAG_CANCEL_CURRENT, FLAG_UPDATE_CURRENT (PendingIntent.java:225, 237, 253).
const FLAG_NO_CREATE: i32 = 1 << 29;
const FLAG_CANCEL_CURRENT: i32 = 1 << 28;
const FLAG_UPDATE_CURRENT: i32 = 1 << 27;
/// Read by getIntentSender and cleared from the key (PendingIntentController.java:155-156).
const LOOKUP_FLAGS: i32 = FLAG_NO_CREATE | FLAG_CANCEL_CURRENT | FLAG_UPDATE_CURRENT;

/// ActivityManager.INTENT_SENDER_BROADCAST and INTENT_SENDER_ACTIVITY_RESULT (ActivityManager.java:666, 681).
pub const INTENT_SENDER_BROADCAST: i32 = 1;
pub const INTENT_SENDER_ACTIVITY_RESULT: i32 = 3;

/// The records, under one lock.
#[derive(Default)]
pub struct PendingIntents {
    table: Mutex<PendingIntentTable<SpIBinder>>,
}

impl PendingIntents {
    fn lock(&self) -> MutexGuard<'_, PendingIntentTable<SpIBinder>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// getIntentSender (PendingIntentController.java:120-195) for `key`, with the flags as the caller sent them.
    pub fn get_or_create(
        &self,
        mut key: Key<SpIBinder>,
        uid: u32,
        flags: i32,
    ) -> binder::Result<Option<Strong<dyn IIntentSender>>> {
        key.flags = flags & !LOOKUP_FLAGS;
        let lookup = Lookup {
            no_create: flags & FLAG_NO_CREATE != 0,
            cancel_current: flags & FLAG_CANCEL_CURRENT != 0,
        };
        let binder = self.lock().get_or_create(key, uid, lookup, new_binder);
        Ok(binder
            .map(|b| b.into_interface::<dyn IIntentSender>())
            .transpose()?)
    }

    /// The creator uid of the record whose binder is `binder`; None when this service holds no such record.
    pub fn creator_uid(&self, binder: &SpIBinder) -> Option<u32> {
        self.lock().find(binder).map(|r| r.uid)
    }

    /// Cancels the record whose binder is `binder`; no such record, nothing.
    pub fn cancel(&self, binder: &SpIBinder) {
        self.lock().remove(binder);
    }
}

/// The IIntentSender a record is handed out as. Only its identity matters here: send is counted (module doc).
struct Sender;

impl Interface for Sender {}

impl IIntentSender for Sender {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        atl_services_wire::unimplemented(INTERFACE, method)
    }
}

/// A new record's binder.
fn new_binder() -> SpIBinder {
    BnIntentSender::new_binder(Sender, BinderFeatures::default()).as_binder()
}
