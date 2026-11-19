// SPDX-License-Identifier: MIT
//! The `telephony.registry` service (ITelephonyRegistry): TelephonyRegistryManager reaches it for TelephonyManager.registerTelephonyCallback / listen and SubscriptionManager's subscription listeners. Without it
//! `sRegistry` is null and the first registration NPEs (WeChat's mars PlatformComm.init, then System.exit(1)). SystemServer adds it on every device, telephony or not (SystemServer.java:1606-1609).
//! Behaviour = TelephonyRegistry on a device with no modem (a Wi-Fi tablet; `android.hardware.telephony` undeclared): getActiveModemCount() is 0, so validatePhoneId is false for every phone and
//! listen's notifyNow delivers nothing (TelephonyRegistry.java:1219, :4517-4522); no phone process ever calls notifySubscriptionInfoChanged, so mHasNotifySubscriptionInfoChangedOccurred stays false and
//! a subscriptions listener gets no initial onSubscriptionsChanged (:1006-1018). Registrations are accepted and never notified; an empty event list unregisters (:1167-1174), as remove does.
//! Gates kept: mAppOps.checkPackage (the package must be the caller's: SecurityException otherwise) and checkListenerPermission's throwing branches (READ_PRECISE_PHONE_STATE, READ_ACTIVE_EMERGENCY_SESSION,
//! READ_PRIVILEGED_PHONE_STATE event sets, :4428-4482); READ_PHONE_STATE's branch only skips the registration (returns false), which is unobservable with no notifications. Not modelled: the location
//! check (LocationAccessPolicy DENIED_HARD), carrier privileges as an alternative to READ_PRECISE_PHONE_STATE (no SIM, so none). Every notify* and the carrier / satellite listeners are the phone process's
//! side or need a SIM: not implemented, so they answer UNKNOWN_TRANSACTION (counted by atl-diag's reply hook).
//! Rung 6 of "AOSP's own code first": TelephonyRegistry runs in system_server (services/core), not in the app process.

use std::sync::Arc;

use atl_aidl::aidl::com::android::internal::telephony::IOnSubscriptionsChangedListener::IOnSubscriptionsChangedListener;
use atl_aidl::aidl::com::android::internal::telephony::IPhoneStateListener::IPhoneStateListener;
use atl_aidl::aidl::com::android::internal::telephony::ITelephonyRegistry::{
    BnTelephonyRegistry, ITelephonyRegistry,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::packages::{Packages, Permissions};

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const TELEPHONY_REGISTRY: &str = "ITelephonyRegistry";

/// TelephonyCallback.EVENT_* ids (TelephonyCallback.java:267-741) of TelephonyRegistry's REQUIRE_PRECISE_PHONE_STATE_PERMISSION set (TelephonyRegistry.java:504-526): PRECISE_CALL_STATE 12,
/// PRECISE_DATA_CONNECTION_STATE 13, DATA_CONNECTION_REAL_TIME_INFO 14, CALL_DISCONNECT_CAUSE 26, CALL_ATTRIBUTES 27, IMS_CALL_DISCONNECT_CAUSE 28, REGISTRATION_FAILURE 31, BARRING_INFO 32,
/// PHYSICAL_CHANNEL_CONFIG 33, DATA_ENABLED 34, LINK_CAPACITY_ESTIMATE 37, MEDIA_QUALITY_STATUS 39.
const PRECISE_EVENTS: [i32; 12] = [12, 13, 14, 26, 27, 28, 31, 32, 33, 34, 37, 39];
/// isActiveEmergencySessionPermissionRequired: OUTGOING_EMERGENCY_CALL 29, OUTGOING_EMERGENCY_SMS 30.
const EMERGENCY_EVENTS: [i32; 2] = [29, 30];
/// isPrivilegedPhoneStatePermissionRequired: SRVCC_STATE 16, VOICE_ACTIVATION_STATE 18, RADIO_POWER_STATE 24, ALLOWED_NETWORK_TYPE_LIST 35, EMERGENCY_CALLBACK_MODE 40,
/// SIMULTANEOUS_CELLULAR_CALLING_SUBSCRIPTIONS 41, CELLULAR_IDENTIFIER_DISCLOSED 47, SECURITY_ALGORITHMS 46.
const PRIVILEGED_EVENTS: [i32; 8] = [16, 18, 24, 35, 40, 41, 46, 47];
const READ_PRECISE_PHONE_STATE: &str = "android.permission.READ_PRECISE_PHONE_STATE";
const READ_ACTIVE_EMERGENCY_SESSION: &str = "android.permission.READ_ACTIVE_EMERGENCY_SESSION";
const READ_PRIVILEGED_PHONE_STATE: &str = "android.permission.READ_PRIVILEGED_PHONE_STATE";

/// The permission each throwing branch of checkListenerPermission enforces, in AOSP's order, for the events that need it.
fn required_permissions(events: &[i32]) -> Vec<&'static str> {
    let needs = |set: &[i32]| events.iter().any(|e| set.contains(e));
    [
        (&PRECISE_EVENTS[..], READ_PRECISE_PHONE_STATE),
        (&EMERGENCY_EVENTS[..], READ_ACTIVE_EMERGENCY_SESSION),
        (&PRIVILEGED_EVENTS[..], READ_PRIVILEGED_PHONE_STATE),
    ]
    .into_iter()
    .filter(|(set, _)| needs(set))
    .map(|(_, p)| p)
    .collect()
}

struct TelephonyRegistry {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
}

impl Interface for TelephonyRegistry {}

fn security(message: String) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(message))
}

impl TelephonyRegistry {
    /// AppOpsManager.checkPackage(callingUid, package): the package must be installed, visible to the caller and the caller's own; else SecurityException.
    fn check_package(&self, method: &str, package: Option<&str>) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        match package {
            Some(p)
                if self.packages.visible(uid, p).is_some() && self.packages.is_same_app(p, uid) =>
            {
                Ok(())
            }
            _ => Err(security(format!(
                "{method}: package {package:?} does not belong to uid {uid}"
            ))),
        }
    }

    /// add(): a subscriptions listener registered and, with no notification ever having occurred, not called back.
    fn add_subscriptions_listener(
        &self,
        method: &str,
        package: Option<&str>,
    ) -> binder::Result<()> {
        self.check_package(method, package)?;
        atl_diag::answered(
            TELEPHONY_REGISTRY,
            &format!("{method} (no modem: never notified)"),
        );
        Ok(())
    }
}

impl ITelephonyRegistry for TelephonyRegistry {
    fn r#addOnSubscriptionsChangedListener(
        &self,
        pkg: Option<&str>,
        _feature_id: Option<&str>,
        _callback: Option<&Strong<dyn IOnSubscriptionsChangedListener>>,
    ) -> binder::Result<()> {
        self.add_subscriptions_listener("addOnSubscriptionsChangedListener", pkg)
    }
    fn r#addOnOpportunisticSubscriptionsChangedListener(
        &self,
        pkg: Option<&str>,
        _feature_id: Option<&str>,
        _callback: Option<&Strong<dyn IOnSubscriptionsChangedListener>>,
    ) -> binder::Result<()> {
        self.add_subscriptions_listener("addOnOpportunisticSubscriptionsChangedListener", pkg)
    }
    /// remove(callback.asBinder()): no record is kept, so nothing to drop.
    fn r#removeOnSubscriptionsChangedListener(
        &self,
        _pkg: Option<&str>,
        _callback: Option<&Strong<dyn IOnSubscriptionsChangedListener>>,
    ) -> binder::Result<()> {
        atl_diag::answered(TELEPHONY_REGISTRY, "removeOnSubscriptionsChangedListener");
        Ok(())
    }
    /// listen(): checkPackage, then an empty list unregisters; else the throwing permission branches, then the registration (no phone is valid, so notifyNow delivers nothing).
    fn r#listenWithEventList(
        &self,
        _renounce_fine_location_access: bool,
        _renounce_coarse_location_access: bool,
        _sub_id: i32,
        calling_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        _callback: Option<&Strong<dyn IPhoneStateListener>>,
        events: Option<&[i32]>,
        _notify_now: bool,
    ) -> binder::Result<()> {
        // listenWithEventList's Arrays.stream(events) throws NullPointerException on null, before listen().
        let events = events.ok_or_else(|| {
            Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("listenWithEventList: events is null"),
            )
        })?;
        self.check_package("listenWithEventList", calling_package)?;
        if events.is_empty() {
            atl_diag::answered(TELEPHONY_REGISTRY, "listenWithEventList (unregistered)");
            return Ok(());
        }
        let uid = ThreadState::get_calling_uid();
        for permission in required_permissions(events) {
            if !self.permissions.holds(uid, &[permission], false) {
                return Err(security(format!(
                    "listenWithEventList: events {events:?} need {permission}"
                )));
            }
        }
        atl_diag::answered(
            TELEPHONY_REGISTRY,
            "listenWithEventList (no modem: never notified)",
        );
        Ok(())
    }
}

/// The `telephony.registry` binder, for `addService`.
pub fn new_binder(packages: Arc<Packages>, permissions: Arc<Permissions>) -> SpIBinder {
    BnTelephonyRegistry::new_binder(
        TelephonyRegistry {
            packages,
            permissions,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TelephonyRegistry.java:504-526 and :4497-4511: a plain registration (SERVICE_STATE 1, CALL_STATE 6, DATA_CONNECTION_STATE 7) needs no throwing permission; PRECISE_CALL_STATE 12 needs
    /// READ_PRECISE_PHONE_STATE; OUTGOING_EMERGENCY_SMS 30 and RADIO_POWER_STATE 24 together need the emergency then the privileged one, in checkListenerPermission's order.
    #[test]
    fn listener_permissions_follow_aosp_event_sets() {
        assert!(required_permissions(&[1, 6, 7]).is_empty());
        assert_eq!(required_permissions(&[1, 12]), [READ_PRECISE_PHONE_STATE]);
        assert_eq!(
            required_permissions(&[24, 30]),
            [READ_ACTIVE_EMERGENCY_SESSION, READ_PRIVILEGED_PHONE_STATE]
        );
    }
}
