// SPDX-License-Identifier: MIT
//! android.content.pm.ResolveInfo: content/pm/ResolveInfo.java writeToParcel (:478) / ResolveInfo(Parcel) (:525). Wire: a component tag (1 activityInfo, 2 serviceInfo, 3 providerInfo, 0 none) and
//! that info's writeToParcel; a presence int + the IntentFilter; priority, preferredOrder, match, specificIndex, labelRes, nonLocalizedLabel (TextUtils.writeToParcel), icon, resolvePackageName
//! (String8), targetUserId, system, noResourceId, iconResourceId, handleAllWebDataURI, mAutoResolutionAllowed, isInstantAppAvailable (ints), the userHandle id (USER_CURRENT -2 for null).
//! isDefault is not parcelled (a server-side sort key only).

use super::ActivityInfo::ActivityInfo;
use super::ProviderInfo::ProviderInfo;
use super::ServiceInfo::ServiceInfo;
use crate::codecs::android::content::IntentFilter::IntentFilter;
use crate::parcel::*;
use crate::parts::*;

/// UserHandle.USER_CURRENT: what a null userHandle is written as.
pub const USER_CURRENT: i32 = -2;

/// The resolved component: at most one of activityInfo / serviceInfo / providerInfo is set (writeToParcel writes the first non-null).
#[derive(Debug, Clone, Default)]
pub enum Resolved {
    Activity(ActivityInfo),
    Service(ServiceInfo),
    Provider(ProviderInfo),
    #[default]
    None,
}

/// Default = `new ResolveInfo()` (:408): specificIndex -1, targetUserId USER_CURRENT, null userHandle, everything else 0 / false / null.
#[derive(Debug, Clone)]
pub struct ResolveInfo {
    pub info: Resolved,
    /// set only under PackageManager.GET_RESOLVED_FILTER
    pub filter: Option<IntentFilter>,
    pub priority: i32,
    pub preferred_order: i32,
    /// IntentFilter.match's result (MATCH_CATEGORY_* + MATCH_ADJUSTMENT_*)
    pub r#match: i32,
    pub specific_index: i32,
    pub label_res: i32,
    pub non_localized_label: Option<String>,
    pub icon: i32,
    pub resolve_package_name: Option<String>,
    pub target_user_id: i32,
    pub system: bool,
    pub no_resource_id: bool,
    pub icon_resource_id: i32,
    pub handle_all_web_data_uri: bool,
    pub auto_resolution_allowed: bool,
    pub instant_app_available: bool,
    /// userHandle's identifier; USER_CURRENT = null
    pub user_handle: i32,
}

impl Default for ResolveInfo {
    fn default() -> Self {
        ResolveInfo {
            info: Resolved::None,
            filter: None,
            priority: 0,
            preferred_order: 0,
            r#match: 0,
            specific_index: -1,
            label_res: 0,
            non_localized_label: None,
            icon: 0,
            resolve_package_name: None,
            target_user_id: USER_CURRENT,
            system: false,
            no_resource_id: false,
            icon_resource_id: 0,
            handle_all_web_data_uri: false,
            auto_resolution_allowed: false,
            instant_app_available: false,
            user_handle: USER_CURRENT,
        }
    }
}

const TAG_NONE: i32 = 0;
const TAG_ACTIVITY: i32 = 1;
const TAG_SERVICE: i32 = 2;
const TAG_PROVIDER: i32 = 3;

impl Part for ResolveInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        match &self.info {
            Resolved::Activity(a) => {
                w_i32(p, TAG_ACTIVITY)?;
                a.write(p)?;
            }
            Resolved::Service(s) => {
                w_i32(p, TAG_SERVICE)?;
                s.write(p)?;
            }
            Resolved::Provider(d) => {
                w_i32(p, TAG_PROVIDER)?;
                d.write(p)?;
            }
            Resolved::None => w_i32(p, TAG_NONE)?,
        }
        w_typed(p, &self.filter)?;
        w_i32(p, self.priority)?;
        w_i32(p, self.preferred_order)?;
        w_i32(p, self.r#match)?;
        w_i32(p, self.specific_index)?;
        w_i32(p, self.label_res)?;
        w_char_sequence(p, &self.non_localized_label)?;
        w_i32(p, self.icon)?;
        w_str8(p, &self.resolve_package_name)?;
        w_i32(p, self.target_user_id)?;
        w_bool(p, self.system)?;
        w_bool(p, self.no_resource_id)?;
        w_i32(p, self.icon_resource_id)?;
        w_bool(p, self.handle_all_web_data_uri)?;
        w_bool(p, self.auto_resolution_allowed)?;
        w_bool(p, self.instant_app_available)?;
        w_i32(p, self.user_handle)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        // an unknown tag: "Missing ComponentInfo!" and no info, as Java
        let info = match r_i32(p)? {
            TAG_ACTIVITY => Resolved::Activity(ActivityInfo::read(p)?),
            TAG_SERVICE => Resolved::Service(ServiceInfo::read(p)?),
            TAG_PROVIDER => Resolved::Provider(ProviderInfo::read(p)?),
            _ => Resolved::None,
        };
        Ok(ResolveInfo {
            info,
            filter: r_typed(p)?,
            priority: r_i32(p)?,
            preferred_order: r_i32(p)?,
            r#match: r_i32(p)?,
            specific_index: r_i32(p)?,
            label_res: r_i32(p)?,
            non_localized_label: r_char_sequence(p)?,
            icon: r_i32(p)?,
            resolve_package_name: r_str8(p)?,
            target_user_id: r_i32(p)?,
            system: r_bool(p)?,
            no_resource_id: r_bool(p)?,
            icon_resource_id: r_i32(p)?,
            handle_all_web_data_uri: r_bool(p)?,
            auto_resolution_allowed: r_bool(p)?,
            instant_app_available: r_bool(p)?,
            user_handle: r_i32(p)?,
        })
    }
}

crate::codec!(ResolveInfo);
