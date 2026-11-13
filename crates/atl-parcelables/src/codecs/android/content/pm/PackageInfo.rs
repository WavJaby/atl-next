// SPDX-License-Identifier: MIT
//! android.content.pm.PackageInfo: content/pm/PackageInfo.java writeToParcel (:581) / PackageInfo(Parcel) (:659), squashing allowed around the whole value: every component's ApplicationInfo
//! after the first copy is a back-reference (`crate::squash`).
//! Component and permission arrays: writeTypedArray (-1 = null, else the count and each element as writeTypedObject). Not modelled, null only: signatures, configPreferences,
//! reqFeatures, featureGroups, attributions (typed arrays), signingInfo.

use super::ActivityInfo::ActivityInfo;
use super::ApplicationInfo::ApplicationInfo;
use super::InstrumentationInfo::InstrumentationInfo;
use super::PermissionInfo::PermissionInfo;
use super::ProviderInfo::ProviderInfo;
use super::ServiceInfo::ServiceInfo;
use crate::parcel::*;
use crate::parts::*;

/// A writeTypedArray; None = null.
pub type TypedArray<T> = Option<Vec<Option<T>>>;

#[derive(Debug, Clone, Default)]
pub struct PackageInfo {
    pub package_name: Option<String>,
    pub split_names: Option<Vec<Option<String>>>,
    pub version_code: i32,
    pub version_code_major: i32,
    pub version_name: Option<String>,
    pub base_revision_code: i32,
    pub split_revision_codes: Option<Vec<i32>>,
    pub shared_user_id: Option<String>,
    pub shared_user_label: i32,
    pub application_info: Option<ApplicationInfo>,
    pub first_install_time: i64,
    pub last_update_time: i64,
    pub gids: Option<Vec<i32>>,
    pub activities: TypedArray<ActivityInfo>,
    pub receivers: TypedArray<ActivityInfo>,
    pub services: TypedArray<ServiceInfo>,
    pub providers: TypedArray<ProviderInfo>,
    pub instrumentation: TypedArray<InstrumentationInfo>,
    pub permissions: TypedArray<PermissionInfo>,
    pub requested_permissions: Option<Vec<Option<String>>>,
    pub requested_permissions_flags: Option<Vec<i32>>,
    pub install_location: i32,
    pub is_stub: bool,
    pub core_app: bool,
    pub required_for_all_users: bool,
    pub restricted_account_type: Option<String>,
    pub required_account_type: Option<String>,
    pub overlay_target: Option<String>,
    pub overlay_category: Option<String>,
    pub overlay_priority: i32,
    pub overlay_is_static: bool,
    pub compile_sdk_version: i32,
    pub compile_sdk_version_codename: Option<String>,
    pub is_apex: bool,
    pub is_active_apex: bool,
    pub archive_time_millis: i64,
    pub apex_package_name: Option<String>,
}

/// The unmodelled typed arrays after requestedPermissionsFlags, refused when non-null.
const ARRAYS_B: [&str; 5] = [
    "android.content.pm.PackageInfo.signatures",
    "android.content.pm.PackageInfo.configPreferences",
    "android.content.pm.PackageInfo.reqFeatures",
    "android.content.pm.PackageInfo.featureGroups",
    "android.content.pm.PackageInfo.attributions",
];

impl Part for PackageInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        crate::squash::allow(crate::squash::key(p), || self.write_fields(p))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        crate::squash::allow(crate::squash::key(p), || Self::read_fields(p))
    }
}

impl PackageInfo {
    fn write_fields(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.package_name)?;
        w_str8_array(p, &self.split_names)?;
        w_i32(p, self.version_code)?;
        w_i32(p, self.version_code_major)?;
        w_str8(p, &self.version_name)?;
        w_i32(p, self.base_revision_code)?;
        w_i32_array(p, &self.split_revision_codes)?;
        w_str8(p, &self.shared_user_id)?;
        w_i32(p, self.shared_user_label)?;
        // `hasApp` int, then the ApplicationInfo in place (not writeTypedObject: same bytes)
        w_typed(p, &self.application_info)?;
        w_i64(p, self.first_install_time)?;
        w_i64(p, self.last_update_time)?;
        w_i32_array(p, &self.gids)?;
        w_typed_list(p, &self.activities)?;
        w_typed_list(p, &self.receivers)?;
        w_typed_list(p, &self.services)?;
        w_typed_list(p, &self.providers)?;
        w_typed_list(p, &self.instrumentation)?;
        w_typed_list(p, &self.permissions)?;
        w_str8_array(p, &self.requested_permissions)?;
        w_i32_array(p, &self.requested_permissions_flags)?;
        ARRAYS_B.iter().try_for_each(|_| w_null(p))?;
        w_i32(p, self.install_location)?;
        w_bool(p, self.is_stub)?;
        w_bool(p, self.core_app)?;
        w_bool(p, self.required_for_all_users)?;
        w_str8(p, &self.restricted_account_type)?;
        w_str8(p, &self.required_account_type)?;
        w_str8(p, &self.overlay_target)?;
        w_str8(p, &self.overlay_category)?;
        w_i32(p, self.overlay_priority)?;
        w_bool(p, self.overlay_is_static)?;
        w_i32(p, self.compile_sdk_version)?;
        w_str8(p, &self.compile_sdk_version_codename)?;
        w_i32(p, 0)?; // signingInfo
        w_bool(p, self.is_apex)?;
        w_bool(p, self.is_active_apex)?;
        w_i64(p, self.archive_time_millis)?;
        match &self.apex_package_name {
            None => w_i32(p, 0),
            Some(_) => {
                w_i32(p, 1)?;
                w_str8(p, &self.apex_package_name)
            }
        }
    }

    fn read_fields(p: &In<'_>) -> Result<Self> {
        let package_name = r_str8(p)?;
        let split_names = r_str8_array(p)?;
        let version_code = r_i32(p)?;
        let version_code_major = r_i32(p)?;
        let version_name = r_str8(p)?;
        let base_revision_code = r_i32(p)?;
        let split_revision_codes = r_i32_array(p)?;
        let shared_user_id = r_str8(p)?;
        let shared_user_label = r_i32(p)?;
        let application_info = r_typed(p)?;
        let first_install_time = r_i64(p)?;
        let last_update_time = r_i64(p)?;
        let gids = r_i32_array(p)?;
        let activities = r_typed_list(p)?;
        let receivers = r_typed_list(p)?;
        let services = r_typed_list(p)?;
        let providers = r_typed_list(p)?;
        let instrumentation = r_typed_list(p)?;
        let permissions = r_typed_list(p)?;
        let requested_permissions = r_str8_array(p)?;
        let requested_permissions_flags = r_i32_array(p)?;
        for what in ARRAYS_B {
            r_null(p, what)?;
        }
        let install_location = r_i32(p)?;
        let is_stub = r_bool(p)?;
        let core_app = r_bool(p)?;
        let required_for_all_users = r_bool(p)?;
        let restricted_account_type = r_str8(p)?;
        let required_account_type = r_str8(p)?;
        let overlay_target = r_str8(p)?;
        let overlay_category = r_str8(p)?;
        let overlay_priority = r_i32(p)?;
        let overlay_is_static = r_bool(p)?;
        let compile_sdk_version = r_i32(p)?;
        let compile_sdk_version_codename = r_str8(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.content.pm.PackageInfo.signingInfo"));
        }
        let is_apex = r_bool(p)?;
        let is_active_apex = r_bool(p)?;
        let archive_time_millis = r_i64(p)?;
        let apex_package_name = if r_i32(p)? != 0 { r_str8(p)? } else { None };
        Ok(PackageInfo {
            package_name,
            split_names,
            version_code,
            version_code_major,
            version_name,
            base_revision_code,
            split_revision_codes,
            shared_user_id,
            shared_user_label,
            application_info,
            first_install_time,
            last_update_time,
            gids,
            activities,
            receivers,
            services,
            providers,
            instrumentation,
            permissions,
            requested_permissions,
            requested_permissions_flags,
            install_location,
            is_stub,
            core_app,
            required_for_all_users,
            restricted_account_type,
            required_account_type,
            overlay_target,
            overlay_category,
            overlay_priority,
            overlay_is_static,
            compile_sdk_version,
            compile_sdk_version_codename,
            is_apex,
            is_active_apex,
            archive_time_millis,
            apex_package_name,
        })
    }
}

crate::codec!(PackageInfo);
