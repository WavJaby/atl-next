// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/BroadcastQueueImpl.java
//! The delivery side of BroadcastQueueImpl, P1 subset: one thread sends each delivery to its process as scheduleReceiverWarmLocked (:1066) does, a registered receiver through
//! IApplicationThread.scheduleRegisteredReceiver (:1211), a manifest receiver through scheduleReceiver (:1227). Both are oneway.
//! Order: the activity service enqueues under its state lock, so the channel's order is the lock's; one sender thread keeps it, which gives each process its deliveries in FIFO order (AOSP's
//! BroadcastProcessQueue guarantee for non-ordered broadcasts; no order across processes, as in AOSP). No call leaves the system process with the lock held.
//! Not modelled: delivery timeouts and ANRs (BroadcastConstants.TIMEOUT: nothing waits on a non-ordered delivery here; a manifest receiver's finishReceiver is accepted and counted), process-state
//! accounting (processState is PROCESS_STATE_UNKNOWN), shareIdentity (sentFromUid INVALID_UID, sentFromPackage null), the kill of an app a delivery fails to (:1239: logged and counted).

use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};

use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_aidl::aidl::android::content::IIntentReceiver::IIntentReceiver;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::content::pm::ActivityInfo;
use atl_parcelables::android::os::Bundle;
use binder::Strong;

use crate::activity::log;
use atl_services_types::counters;

const BQ: &str = "BroadcastQueue";
/// ActivityManager.PROCESS_STATE_UNKNOWN: the reported process state, not tracked.
const PROCESS_STATE_UNKNOWN: i32 = -1;
/// Process.INVALID_UID: sentFromUid without shareIdentity.
const INVALID_UID: i32 = -1;

/// What one receiver gets (BroadcastRecord + its receiver at `index`, reduced to the wire arguments).
pub(crate) enum Job {
    /// a BroadcastFilter: r.ordered is false and resultTo null, so assumeDelivered (BroadcastRecord.isAssumedDelivered:271-274)
    Registered {
        receiver: Strong<dyn IIntentReceiver>,
        intent: Intent,
        result_code: i32,
        data: Option<String>,
        extras: Option<Bundle>,
        /// r.initialSticky: a sticky replayed on registration
        initial_sticky: bool,
        user: i32,
    },
    /// a manifest receiver (ResolveInfo.activityInfo): not assumed delivered, the app answers finishReceiver
    Manifest {
        intent: Intent,
        info: Box<ActivityInfo>,
        result_code: i32,
        data: Option<String>,
        extras: Option<Bundle>,
        user: i32,
    },
}

/// The sender thread and its channel.
pub(crate) struct Queue {
    tx: Mutex<Sender<(Strong<dyn IApplicationThread>, Job)>>,
}

impl Queue {
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel::<(Strong<dyn IApplicationThread>, Job)>();
        std::thread::Builder::new()
            .name(BQ.into())
            .spawn(move || {
                for (thread, job) in rx {
                    if let Err(e) = deliver(&thread, &job) {
                        counters::answered(BQ, "delivery failed (the app is not killed)");
                        log(format_args!("Failed to schedule broadcast: {e}"));
                    }
                }
            })
            .expect("starting the BroadcastQueue thread");
        Queue { tx: Mutex::new(tx) }
    }

    /// Hands `job` for `thread`'s process to the sender, in call order.
    pub(crate) fn enqueue(&self, thread: Strong<dyn IApplicationThread>, job: Job) {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        // the receiver ends with the process; a failed send means shutdown
        let _ = tx.send((thread, job));
    }
}

/// scheduleReceiverWarmLocked's two wire calls (:1209-1232).
fn deliver(thread: &Strong<dyn IApplicationThread>, job: &Job) -> binder::Result<()> {
    match job {
        Job::Registered {
            receiver,
            intent,
            result_code,
            data,
            extras,
            initial_sticky,
            user,
        } => {
            counters::answered(BQ, "scheduleRegisteredReceiver");
            thread.r#scheduleRegisteredReceiver(
                Some(receiver),
                Some(intent),
                *result_code,
                data.as_deref(),
                extras.as_ref(),
                false,
                *initial_sticky,
                true,
                *user,
                PROCESS_STATE_UNKNOWN,
                INVALID_UID,
                None,
            )
        }
        Job::Manifest {
            intent,
            info,
            result_code,
            data,
            extras,
            user,
        } => {
            counters::answered(BQ, "scheduleReceiver");
            thread.r#scheduleReceiver(
                Some(intent),
                Some(info),
                None,
                *result_code,
                data.as_deref(),
                extras.as_ref(),
                false,
                false,
                *user,
                PROCESS_STATE_UNKNOWN,
                INVALID_UID,
                None,
            )
        }
    }
}
