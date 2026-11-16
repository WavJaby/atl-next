// SPDX-License-Identifier: MIT
//! The `trust` service (Context.TRUST_SERVICE): KeyguardManager's and TrustManager's constructors look it up (getServiceOrThrow). Without it getSystemService(KEYGUARD_SERVICE) returns null (the failed lookup logs "No service published for: trust") and the app's first isDeviceLocked NPEs (Battle Cats TW, Google Mobile Ads zzflu.zzc).
//! Behaviour = TrustManagerService for a device with no lock-screen credential that is unlocked. refreshDeviceLockedForUser sets a user's locked state only for a secure user (`secure && showingKeyguard && !trusted && !biometricAuthenticated`, TrustManagerService.java:1047-1085), and LockPatternUtils.isSecure is false without a credential, so isDeviceLocked and isDeviceSecure are false for every user and device (:1867-1904).
//! No lock, trust agent or biometric unlock is ever reported here, so the reports and the listener registrations are accepted and counted: AOSP would never notify a listener. Their gates are AOSP's (enforceReportPermission, enforceListenerPermission, the @EnforcePermission annotations through the enforce_permission hook): a caller without the permission gets SecurityException.
//! Not modelled: setDeviceLockedForUser(locked = true) changes no answer, since no locked state is kept (only system components call it). Trust agents, significant places and active unlock providers do not exist on the desktop, so their answers are AOSP's empty defaults.
//! The desktop has no Android lock screen; the desktop's own lock is the OS's (behaviour decision 3). Rung 6 of "AOSP's own code first": TrustManagerService runs in system_server (services/core), not in the app process, so the service is reproduced minimally, as the other system services are (AGENTS: system services are Rust by default).

use std::sync::Arc;

use atl_aidl::aidl::android::app::trust::ITrustListener::ITrustListener;
use atl_aidl::aidl::android::app::trust::ITrustManager::{
    BnTrustManager, EnforcePermission, ITrustManager,
};
use atl_aidl::aidl::com::android::internal::policy::IDeviceLockedStateListener::IDeviceLockedStateListener;
use atl_parcelables::android::hardware::biometrics::BiometricSourceType;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::packages::Permissions;

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const TRUST_MANAGER: &str = "ITrustManager";
/// The permissions of AOSP's hand-written gates (TrustManagerService.enforceReportPermission, enforceListenerPermission).
const ACCESS_KEYGUARD_SECURE_STORAGE: &str = "android.permission.ACCESS_KEYGUARD_SECURE_STORAGE";
const TRUST_LISTENER: &str = "android.permission.TRUST_LISTENER";

struct TrustService {
    permissions: Arc<Permissions>,
}

impl Interface for TrustService {}

impl TrustService {
    /// Root / system, or a caller whose install-time grants hold `permissions` (Permissions::holds, rule A); else SecurityException.
    fn check(&self, method: &str, permissions: &[&str], any: bool) -> binder::Result<()> {
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), permissions, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permissions:?}")),
        ))
    }

    /// A report or a listener call gated by `permission`: nothing to change, so accepted and counted.
    fn accept(&self, method: &str, permission: &str) -> binder::Result<()> {
        self.check(method, &[permission], false)?;
        atl_diag::answered(
            TRUST_MANAGER,
            &format!("{method} (accepted: nothing to change)"),
        );
        Ok(())
    }
}

impl ITrustManager for TrustService {
    /// The @EnforcePermission methods: TRUST_LISTENER, ACCESS_FINE_LOCATION, SUBSCRIBE_TO_KEYGUARD_LOCKED_STATE.
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
        self.check(method, perms, any)
    }

    fn r#reportUnlockAttempt(&self, _successful: bool, _user_id: i32) -> binder::Result<()> {
        self.accept("reportUnlockAttempt", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#reportUserRequestedUnlock(
        &self,
        _user_id: i32,
        _dismiss_keyguard: bool,
    ) -> binder::Result<()> {
        self.accept("reportUserRequestedUnlock", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#reportUserMayRequestUnlock(&self, _user_id: i32) -> binder::Result<()> {
        self.accept("reportUserMayRequestUnlock", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#reportUnlockLockout(&self, _timeout_ms: i32, _user_id: i32) -> binder::Result<()> {
        self.accept("reportUnlockLockout", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#reportEnabledTrustAgentsChanged(&self, _user_id: i32) -> binder::Result<()> {
        self.accept(
            "reportEnabledTrustAgentsChanged",
            ACCESS_KEYGUARD_SECURE_STORAGE,
        )
    }
    fn r#reportKeyguardShowingChanged(&self) -> binder::Result<()> {
        self.accept(
            "reportKeyguardShowingChanged",
            ACCESS_KEYGUARD_SECURE_STORAGE,
        )
    }
    fn r#setDeviceLockedForUser(&self, _user_id: i32, _locked: bool) -> binder::Result<()> {
        self.accept("setDeviceLockedForUser", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#unlockedByBiometricForUser(
        &self,
        _user_id: i32,
        _source: Option<&BiometricSourceType>,
    ) -> binder::Result<()> {
        self.accept("unlockedByBiometricForUser", ACCESS_KEYGUARD_SECURE_STORAGE)
    }
    fn r#clearAllBiometricRecognized(
        &self,
        _target: Option<&BiometricSourceType>,
        _unlocked_user: i32,
    ) -> binder::Result<()> {
        self.accept(
            "clearAllBiometricRecognized",
            ACCESS_KEYGUARD_SECURE_STORAGE,
        )
    }
    fn r#registerTrustListener(
        &self,
        _listener: Option<&Strong<dyn ITrustListener>>,
    ) -> binder::Result<()> {
        self.accept("registerTrustListener", TRUST_LISTENER)
    }
    fn r#unregisterTrustListener(
        &self,
        _listener: Option<&Strong<dyn ITrustListener>>,
    ) -> binder::Result<()> {
        self.accept("unregisterTrustListener", TRUST_LISTENER)
    }

    /// TrustManagerService.isDeviceLocked: false for a secure-less user (see the module doc); the device and user do not change it.
    fn r#isDeviceLocked(&self, _user_id: i32, _device_id: i32) -> binder::Result<bool> {
        atl_diag::answered(TRUST_MANAGER, "isDeviceLocked (no credential: false)");
        Ok(false)
    }
    /// TrustManagerService.isDeviceSecure: LockPatternUtils.isSecure, false without a credential.
    fn r#isDeviceSecure(&self, _user_id: i32, _device_id: i32) -> binder::Result<bool> {
        atl_diag::answered(TRUST_MANAGER, "isDeviceSecure (no credential: false)");
        Ok(false)
    }
    /// The persisted per-user flag, set only by trust agents: none here, so false.
    fn r#isTrustUsuallyManaged(&self, _user_id: i32) -> binder::Result<bool> {
        atl_diag::answered(
            TRUST_MANAGER,
            "isTrustUsuallyManaged (no trust agent: false)",
        );
        Ok(false)
    }
    /// aggregateIsActiveUnlockRunning over the active unlock providers: none here, so false.
    fn r#isActiveUnlockRunning(&self, _user_id: i32) -> binder::Result<bool> {
        atl_diag::answered(TRUST_MANAGER, "isActiveUnlockRunning (no provider: false)");
        Ok(false)
    }
    /// mIsInSignificantPlace, set only by the significant-place provider: none here, so false.
    fn r#isInSignificantPlace(&self) -> binder::Result<bool> {
        atl_diag::answered(TRUST_MANAGER, "isInSignificantPlace (no provider: false)");
        Ok(false)
    }
    fn r#registerDeviceLockedStateListener(
        &self,
        _listener: Option<&Strong<dyn IDeviceLockedStateListener>>,
        _device_id: i32,
    ) -> binder::Result<()> {
        atl_diag::answered(
            TRUST_MANAGER,
            "registerDeviceLockedStateListener (accepted: never notified)",
        );
        Ok(())
    }
    fn r#unregisterDeviceLockedStateListener(
        &self,
        _listener: Option<&Strong<dyn IDeviceLockedStateListener>>,
    ) -> binder::Result<()> {
        atl_diag::answered(
            TRUST_MANAGER,
            "unregisterDeviceLockedStateListener (accepted: never notified)",
        );
        Ok(())
    }
}

/// The `trust` binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnTrustManager::new_binder(TrustService { permissions }, BinderFeatures::default()).as_binder()
}
