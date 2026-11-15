// SPDX-License-Identifier: MIT
//! The `system_config` service (SystemConfigService's ISystemConfig, frameworks/base/services/java/com/android/server/SystemConfigService.java): SettingsProvider's first-boot defaults
//! (SettingsProvider.UpgradeController) ask it for the default VR components. getDefaultVrComponents enforces QUERY_ALL_PACKAGES, then answers SystemConfig's `<default-enabled-vr-app>`
//! entries; no permission file of this runtime declares one (system_config.rs in atl-services-types reads features only; the runtime's sysconfig files are atld's derived feature file), so the
//! list is empty. Every other method stays counted.

use std::sync::Arc;

use atl_aidl::aidl::android::os::ISystemConfig::{BnSystemConfig, ISystemConfig};
use atl_parcelables::android::content::ComponentName;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;

/// Manifest.permission.QUERY_ALL_PACKAGES.
const QUERY_ALL_PACKAGES: &str = "android.permission.QUERY_ALL_PACKAGES";

struct SystemConfigService {
    permissions: Arc<Permissions>,
}

impl Interface for SystemConfigService {}

impl ISystemConfig for SystemConfigService {
    /// SystemConfigService.getDefaultVrComponents:110-114: enforceCallingOrSelfPermission(QUERY_ALL_PACKAGES), then SystemConfig.getDefaultVrComponents (empty here, see the module doc).
    fn r#getDefaultVrComponents(&self) -> binder::Result<Option<Vec<Option<ComponentName>>>> {
        let uid = ThreadState::get_calling_uid();
        if !self.permissions.granted(uid, QUERY_ALL_PACKAGES) {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!("Caller must hold {QUERY_ALL_PACKAGES}")),
            ));
        }
        Ok(Some(Vec::new()))
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnSystemConfig::new_binder(SystemConfigService { permissions }, BinderFeatures::default())
        .as_binder()
}
