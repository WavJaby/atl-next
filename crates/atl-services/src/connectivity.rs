// SPDX-License-Identifier: MIT
//! The `connectivity` service (Context.CONNECTIVITY_SERVICE): ConnectivityFrameworkInitializer.registerServiceWrappers builds ConnectivityManager only from this binder; without it
//! getSystemService returns null and apps that never null-check it throw (Cut the Rope's ZSystemInfo.isNetworkAvailable from a native call, leaving the exception pending so every later JNI call fails).
//! Models a device with no network agent: ConnectivityService answers then as below. Network state from the host (platform layer) is not built; every other method stays counted
//! (UNKNOWN_TRANSACTION -> the Java proxy reads null / 0 / false). Known divergence: getNetworkInfo(type) answers null where AOSP makes a fake DISCONNECTED NetworkInfo
//! (getFilteredNetworkInfoForType -> makeFakeNetworkInfo), which needs a NetworkInfo codec.

use atl_aidl::aidl::android::net::IConnectivityManager::{
    BnConnectivityManager, IConnectivityManager,
};
use atl_parcelables::android::net::{Network, NetworkInfo};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};
use std::sync::Arc;

use crate::counters;
use crate::permission::Permissions;

/// The interface name counters use.
const INTERFACE: &str = "IConnectivityManager";
/// ConnectivityService.enforceAccessPermission: every query below needs it.
const ACCESS_NETWORK_STATE: &str = "android.permission.ACCESS_NETWORK_STATE";

struct ConnectivityService {
    permissions: Arc<Permissions>,
}

impl ConnectivityService {
    /// ConnectivityService.enforceAccessPermission (enforceCallingOrSelfPermission -> SecurityException).
    fn enforce_access(&self, method: &str) -> binder::Result<()> {
        if self.permissions.holds(
            ThreadState::get_calling_uid(),
            &[ACCESS_NETWORK_STATE],
            false,
        ) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {ACCESS_NETWORK_STATE}")),
        ))
    }
}

impl Interface for ConnectivityService {}

impl IConnectivityManager for ConnectivityService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// ConnectivityService.getActiveNetworkInfo: no agent for the uid -> null.
    fn r#getActiveNetworkInfo(&self) -> binder::Result<Option<NetworkInfo>> {
        self.enforce_access("getActiveNetworkInfo")?;
        Ok(None)
    }

    /// ConnectivityService.getActiveNetwork -> getActiveNetworkForUidInternal: no default network -> null.
    fn r#getActiveNetwork(&self) -> binder::Result<Option<Network>> {
        self.enforce_access("getActiveNetwork")?;
        Ok(None)
    }

    /// ConnectivityService.getAllNetworks: one entry per mNetworkForNetId, none here.
    fn r#getAllNetworks(&self) -> binder::Result<Option<Vec<Option<Network>>>> {
        self.enforce_access("getAllNetworks")?;
        Ok(Some(Vec::new()))
    }

    /// ConnectivityService.isActiveNetworkMetered: no active network capabilities -> true ("the most conservative value").
    fn r#isActiveNetworkMetered(&self) -> binder::Result<bool> {
        self.enforce_access("isActiveNetworkMetered")?;
        Ok(true)
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnConnectivityManager::new_binder(
        ConnectivityService { permissions },
        BinderFeatures::default(),
    )
    .as_binder()
}
